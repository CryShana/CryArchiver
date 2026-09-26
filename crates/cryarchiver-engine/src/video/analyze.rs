use std::path::Path;

use tokio_util::sync::CancellationToken;

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
const CLEAN_PSNR_DB: f64 = 50.0;
const FILTERS: &str = "idet=intl_thres=1.2,blackdetect=d=1,freezedetect=n=-60dB:d=1,\
	scale='min(1280,iw)':'min(1280,ih)':force_original_aspect_ratio=decrease:force_divisible_by=2,\
	split[a][b];[b]hqdn3d[d];[a][d]psnr[out]";

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Analysis {
	pub scan: ScanType,
	pub noise: NoiseClass,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Segment {
	pub start_s: f64,
	pub length_s: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SegmentStats {
	pub length_s: f64,
	pub neither: u64,
	pub top: u64,
	pub bottom: u64,
	pub tff: u64,
	pub bff: u64,
	pub black_s: f64,
	pub frozen_s: f64,
	pub psnr_y: Option<f64>,
}

pub fn segments(duration_s: Option<f64>) -> Vec<Segment> {
	match duration_s {
		Some(duration) if duration >= WHOLE_FILE_BELOW_S => (0..4)
			.map(|index| Segment {
				start_s: EDGE_SKIP * duration
					+ f64::from(index) * (duration * (1.0 - 2.0 * EDGE_SKIP) - SEGMENT_S) / 3.0,
				length_s: Some(SEGMENT_S),
			})
			.collect(),
		_ => vec![Segment {
			start_s: 0.0,
			length_s: None,
		}],
	}
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
		cmd.arg("-ss")
			.arg(format!("{:.3}", segment.start_s))
			.arg("-t")
			.arg(format!("{length:.3}"));
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

pub fn parse_segment(stderr: &[String], length_s: f64) -> Option<SegmentStats> {
	let mut stats = SegmentStats {
		length_s,
		..SegmentStats::default()
	};
	let mut open_freeze: Option<f64> = None;

	for line in stderr {
		if let Some(rest) = line.split("Repeated Fields:").nth(1) {
			[stats.neither, stats.top, stats.bottom] = counts(rest);
		} else if let Some(rest) = line.split("Single frame detection:").nth(1) {
			let [tff, bff, _] = counts(rest);
			(stats.tff, stats.bff) = (tff, bff);
		} else if let Some(value) = field(line, "black_duration:") {
			stats.black_s += value;
		} else if let Some(value) = field(line, "freeze_start:") {
			open_freeze = Some(value);
		} else if let Some(value) = field(line, "freeze_duration:") {
			stats.frozen_s += value;
			open_freeze = None;
		} else if let Some(value) = field(line, "PSNR y:") {
			stats.psnr_y = Some(value);
		}
	}

	if let Some(start) = open_freeze {
		stats.frozen_s += (length_s - start).max(0.0);
	}
	(stats.neither + stats.top + stats.bottom > 0).then_some(stats)
}

#[allow(clippy::cast_precision_loss)]
pub fn classify(stats: &[SegmentStats], fps: Option<f64>, field_order: Option<&str>) -> Analysis {
	let voting: Vec<&SegmentStats> = stats
		.iter()
		.filter(|segment| {
			segment.black_s < REJECT_SHARE * segment.length_s && segment.frozen_s < REJECT_SHARE * segment.length_s
		})
		.collect();
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

	let scan = match fps {
		_ if frames < 1.0 => ScanType::Progressive,
		Some(fps) if fps > MAX_INTERLACED_FPS => ScanType::Progressive,
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

	let finite: Vec<f64> = voting
		.iter()
		.filter_map(|segment| segment.psnr_y)
		.filter(|db| db.is_finite())
		.collect();
	let noise = if finite.is_empty() || finite.iter().sum::<f64>() / finite.len() as f64 >= CLEAN_PSNR_DB {
		NoiseClass::Clean
	} else {
		NoiseClass::Light
	};
	Analysis { scan, noise }
}

pub async fn analyze(
	tools: &Toolchain,
	source: &Path,
	main: &Stream,
	duration_s: Option<f64>,
	cancel: &CancellationToken,
) -> Result<Analysis, JobError> {
	let segments = segments(duration_s);

	let run = |segment: Option<Segment>| async move {
		let Some(segment) = segment else {
			return Ok(None);
		};

		let cmd = segment_command(&tools.ffmpeg, source, main.index, forced_decoder(main), &segment);
		let out = super::run_ffmpeg(&cmd, |_| {}, cancel).await?;

		Ok::<_, JobError>(parse_segment(&out.stderr_tail, segment_length(&segment, duration_s)))
	};

	let (first, second, third, fourth) = tokio::join!(
		run(segments.first().copied()),
		run(segments.get(1).copied()),
		run(segments.get(2).copied()),
		run(segments.get(3).copied())
	);
	let stats: Vec<SegmentStats> = [first?, second?, third?, fourth?].into_iter().flatten().collect();

	Ok(classify(
		&stats,
		main.r_frame_rate.or(main.fps()),
		main.field_order.as_deref(),
	))
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
			length_s: 3.0,
			neither: repeated[0],
			top: repeated[1],
			bottom: repeated[2],
			tff: single[0],
			bff: single[1],
			black_s: 0.0,
			frozen_s: 0.0,
			psnr_y: Some(45.4),
		}
	}

	#[test]
	fn segments_skip_the_ends_and_short_files_are_whole() {
		let four = segments(Some(100.0));

		assert_eq!(four.len(), 4);
		assert!((four[0].start_s - 5.0).abs() < 1e-9);
		assert!((four[3].start_s + 3.0 - 95.0).abs() < 1e-9);
		assert!(four.iter().all(|segment| segment.length_s == Some(3.0)));
		assert_eq!(
			segments(Some(11.9)),
			vec![Segment {
				start_s: 0.0,
				length_s: None
			}]
		);
		assert_eq!(
			segments(None),
			vec![Segment {
				start_s: 0.0,
				length_s: None
			}]
		);
	}

	#[test]
	fn parses_the_last_idet_block_black_freeze_and_psnr() {
		let lines = [
			"[Parsed_idet_0 @ 000001cfd580c3c0] Repeated Fields: Neither:     0 Top:     0 Bottom:     0",
			"[Parsed_idet_0 @ 000001cfd580c3c0] Single frame detection: TFF:     0 BFF:     0 Progressive:     0 Undetermined:     0",
			"[Parsed_blackdetect_1 @ 0000017c27a45900] black_start:0 black_end:1.2 black_duration:1.2",
			"[Parsed_freezedetect_2 @ 0000017c27a45100] lavfi.freezedetect.freeze_start: 0",
			"[Parsed_freezedetect_2 @ 0000017c27a45100] lavfi.freezedetect.freeze_duration: 1.2",
			"[Parsed_freezedetect_2 @ 0000017c27a45100] lavfi.freezedetect.freeze_end: 1.2",
			"[Parsed_freezedetect_2 @ 0000017c27a45100] lavfi.freezedetect.freeze_start: 2.5",
			"[Parsed_psnr_6 @ 0000017c27a44000] PSNR y:57.184443 u:57.445223 v:57.552183 average:57.286586 min:50.874693 max:inf",
			"[Parsed_idet_0 @ 000001cfd580c3c0] Repeated Fields: Neither:    54 Top:    18 Bottom:    18",
			"[Parsed_idet_0 @ 000001cfd580c3c0] Single frame detection: TFF:    72 BFF:     0 Progressive:    18 Undetermined:     0",
		]
		.map(str::to_owned);

		let parsed = parse_segment(&lines, 3.0).unwrap();

		assert_eq!((parsed.neither, parsed.top, parsed.bottom), (54, 18, 18));
		assert_eq!((parsed.tff, parsed.bff), (72, 0));
		assert!((parsed.black_s - 1.2).abs() < 1e-9);
		assert!((parsed.frozen_s - 1.7).abs() < 1e-9);
		assert_eq!(parsed.psnr_y, Some(57.184_443));
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

		assert_eq!(parse_segment(&empty, 3.0), None);
		assert_eq!(parse_segment(&black, 3.0).unwrap().psnr_y, Some(f64::INFINITY));
	}

	#[test]
	fn classifies_the_corpus_samples() {
		let ntsc = Some(30_000.0 / 1001.0);
		let tff_480i = [stats([90, 0, 0], [90, 0])];
		let bff_576i = [stats([75, 0, 0], [0, 75])];
		let telecined = [stats([54, 18, 18], [72, 0])];
		let concertos = [stats([81, 5, 4], [81, 1]), stats([85, 3, 2], [84, 2])];
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
			classify(&concertos, ntsc, None).scan,
			ScanType::Interlaced { top_first: true }
		);
		assert_eq!(classify(&testsrc, Some(30.0), None).scan, ScanType::Progressive);
		assert_eq!(classify(&hfr, Some(120.0), None).scan, ScanType::Progressive);
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
			}
		);
	}
}
