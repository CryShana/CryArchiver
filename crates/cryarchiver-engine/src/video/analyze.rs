use std::path::Path;

use tokio_util::sync::CancellationToken;

use crate::options::TrimWindow;
use crate::outcome::JobError;
use crate::probe::Stream;
use crate::process::{self, RunError, STALL_TIMEOUT, ToolCommand};
use crate::toolchain::Toolchain;
use crate::video::noise::{self, Combing, Measured, Triplet};

const SEGMENT_S: f64 = 3.0;
const WHOLE_FILE_BELOW_S: f64 = 12.0;
const EDGE_SKIP: f64 = 0.05;
const MAX_INTERLACED_FPS: f64 = 31.0;
const NTSC_FPS: f64 = 30_000.0 / 1001.0;
const TELECINE_REPEATS: f64 = 0.25;
const INTERLACED_SHARE: f64 = 0.6;
const FLAGGED_INTERLACED_SHARE: f64 = 0.05;
const FLAGGED_INTERLACED_VOTES: f64 = 10.0;
const FIELD_ORDER_SHARE: f64 = 0.8;
const REJECT_SHARE: f64 = 0.5;
const INTERLACED_FIELD_ORDERS: &[&str] = &["tt", "bb", "tb", "bt"];
const LOW_DETAIL_SI: f64 = 15.0;
const MIN_COMBED_BLOCKS: u64 = 40;
const COMBED_SHARE: f64 = 0.8;
/// The comb measure is overruled when idet calls most frames progressive: rows that differ by design (scanlines, line
/// art) under uneven motion are the one progressive picture its symmetric sums can mistake for fields.
const COMBED_MAX_PROGRESSIVE_SHARE: f64 = 0.5;
const FILTERS: &str = "idet=intl_thres=1.2,blackdetect=d=1,freezedetect=n=-60dB:d=1,\
	scale='min(1280,iw)':'min(1280,ih)':force_original_aspect_ratio=decrease:force_divisible_by=2,\
	format=yuv420p,siti=print_summary=1[out]";

pub const DEFAULT_SEGMENTS: usize = 4;
/// The noise deviation, in 8-bit code values, from which a source counts as noisy.
pub const NOISY_SIGMA: f64 = 0.29;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanType {
	Progressive,
	Interlaced { top_first: bool },
	Telecined,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoiseClass {
	Clean,
	Light,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Analysis {
	pub scan: ScanType,
	pub noise: NoiseClass,
	/// The temporal noise on flat areas (`noise::level`), in 8-bit code values.
	pub noise_sigma: Option<f64>,
	pub detail: Vec<Detail>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Detail {
	pub start_s: f64,
	pub length_s: f64,
	pub si: Option<f64>,
	pub ti: Option<f64>,
	/// The mean luma on the 8-bit scale.
	pub luma: Option<f64>,
	/// The segment's own noise level (`noise::level`).
	pub noise: Option<f64>,
	pub usable: bool,
	/// Held for most of the segment: its frames repeat.
	pub still: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Segment {
	pub start_s: f64,
	pub length_s: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SegmentStats {
	pub start_s: f64,
	pub length_s: f64,
	pub neither: u64,
	pub top: u64,
	pub bottom: u64,
	pub tff: u64,
	pub bff: u64,
	pub progressive: u64,
	pub black_s: f64,
	pub frozen_s: f64,
	pub si: Option<f64>,
	pub ti: Option<f64>,
	pub noise: Vec<Triplet>,
	/// The mean luma of the frames measured for noise, on the 8-bit scale.
	pub luma: Option<f64>,
	pub combing: Combing,
}

pub fn segments(duration_s: Option<f64>, count: usize) -> Vec<Segment> {
	let count = u32::try_from(count.max(2)).unwrap_or(u32::MAX);

	match duration_s {
		Some(duration) if duration >= WHOLE_FILE_BELOW_S => (0..count)
			.map(|index| Segment {
				start_s: EDGE_SKIP * duration
					+ f64::from(index) * (duration * (1.0 - 2.0 * EDGE_SKIP) - SEGMENT_S) / f64::from(count - 1),
				length_s: Some(SEGMENT_S),
			})
			.collect(),
		_ => vec![Segment {
			start_s: 0.0,
			length_s: None,
		}],
	}
}

pub fn window_segments(duration_s: Option<f64>, count: usize, window: Option<TrimWindow>) -> Vec<Segment> {
	let Some(window) = window else {
		return segments(duration_s, count);
	};

	segments(Some(window.length_s), count)
		.into_iter()
		.map(|segment| Segment {
			start_s: segment.start_s + window.start_s,
			length_s: Some(segment.length_s.unwrap_or(window.length_s)),
		})
		.collect()
}

pub fn segment_command(
	ffmpeg: &Path,
	source: &Path,
	stream_index: u32,
	decoder: Option<&str>,
	segment: &Segment,
) -> ToolCommand {
	let mut cmd = ToolCommand::new(ffmpeg, STALL_TIMEOUT);
	cmd.args(["-hide_banner", "-nostdin", "-noautorotate"]);
	if let Some(length) = segment.length_s {
		cmd.seek(segment.start_s, length);
	}

	if let Some(decoder) = decoder {
		cmd.arg("-c:v").arg(decoder);
	}

	cmd.arg("-i")
		.arg(process::tool_path(source))
		.arg("-filter_complex")
		.arg(format!("[0:{stream_index}]{FILTERS}"))
		.args(["-map", "[out]", "-f", "null", "-"]);

	cmd
}

pub fn parse_segment(stderr: &[String], start_s: f64, length_s: f64) -> Option<SegmentStats> {
	let mut stats = SegmentStats {
		start_s,
		length_s,
		..SegmentStats::default()
	};
	let mut open_freeze: Option<f64> = None;
	let mut spatial = false;
	let mut temporal = false;

	for line in stderr {
		if let Some(rest) = line.split("Repeated Fields:").nth(1) {
			[stats.neither, stats.top, stats.bottom] = counts(rest);
		} else if let Some(rest) = line.split("Single frame detection:").nth(1) {
			[stats.tff, stats.bff, stats.progressive] = counts(rest);
		} else if line.contains("Spatial Information:") {
			spatial = true;
		} else if spatial && let Some(value) = field(line, "Average:") {
			spatial = false;
			if value.is_finite() {
				stats.si = Some(value);
			}
		} else if line.contains("Temporal Information:") {
			temporal = true;
		} else if temporal && let Some(value) = field(line, "Average:") {
			temporal = false;
			if value.is_finite() {
				stats.ti = Some(value);
			}
		} else if let Some(value) = field(line, "black_duration:") {
			stats.black_s += value;
		} else if let Some(value) = field(line, "freeze_start:") {
			open_freeze = Some(value);
		} else if let Some(value) = field(line, "freeze_duration:") {
			stats.frozen_s += value;
			open_freeze = None;
		}
	}

	if let Some(start) = open_freeze {
		stats.frozen_s += (length_s - start).max(0.0);
	}
	(stats.neither + stats.top + stats.bottom > 0).then_some(stats)
}

#[allow(clippy::cast_precision_loss)]
pub fn classify(stats: &[SegmentStats], fps: Option<f64>, field_order: Option<&str>) -> Analysis {
	let voting: Vec<&SegmentStats> = stats.iter().filter(|segment| !blank(segment)).collect();
	let voting = if voting.is_empty() {
		stats.iter().collect()
	} else {
		voting
	};

	let sum = |part: fn(&SegmentStats) -> u64| voting.iter().map(|segment| part(segment)).sum::<u64>() as f64;
	let frames = sum(|segment| segment.neither + segment.top + segment.bottom);
	let (repeated, tff, bff, progressive) = (
		sum(|segment| segment.top + segment.bottom),
		sum(|segment| segment.tff),
		sum(|segment| segment.bff),
		sum(|segment| segment.progressive),
	);

	let combed = voting.iter().fold(Combing::default(), |mut total, segment| {
		total.add(segment.combing);
		total
	});

	let flagged_interlaced = field_order.is_some_and(|order| INTERLACED_FIELD_ORDERS.contains(&order));
	let scan = match fps {
		_ if frames < 1.0 => ScanType::Progressive,
		Some(fps) if fps > MAX_INTERLACED_FPS && !flagged_interlaced => ScanType::Progressive,
		Some(fps) if (fps - NTSC_FPS).abs() < 0.01 && repeated / frames >= TELECINE_REPEATS => ScanType::Telecined,
		_ if (tff + bff) / frames >= INTERLACED_SHARE => {
			let top_share = tff / (tff + bff);
			let top_first = if top_share >= FIELD_ORDER_SHARE {
				true
			} else if top_share <= 1.0 - FIELD_ORDER_SHARE {
				false
			} else {
				!matches!(field_order, Some("bb" | "bt"))
			};
			ScanType::Interlaced { top_first }
		}
		// A mostly still picture leaves most interlaced frames undetermined, so an interlaced container needs only a
		// few frames that agree on one field order. They must outnumber the progressive frames: a slow vertical pan
		// of progressive video draws a steady share of one-sided votes too.
		_ if flagged_interlaced
			&& (tff + bff) / frames >= FLAGGED_INTERLACED_SHARE
			&& tff + bff >= FLAGGED_INTERLACED_VOTES
			&& tff + bff > progressive =>
		{
			let top_share = tff / (tff + bff);
			if top_share >= FIELD_ORDER_SHARE {
				ScanType::Interlaced { top_first: true }
			} else if top_share <= 1.0 - FIELD_ORDER_SHARE {
				ScanType::Interlaced { top_first: false }
			} else {
				ScanType::Progressive
			}
		}
		// Faint interlacing that nothing flags: idet weighs whole frames, where a small moving area drowns in the still
		// rest, while the comb measure counts only the moving blocks.
		_ if progressive / frames < COMBED_MAX_PROGRESSIVE_SHARE
			&& combed.top_first + combed.bottom_first >= MIN_COMBED_BLOCKS
			&& (combed.top_first + combed.bottom_first) as f64
				>= COMBED_SHARE * (combed.top_first + combed.bottom_first + combed.progressive) as f64 =>
		{
			let top_share = combed.top_first as f64 / (combed.top_first + combed.bottom_first) as f64;
			if top_share >= FIELD_ORDER_SHARE {
				ScanType::Interlaced { top_first: true }
			} else if top_share <= 1.0 - FIELD_ORDER_SHARE {
				ScanType::Interlaced { top_first: false }
			} else {
				ScanType::Progressive
			}
		}
		_ => ScanType::Progressive,
	};

	// Frames repeated exactly carry no noise, so a held segment votes, but only once: freeze-frames over a noisy film
	// mustn't outvote it.
	let triplets: Vec<Triplet> = stats
		.iter()
		.filter(|segment| segment.black_s < REJECT_SHARE * segment.length_s)
		.flat_map(|segment| {
			let held = segment.frozen_s >= REJECT_SHARE * segment.length_s;
			segment.noise.iter().copied().take(if held { 1 } else { usize::MAX })
		})
		.collect();
	let noise_sigma = noise::level(&triplets);
	let noise = if noise_sigma.is_some_and(|sigma| sigma >= NOISY_SIGMA) {
		NoiseClass::Light
	} else {
		NoiseClass::Clean
	};

	let detail = stats
		.iter()
		.map(|segment| Detail {
			start_s: segment.start_s,
			length_s: segment.length_s,
			si: segment.si,
			ti: segment.ti,
			luma: segment.luma,
			noise: noise::level(&segment.noise),
			usable: !blank(segment) && segment.si.is_none_or(|si| si >= LOW_DETAIL_SI),
			still: segment.frozen_s >= REJECT_SHARE * segment.length_s,
		})
		.collect();
	Analysis {
		scan,
		noise,
		noise_sigma,
		detail,
	}
}

fn blank(segment: &SegmentStats) -> bool {
	segment.black_s >= REJECT_SHARE * segment.length_s || segment.frozen_s >= REJECT_SHARE * segment.length_s
}

pub async fn analyze(
	tools: &Toolchain,
	source: &Path,
	main: &Stream,
	duration_s: Option<f64>,
	count: usize,
	window: Option<TrimWindow>,
	cancel: &CancellationToken,
) -> Result<Analysis, JobError> {
	let end_s = window.map_or(duration_s, |window| Some(window.start_s + window.length_s));
	let segments = window_segments(duration_s, count, window);

	let run = |segment: Option<Segment>| async move {
		let Some(segment) = segment else {
			return Ok(None);
		};

		let cmd = segment_command(&tools.ffmpeg, source, main.index, forced_decoder(main), &segment);
		let (out, pixels) = tokio::join!(
			super::run_ffmpeg(&cmd, |_| {}, cancel),
			segment_pixels(tools, source, main, &segment, cancel)
		);
		let (out, pixels) = (out?, pixels?);

		Ok::<_, JobError>(
			parse_segment(&out.stderr_tail, segment.start_s, segment_length(&segment, end_s)).map(|stats| {
				SegmentStats {
					noise: pixels.triplets,
					luma: pixels.luma,
					combing: pixels.combing,
					..stats
				}
			}),
		)
	};

	let mut stats = Vec::new();
	for chunk in segments.chunks(4) {
		let (first, second, third, fourth) = tokio::join!(
			run(chunk.first().copied()),
			run(chunk.get(1).copied()),
			run(chunk.get(2).copied()),
			run(chunk.get(3).copied())
		);
		stats.extend([first?, second?, third?, fourth?].into_iter().flatten());
	}

	Ok(classify(&stats, main.fps(), main.field_order.as_deref()))
}

/// A source whose luma can't be decoded to raw frames (an RGB stream, say) is left unmeasured rather than failed.
async fn segment_pixels(
	tools: &Toolchain,
	source: &Path,
	main: &Stream,
	segment: &Segment,
	cancel: &CancellationToken,
) -> Result<Measured, JobError> {
	let cmd = noise::command(&tools.ffmpeg, source, main, segment);
	let mut reader = noise::Reader::default();

	let out = match process::run_streamed(&cmd, |chunk| reader.push(chunk), cancel).await {
		Ok(out) => out,
		Err(RunError::CaptureLimit(_) | RunError::Crashed { .. }) => return Ok(Measured::default()),
		Err(err) => return Err(err.into()),
	};
	Ok(if out.status.success() {
		reader.finish()
	} else {
		Measured::default()
	})
}

pub fn forced_decoder(stream: &Stream) -> Option<&'static str> {
	if stream.tag("alpha_mode") != Some("1") {
		return None;
	}

	match stream.codec_name.as_str() {
		"vp8" => Some("libvpx"),
		"vp9" => Some("libvpx-vp9"),
		_ => None,
	}
}

fn segment_length(segment: &Segment, duration_s: Option<f64>) -> f64 {
	match (segment.length_s, duration_s) {
		(Some(length), Some(duration)) => length.min(duration - segment.start_s).max(0.0),
		(Some(length), None) => length,
		(None, duration) => duration.unwrap_or(SEGMENT_S),
	}
}

fn counts(text: &str) -> [u64; 3] {
	let mut numbers = text
		.split(':')
		.skip(1)
		.filter_map(|part| part.split_whitespace().next()?.parse().ok());
	[
		numbers.next().unwrap_or(0),
		numbers.next().unwrap_or(0),
		numbers.next().unwrap_or(0),
	]
}

fn field(line: &str, key: &str) -> Option<f64> {
	line.split(key).nth(1)?.split_whitespace().next()?.parse().ok()
}

#[cfg(test)]
mod tests {
	use super::*;

	fn stats(repeated: [u64; 3], single: [u64; 2]) -> SegmentStats {
		SegmentStats {
			start_s: 0.0,
			length_s: 3.0,
			neither: repeated[0],
			top: repeated[1],
			bottom: repeated[2],
			tff: single[0],
			bff: single[1],
			progressive: 0,
			black_s: 0.0,
			frozen_s: 0.0,
			si: Some(60.0),
			ti: None,
			noise: Vec::new(),
			luma: None,
			combing: Combing::default(),
		}
	}

	#[test]
	fn segments_skip_the_ends_and_short_files_are_whole() {
		let four = segments(Some(100.0), 4);

		assert_eq!(four.len(), 4);
		assert!((four[0].start_s - 5.0).abs() < 1e-9);
		assert!((four[3].start_s + 3.0 - 95.0).abs() < 1e-9);
		assert!(four.iter().all(|segment| segment.length_s == Some(3.0)));
		assert_eq!(
			segments(Some(11.9), 4),
			vec![Segment {
				start_s: 0.0,
				length_s: None
			}]
		);
		assert_eq!(
			segments(None, 4),
			vec![Segment {
				start_s: 0.0,
				length_s: None
			}]
		);

		let eight = segments(Some(100.0), 8);
		assert_eq!(eight.len(), 8);
		assert!((eight[0].start_s - 5.0).abs() < 1e-9);
		assert!((eight[7].start_s + 3.0 - 95.0).abs() < 1e-9);
	}

	#[test]
	fn a_trim_is_analysed_only_inside_its_window() {
		let from_zero = window_segments(
			Some(10.0),
			4,
			Some(TrimWindow {
				start_s: 0.0,
				length_s: 8.0,
			}),
		);
		let later = window_segments(
			Some(100.0),
			4,
			Some(TrimWindow {
				start_s: 40.0,
				length_s: 20.0,
			}),
		);
		let whole = segment_command(Path::new("ffmpeg"), Path::new("in.mkv"), 0, None, &from_zero[0]);

		assert_eq!(
			from_zero,
			vec![Segment {
				start_s: 0.0,
				length_s: Some(8.0)
			}]
		);
		assert!(crate::video::joined(&whole).contains("-ss 0.000 -t 8.000 -i "));
		assert_eq!(later.len(), 4);
		assert!((later[0].start_s - 41.0).abs() < 1e-9, "{later:?}");
		assert!((later[3].start_s + 3.0 - 59.0).abs() < 1e-9, "{later:?}");
		assert_eq!(
			window_segments(Some(10.0), 4, None),
			vec![Segment {
				start_s: 0.0,
				length_s: None
			}]
		);
	}

	#[test]
	fn parses_the_last_idet_block_black_freeze_psnr_and_detail() {
		let lines = [
			"[Parsed_idet_0 @ 000001cfd580c3c0] Repeated Fields: Neither:     0 Top:     0 Bottom:     0",
			"[Parsed_idet_0 @ 000001cfd580c3c0] Single frame detection: TFF:     0 BFF:     0 Progressive:     0 Undetermined:     0",
			"[Parsed_siti_6 @ 00000297d99eaf40] SITI Summary:",
			"Total frames: 0",
			"",
			"Spatial Information:",
			"Average: nan",
			"[Parsed_blackdetect_1 @ 0000017c27a45900] black_start:0 black_end:1.2 black_duration:1.2",
			"[Parsed_freezedetect_2 @ 0000017c27a45100] lavfi.freezedetect.freeze_start: 0",
			"[Parsed_freezedetect_2 @ 0000017c27a45100] lavfi.freezedetect.freeze_duration: 1.2",
			"[Parsed_freezedetect_2 @ 0000017c27a45100] lavfi.freezedetect.freeze_end: 1.2",
			"[Parsed_freezedetect_2 @ 0000017c27a45100] lavfi.freezedetect.freeze_start: 2.5",
			"[Parsed_psnr_9 @ 0000017c27a44000] PSNR y:57.184443 u:57.445223 v:57.552183 average:57.286586 min:50.874693 max:inf",
			"[psnr@native @ 0000017c27a44100] PSNR y:42.809130 u:46.427337 v:46.440324 average:43.805400 min:43.716648 max:56.904948",
			"[Parsed_siti_6 @ 00000297db9eb200] SITI Summary:",
			"Total frames: 90",
			"",
			"Spatial Information:",
			"Average: 102.528107",
			"Max: 103.951012",
			"",
			"Temporal Information:",
			"Average: 13.375446",
			"[Parsed_idet_0 @ 000001cfd580c3c0] Repeated Fields: Neither:    54 Top:    18 Bottom:    18",
			"[Parsed_idet_0 @ 000001cfd580c3c0] Single frame detection: TFF:    72 BFF:     0 Progressive:    18 Undetermined:     0",
		]
		.map(str::to_owned);

		let parsed = parse_segment(&lines, 12.5, 3.0).unwrap();

		assert!((parsed.start_s - 12.5).abs() < 1e-9);
		assert_eq!((parsed.neither, parsed.top, parsed.bottom), (54, 18, 18));
		assert_eq!((parsed.tff, parsed.bff, parsed.progressive), (72, 0, 18));
		assert!((parsed.black_s - 1.2).abs() < 1e-9);
		assert!((parsed.frozen_s - 1.7).abs() < 1e-9);
		assert_eq!(parsed.si, Some(102.528_107));
		assert_eq!(parsed.ti, Some(13.375_446));
		assert!(parsed.noise.is_empty());
	}

	#[test]
	fn temporal_information_is_read_after_the_spatial_summary() {
		let log: Vec<String> = [
			"[Parsed_idet_0 @ 0x1] Repeated Fields: Neither:    75 Top:     0 Bottom:     0",
			"[Parsed_siti_9 @ 0x2] SITI Summary:",
			"[Parsed_siti_9 @ 0x2] Spatial Information:",
			"[Parsed_siti_9 @ 0x2] Average: 38.9",
			"[Parsed_siti_9 @ 0x2] Temporal Information:",
			"[Parsed_siti_9 @ 0x2] Average: 12.4",
		]
		.map(str::to_owned)
		.to_vec();

		let stats = parse_segment(&log, 0.0, 3.0).unwrap();

		assert_eq!(stats.si, Some(38.9));
		assert_eq!(stats.ti, Some(12.4));
	}

	fn combed(top_first: u64, bottom_first: u64, progressive: u64) -> SegmentStats {
		SegmentStats {
			combing: Combing {
				top_first,
				bottom_first,
				progressive,
			},
			..stats([75, 0, 0], [0, 0])
		}
	}

	#[test]
	fn moving_blocks_that_comb_make_an_unflagged_source_interlaced() {
		let top = [combed(91, 0, 4), combed(0, 0, 0)];
		let bottom = [combed(1, 86, 7), combed(2, 30, 0)];

		for order in [None, Some("progressive"), Some("unknown")] {
			assert_eq!(
				classify(&top, Some(25.0), order).scan,
				ScanType::Interlaced { top_first: true },
				"{order:?}"
			);
			assert_eq!(
				classify(&bottom, Some(25.0), order).scan,
				ScanType::Interlaced { top_first: false },
				"{order:?}"
			);
		}
	}

	#[test]
	fn few_mixed_or_split_combed_blocks_and_high_frame_rates_stay_progressive() {
		let telecine_like = [combed(75, 0, 96)];
		let progressive_pans = [combed(2, 15, 219), combed(9, 0, 341), combed(8, 2, 110)];
		let few = [combed(30, 0, 0)];
		let split = [combed(60, 50, 2)];
		let fast = [combed(37, 13, 6), combed(300, 0, 5)];
		let mut black = combed(300, 0, 0);
		black.black_s = 2.9;
		let called_progressive = [SegmentStats {
			progressive: 40,
			..combed(95, 0, 3)
		}];
		assert_eq!(
			classify(&called_progressive, Some(25.0), None).scan,
			ScanType::Progressive
		);

		for stats in [
			&telecine_like[..],
			&progressive_pans,
			&few,
			&split,
			&[black, combed(0, 0, 40)],
		] {
			assert_eq!(
				classify(stats, Some(25.0), None).scan,
				ScanType::Progressive,
				"{stats:?}"
			);
		}
		assert_eq!(classify(&fast, Some(50.0), None).scan, ScanType::Progressive);
		assert_eq!(
			classify(&fast, Some(120.0), Some("progressive")).scan,
			ScanType::Progressive
		);
	}

	#[test]
	fn each_segment_keeps_its_motion_brightness_noise_and_stillness() {
		let dark = SegmentStats {
			ti: Some(21.0),
			luma: Some(18.5),
			noise: vec![
				Triplet {
					bands: [Some(0.6), Some(1.4), None, None],
				};
				3
			],
			black_s: 2.9,
			..stats([90, 0, 0], [0, 0])
		};
		let held = SegmentStats {
			frozen_s: 3.0,
			..stats([90, 0, 0], [0, 0])
		};

		let analysis = classify(&[dark, held], Some(25.0), None);

		let [dark, held] = [analysis.detail[0], analysis.detail[1]];
		assert_eq!((dark.ti, dark.luma, dark.noise), (Some(21.0), Some(18.5), Some(1.4)));
		assert!(!dark.usable && !dark.still, "{dark:?}");
		assert_eq!((held.ti, held.luma, held.noise), (None, None, None));
		assert!(!held.usable && held.still, "{held:?}");
	}

	#[test]
	fn empty_segments_and_identical_frames() {
		let empty = [
			"[Parsed_idet_0 @ 0000] Repeated Fields: Neither:     0 Top:     0 Bottom:     0".to_owned(),
			"[Parsed_idet_0 @ 0000] Single frame detection: TFF:     0 BFF:     0 Progressive:     0 Undetermined:     0"
				.to_owned(),
		];
		let black = [
			"[Parsed_idet_0 @ 0000] Repeated Fields: Neither:    90 Top:     0 Bottom:     0".to_owned(),
			"[Parsed_psnr_6 @ 0000] PSNR y:inf u:inf v:inf average:inf min:inf max:inf".to_owned(),
		];

		assert_eq!(parse_segment(&empty, 0.0, 3.0), None);
		assert_eq!(parse_segment(&black, 0.0, 3.0).unwrap().neither, 90);
	}

	#[test]
	fn classifies_measured_field_statistics() {
		let ntsc = Some(30_000.0 / 1001.0);
		let tff_480i = [stats([90, 0, 0], [90, 0])];
		let bff_576i = [stats([75, 0, 0], [0, 75])];
		let telecined = [stats([54, 18, 18], [72, 0])];
		let broadcast_1080i = [stats([81, 5, 4], [81, 1]), stats([85, 3, 2], [84, 2])];
		let testsrc = [stats([91, 0, 0], [0, 0])];
		let hfr = [stats([240, 0, 0], [24, 0])];

		assert_eq!(
			classify(&tff_480i, ntsc, Some("tb")).scan,
			ScanType::Interlaced { top_first: true }
		);
		assert_eq!(
			classify(&bff_576i, Some(25.0), None).scan,
			ScanType::Interlaced { top_first: false }
		);
		assert_eq!(classify(&telecined, ntsc, None).scan, ScanType::Telecined);
		assert_eq!(
			classify(&broadcast_1080i, ntsc, None).scan,
			ScanType::Interlaced { top_first: true }
		);
		assert_eq!(classify(&testsrc, Some(30.0), None).scan, ScanType::Progressive);
		assert_eq!(classify(&hfr, Some(120.0), None).scan, ScanType::Progressive);
	}

	#[test]
	fn a_flagged_interlaced_container_overrides_the_field_rate_gate() {
		let tff = [stats([90, 0, 0], [90, 0])];

		assert_eq!(
			classify(&tff, Some(50.0), Some("tt")).scan,
			ScanType::Interlaced { top_first: true }
		);
		assert_eq!(
			classify(&tff, Some(50.0), Some("progressive")).scan,
			ScanType::Progressive
		);
	}

	#[test]
	fn ambiguous_field_order_falls_back_to_the_container() {
		let mixed = [stats([90, 0, 0], [50, 40])];

		assert_eq!(
			classify(&mixed, Some(25.0), Some("bb")).scan,
			ScanType::Interlaced { top_first: false }
		);
		assert_eq!(
			classify(&mixed, Some(25.0), None).scan,
			ScanType::Interlaced { top_first: true }
		);
	}

	#[test]
	fn faint_field_evidence_in_an_interlaced_container_is_interlaced() {
		let still_home_video = |first: [u64; 2], second: [u64; 2]| {
			[
				stats([75, 0, 0], first),
				stats([75, 0, 0], second),
				stats([75, 0, 0], [0, 0]),
				stats([75, 0, 0], [0, 0]),
			]
		};
		let top = still_home_video([3, 0], [49, 0]);
		let bottom = still_home_video([0, 3], [0, 49]);

		for order in ["tt", "tb"] {
			assert_eq!(
				classify(&top, Some(25.0), Some(order)).scan,
				ScanType::Interlaced { top_first: true },
				"{order}"
			);
		}

		for order in ["bb", "bt"] {
			assert_eq!(
				classify(&bottom, Some(25.0), Some(order)).scan,
				ScanType::Interlaced { top_first: false },
				"{order}"
			);
		}

		assert_eq!(
			classify(&top, Some(25.0), Some("bb")).scan,
			ScanType::Interlaced { top_first: true }
		);
	}

	#[test]
	fn faint_or_split_field_evidence_alone_is_progressive() {
		let faint = [stats([75, 0, 0], [30, 0]), stats([75, 0, 0], [0, 0])];
		let split = [stats([75, 0, 0], [20, 12]), stats([75, 0, 0], [0, 0])];
		let still = stats([75, 0, 0], [0, 0]);
		let stray = [stats([75, 0, 0], [3, 0]), still.clone(), still];

		for order in [None, Some("progressive"), Some("unknown")] {
			assert_eq!(
				classify(&faint, Some(25.0), order).scan,
				ScanType::Progressive,
				"{order:?}"
			);
		}

		assert_eq!(classify(&split, Some(25.0), Some("tt")).scan, ScanType::Progressive);
		assert_eq!(classify(&stray, Some(25.0), Some("tt")).scan, ScanType::Progressive);
	}

	#[test]
	fn faint_field_votes_outnumbered_by_progressive_ones_stay_progressive() {
		let panned = |single: [u64; 2], progressive: u64| SegmentStats {
			progressive,
			..stats([100, 0, 0], single)
		};
		let down = [panned([0, 42], 58)];
		let up = [panned([18, 0], 68)];
		let tied = [panned([0, 50], 50)];
		let short = [SegmentStats {
			progressive: 1,
			..stats([25, 0, 0], [3, 0])
		}];

		for order in ["tt", "bb", "tb", "bt"] {
			assert_eq!(classify(&down, Some(25.0), Some(order)).scan, ScanType::Progressive);
			assert_eq!(classify(&up, Some(25.0), Some(order)).scan, ScanType::Progressive);
			assert_eq!(classify(&tied, Some(25.0), Some(order)).scan, ScanType::Progressive);
			assert_eq!(classify(&short, Some(25.0), Some(order)).scan, ScanType::Progressive);
		}

		assert_eq!(
			classify(&[panned([0, 32], 19)], Some(25.0), Some("bb")).scan,
			ScanType::Interlaced { top_first: false }
		);
		assert_eq!(
			classify(&[panned([10, 0], 9)], Some(25.0), Some("tt")).scan,
			ScanType::Interlaced { top_first: true }
		);
	}

	#[test]
	fn black_and_frozen_segments_do_not_vote() {
		let mut black = stats([90, 0, 0], [90, 0]);
		black.black_s = 2.9;
		let mut frozen = stats([90, 0, 0], [90, 0]);
		frozen.frozen_s = 1.5;
		let progressive = stats([90, 0, 0], [0, 0]);

		assert_eq!(
			classify(&[black, frozen, progressive], Some(25.0), None).scan,
			ScanType::Progressive
		);
	}

	fn measured(bands: [Option<f64>; 4]) -> SegmentStats {
		SegmentStats {
			noise: vec![Triplet { bands }; 3],
			..stats([90, 0, 0], [0, 0])
		}
	}

	#[test]
	fn the_noise_comes_from_the_triplets_of_every_segment_but_black_ones() {
		let camera = measured([Some(0.9), Some(1.2), None, None]);
		let animation = measured([Some(0.1), Some(0.25), Some(0.15), None]);
		let flickering = measured([Some(0.5), Some(0.5), None, None]);
		let mut black = measured([Some(5.0); 4]);
		black.black_s = 2.9;
		let mut held = measured([Some(0.0); 4]);
		held.frozen_s = 3.0;

		let grainy = classify(&[camera.clone(), black.clone()], Some(25.0), None);
		let clean = classify(&[animation, black], Some(25.0), None);
		let drawings_held = classify(
			&[flickering, held.clone(), held.clone(), held.clone()],
			Some(25.0),
			None,
		);
		let freeze_frames = classify(&[camera.clone(), camera, held.clone(), held], Some(25.0), None);
		let unmeasured = classify(&[stats([90, 0, 0], [0, 0])], Some(25.0), None);

		assert_eq!((grainy.noise, grainy.noise_sigma), (NoiseClass::Light, Some(1.2)));
		assert_eq!((clean.noise, clean.noise_sigma), (NoiseClass::Clean, Some(0.25)));
		assert_eq!(drawings_held.noise, NoiseClass::Clean, "{drawings_held:?}");
		assert_eq!(
			(freeze_frames.noise, freeze_frames.noise_sigma),
			(NoiseClass::Light, Some(1.2))
		);
		assert_eq!((unmeasured.noise, unmeasured.noise_sigma), (NoiseClass::Clean, None));
		assert_eq!(
			classify(&[], Some(25.0), None),
			Analysis {
				scan: ScanType::Progressive,
				noise: NoiseClass::Clean,
				noise_sigma: None,
				detail: Vec::new(),
			}
		);
	}

	#[test]
	fn noise_just_under_the_threshold_is_clean() {
		let faint = classify(
			&[measured([None, Some(NOISY_SIGMA - 0.01), None, None])],
			Some(25.0),
			None,
		);
		let noisy = classify(&[measured([None, Some(NOISY_SIGMA), None, None])], Some(25.0), None);

		assert_eq!(faint.noise, NoiseClass::Clean);
		assert_eq!(noisy.noise, NoiseClass::Light);
	}

	#[test]
	fn blank_and_low_detail_segments_are_not_usable() {
		let detailed = SegmentStats {
			start_s: 10.0,
			si: Some(80.0),
			..stats([90, 0, 0], [0, 0])
		};
		let flat = SegmentStats {
			start_s: 40.0,
			si: Some(4.0),
			..stats([90, 0, 0], [0, 0])
		};
		let black = SegmentStats {
			start_s: 70.0,
			black_s: 2.9,
			..stats([90, 0, 0], [0, 0])
		};
		let unmeasured = SegmentStats {
			start_s: 100.0,
			si: None,
			..stats([90, 0, 0], [0, 0])
		};

		let analysis = classify(&[detailed, flat, black, unmeasured], Some(25.0), None);

		assert_eq!(
			analysis
				.detail
				.iter()
				.map(|detail| (detail.start_s, detail.usable))
				.collect::<Vec<_>>(),
			[(10.0, true), (40.0, false), (70.0, false), (100.0, true)]
		);
		assert_eq!(analysis.detail[0].si, Some(80.0));
	}
}
