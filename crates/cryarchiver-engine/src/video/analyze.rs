use std::path::Path;

use tokio_util::sync::CancellationToken;

use crate::options::TrimWindow;
use crate::outcome::JobError;
use crate::probe::Stream;
use crate::process::{self, STALL_TIMEOUT, ToolCommand};
use crate::toolchain::Toolchain;

const SEGMENT_S: f64 = 3.0;
const WHOLE_FILE_BELOW_S: f64 = 12.0;
const EDGE_SKIP: f64 = 0.05;
const MAX_INTERLACED_FPS: f64 = 31.0;
const NTSC_FPS: f64 = 30_000.0 / 1001.0;
const TELECINE_REPEATS: f64 = 0.25;
const INTERLACED_SHARE: f64 = 0.6;
const FIELD_ORDER_SHARE: f64 = 0.8;
const REJECT_SHARE: f64 = 0.5;
const INTERLACED_FIELD_ORDERS: &[&str] = &["tt", "bb", "tb", "bt"];
const CLEAN_PSNR_DB: f64 = 50.0;
const LOW_DETAIL_SI: f64 = 15.0;
const FILTERS: &str = "idet=intl_thres=1.2,blackdetect=d=1,freezedetect=n=-60dB:d=1,split[n][m];\
	[n]split[na][nb];[nb]hqdn3d[nd];[na][nd]psnr@native,nullsink;\
	[m]scale='min(1280,iw)':'min(1280,ih)':force_original_aspect_ratio=decrease:force_divisible_by=2,\
	split=3[a][b][s];[s]format=yuv420p,siti=print_summary=1,nullsink;[b]hqdn3d[d];[a][d]psnr[out]";
const NATIVE_PSNR_LOG: &str = "[psnr@native @";

pub const DEFAULT_SEGMENTS: usize = 4;

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
	pub noise_db: Option<f64>,
	pub detail: Vec<Detail>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Detail {
	pub start_s: f64,
	pub length_s: f64,
	pub si: Option<f64>,
	pub usable: bool,
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
	pub black_s: f64,
	pub frozen_s: f64,
	pub psnr_y: Option<f64>,
	pub native_psnr_y: Option<f64>,
	pub si: Option<f64>,
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

	for line in stderr {
		if let Some(rest) = line.split("Repeated Fields:").nth(1) {
			[stats.neither, stats.top, stats.bottom] = counts(rest);
		} else if let Some(rest) = line.split("Single frame detection:").nth(1) {
			let [tff, bff, _] = counts(rest);
			(stats.tff, stats.bff) = (tff, bff);
		} else if line.contains("Spatial Information:") {
			spatial = true;
		} else if spatial && let Some(value) = field(line, "Average:") {
			spatial = false;
			if value.is_finite() {
				stats.si = Some(value);
			}
		} else if let Some(value) = field(line, "black_duration:") {
			stats.black_s += value;
		} else if let Some(value) = field(line, "freeze_start:") {
			open_freeze = Some(value);
		} else if let Some(value) = field(line, "freeze_duration:") {
			stats.frozen_s += value;
			open_freeze = None;
		} else if let Some(value) = field(line, "PSNR y:") {
			if line.contains(NATIVE_PSNR_LOG) {
				stats.native_psnr_y = Some(value);
			} else {
				stats.psnr_y = Some(value);
			}
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
	let (repeated, tff, bff) = (
		sum(|segment| segment.top + segment.bottom),
		sum(|segment| segment.tff),
		sum(|segment| segment.bff),
	);

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
		_ => ScanType::Progressive,
	};

	let scaled_db = finite_mean(voting.iter().map(|segment| segment.psnr_y));
	let noise_db = finite_mean(voting.iter().map(|segment| segment.native_psnr_y));
	let noise = if scaled_db.is_none_or(|db| db >= CLEAN_PSNR_DB) {
		NoiseClass::Clean
	} else {
		NoiseClass::Light
	};

	let detail = stats
		.iter()
		.map(|segment| Detail {
			start_s: segment.start_s,
			length_s: segment.length_s,
			si: segment.si,
			usable: !blank(segment) && segment.si.is_none_or(|si| si >= LOW_DETAIL_SI),
		})
		.collect();
	Analysis {
		scan,
		noise,
		noise_db,
		detail,
	}
}

#[allow(clippy::cast_precision_loss)]
fn finite_mean(values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
	let finite: Vec<f64> = values.flatten().filter(|db| db.is_finite()).collect();
	(!finite.is_empty()).then(|| finite.iter().sum::<f64>() / finite.len() as f64)
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
		let out = super::run_ffmpeg(&cmd, |_| {}, cancel).await?;

		Ok::<_, JobError>(parse_segment(
			&out.stderr_tail,
			segment.start_s,
			segment_length(&segment, end_s),
		))
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
	use crate::video::plan::grain_level;

	fn stats(repeated: [u64; 3], single: [u64; 2]) -> SegmentStats {
		SegmentStats {
			start_s: 0.0,
			length_s: 3.0,
			neither: repeated[0],
			top: repeated[1],
			bottom: repeated[2],
			tff: single[0],
			bff: single[1],
			black_s: 0.0,
			frozen_s: 0.0,
			psnr_y: Some(45.4),
			native_psnr_y: Some(45.4),
			si: Some(60.0),
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
		assert_eq!((parsed.tff, parsed.bff), (72, 0));
		assert!((parsed.black_s - 1.2).abs() < 1e-9);
		assert!((parsed.frozen_s - 1.7).abs() < 1e-9);
		assert_eq!(parsed.psnr_y, Some(57.184_443));
		assert_eq!(parsed.native_psnr_y, Some(42.809_13));
		assert_eq!(parsed.si, Some(102.528_107));
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
		assert_eq!(parse_segment(&black, 0.0, 3.0).unwrap().psnr_y, Some(f64::INFINITY));
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

	#[test]
	fn noise_class_comes_from_the_luma_psnr() {
		let mut camera = stats([90, 0, 0], [0, 0]);
		camera.psnr_y = Some(45.4);
		let mut animation = stats([90, 0, 0], [0, 0]);
		animation.psnr_y = Some(57.2);
		let mut still = stats([90, 0, 0], [0, 0]);
		still.psnr_y = Some(f64::INFINITY);

		assert_eq!(classify(&[camera], Some(25.0), None).noise, NoiseClass::Light);
		assert_eq!(
			classify(&[animation, still.clone()], Some(25.0), None).noise,
			NoiseClass::Clean
		);
		assert_eq!(classify(&[still], Some(25.0), None).noise, NoiseClass::Clean);
		assert_eq!(
			classify(&[], Some(25.0), None),
			Analysis {
				scan: ScanType::Progressive,
				noise: NoiseClass::Clean,
				noise_db: None,
				detail: Vec::new(),
			}
		);
	}

	#[test]
	fn the_noise_level_is_the_average_finite_luma_psnr() {
		let psnr = |db: f64| SegmentStats {
			psnr_y: Some(db),
			native_psnr_y: Some(db),
			..stats([90, 0, 0], [0, 0])
		};
		let mut black = psnr(30.0);
		black.black_s = 2.9;

		let grainy = classify(&[psnr(42.0), psnr(44.0), psnr(f64::INFINITY), black], Some(25.0), None);
		let still = classify(&[psnr(f64::INFINITY)], Some(25.0), None);
		let unmeasured = classify(
			&[SegmentStats {
				psnr_y: None,
				native_psnr_y: None,
				..stats([90, 0, 0], [0, 0])
			}],
			Some(25.0),
			None,
		);

		assert_eq!(grainy.noise_db, Some(43.0));
		assert_eq!(grainy.noise, NoiseClass::Light);
		assert_eq!(still.noise_db, None);
		assert_eq!(unmeasured.noise_db, None);
	}

	#[test]
	fn the_noise_level_is_the_native_measure_and_the_class_the_scaled_one() {
		let segment = |scaled: f64, native: f64| SegmentStats {
			psnr_y: Some(scaled),
			native_psnr_y: Some(native),
			..stats([90, 0, 0], [0, 0])
		};

		let grainy_4k = classify(&[segment(45.8, 42.8), segment(45.6, 42.6)], Some(60.0), None);
		let sharp_4k = classify(&[segment(51.0, 46.0)], Some(60.0), None);

		assert_eq!(grainy_4k.noise, NoiseClass::Light);
		assert!(
			grainy_4k.noise_db.is_some_and(|db| (db - 42.7).abs() < 1e-9),
			"{grainy_4k:?}"
		);
		assert_eq!(grain_level(grainy_4k.noise_db), Some(20));
		assert_eq!(sharp_4k.noise, NoiseClass::Clean);
		assert_eq!(sharp_4k.noise_db, Some(46.0));
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
