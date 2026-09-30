use indicatif::DecimalBytes;

use crate::format::OutputFormat;
use crate::images::plan::ImageRoute;

const DURATION_SLACK_S: f64 = 0.05;
const SHORTEST_TRIM_S: f64 = 0.1;
const SHORTEST_SPAN_S: f64 = 0.001;
const TIME_EPSILON_S: f64 = 1e-9;
const SIXTY: f64 = 60.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Trim {
	pub start_s: f64,
	pub end_s: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrimWindow {
	pub start_s: f64,
	pub length_s: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubtitlePick {
	Default,
	Track(u32),
	Language(String),
}

impl SubtitlePick {
	pub fn parse(text: &str) -> Self {
		let text = text.trim();
		if text.is_empty() {
			return Self::Default;
		}

		match text.parse() {
			Ok(number) if text.bytes().all(|byte| byte.is_ascii_digit()) => Self::Track(number),
			_ => Self::Language(text.to_ascii_lowercase()),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BurnSubs {
	pub pick: SubtitlePick,
	pub size_percent: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
pub enum VideoEncoderChoice {
	SvtAv1,
	Aom,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Advanced {
	pub crf: Option<f64>,
	pub video_encoder: Option<VideoEncoderChoice>,
	pub image_encoder: Option<ImageRoute>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct JobOptions {
	pub trim: Option<Trim>,
	pub format: Option<OutputFormat>,
	pub strip: bool,
	pub merge_audio: bool,
	pub burn_subs: Option<BurnSubs>,
	pub advanced: Advanced,
}

impl JobOptions {
	/// `merge_possible` is whether the file actually has enough audio tracks for `--merge-audio` to merge anything;
	/// a single-track file is not forced by the flag alone.
	pub fn forced(&self, merge_possible: bool) -> bool {
		self.trim.is_some()
			|| self.format.is_some()
			|| self.strip
			|| (self.merge_audio && merge_possible)
			|| self.burn_subs.is_some()
	}
}

impl TrimWindow {
	/// The same range on the source's own timestamps, which begin at its start time rather than at 0.
	#[must_use]
	pub fn on_source(self, start_time: Option<f64>) -> Self {
		Self {
			start_s: self.start_s + start_time.unwrap_or(0.0),
			..self
		}
	}

	/// A span's part inside the window, on the window's timeline; `None` when nothing of it is left.
	pub fn clip(self, start_s: f64, end_s: f64) -> Option<(f64, f64)> {
		let start = start_s.max(self.start_s) - self.start_s;
		let end = end_s.min(self.start_s + self.length_s) - self.start_s;
		(end - start > SHORTEST_SPAN_S).then_some((start, end))
	}

	pub fn shortfall(self, decoded_s: f64) -> Option<String> {
		(self.length_s - decoded_s > (self.length_s * 0.02).max(1.0)).then(|| {
			format!(
				"the trimmed range decodes to {decoded_s:.1} s but should last {:.1} s",
				self.length_s
			)
		})
	}
}

impl Trim {
	pub fn window(self, duration_s: Option<f64>) -> Result<TrimWindow, String> {
		self.window_of_at_least(duration_s, SHORTEST_TRIM_S)
	}

	/// Like `window`, and also at least one frame long, so the trim holds a frame at the source's rate.
	pub fn video_window(self, duration_s: Option<f64>, fps: Option<f64>) -> Result<TrimWindow, String> {
		let frame_s = fps.filter(|fps| *fps > 0.0).map_or(0.0, |fps| 1.0 / fps);
		self.window_of_at_least(duration_s, frame_s.max(SHORTEST_TRIM_S))
	}

	fn window_of_at_least(self, duration_s: Option<f64>, shortest_s: f64) -> Result<TrimWindow, String> {
		if let Some(end_s) = self.end_s
			&& end_s <= self.start_s
		{
			return Err(format!(
				"--to ({end_s:.3} s) must be later than --ss ({:.3} s)",
				self.start_s
			));
		}

		let Some(duration_s) = duration_s else {
			let end_s = self
				.end_s
				.ok_or_else(|| "the file's duration is unknown, so --to is needed".to_owned())?;
			return self.lasting(end_s - self.start_s, shortest_s);
		};

		if self.start_s >= duration_s {
			return Err(format!(
				"--ss ({:.3} s) is past the end of the file ({duration_s:.3} s)",
				self.start_s
			));
		}

		let end_s = self.end_s.unwrap_or(duration_s);
		if end_s > duration_s + DURATION_SLACK_S {
			return Err(format!(
				"--to ({end_s:.3} s) is past the end of the file ({duration_s:.3} s)"
			));
		}
		self.lasting(end_s.min(duration_s) - self.start_s, shortest_s)
	}

	fn lasting(self, length_s: f64, shortest_s: f64) -> Result<TrimWindow, String> {
		if length_s + TIME_EPSILON_S < shortest_s {
			let shortest = format!("{shortest_s:.3}");
			return Err(format!(
				"the trimmed range lasts {length_s:.3} s; it must last at least {} s",
				shortest.trim_end_matches('0').trim_end_matches('.')
			));
		}

		Ok(TrimWindow {
			start_s: self.start_s,
			length_s,
		})
	}
}

pub fn parse_time(text: &str) -> Result<f64, String> {
	let invalid = || {
		format!(
			"{text:?} is not a time: use seconds (90), minutes:seconds (1:30) or hours:minutes:seconds (00:01:30.500)"
		)
	};
	let parts: Vec<&str> = text.trim().split(':').collect();
	let Some((last, leading)) = parts.split_last().filter(|_| parts.len() <= 3) else {
		return Err(invalid());
	};

	let seconds = decimal(last).ok_or_else(invalid)?;
	if !leading.is_empty() && seconds >= SIXTY {
		return Err(invalid());
	}

	let mut total = seconds;
	let mut unit = SIXTY;
	for (position, part) in leading.iter().rev().enumerate() {
		let value: u32 = part
			.parse()
			.ok()
			.filter(|_| part.bytes().all(|byte| byte.is_ascii_digit()))
			.ok_or_else(invalid)?;
		if position == 0 && leading.len() == 2 && f64::from(value) >= SIXTY {
			return Err(invalid());
		}

		total += f64::from(value) * unit;
		unit *= SIXTY;
	}
	Ok(total)
}

pub fn kept_as_requested(src_bytes: u64, out_bytes: u64) -> String {
	format!(
		"the output is not smaller than the source ({} → {}); it was written because it was requested",
		DecimalBytes(src_bytes),
		DecimalBytes(out_bytes)
	)
}

pub fn kept_as_requested_part(part_bytes: u64, out_bytes: u64) -> String {
	format!(
		"the output is not smaller than the same part of the source ({} → {}); it was written because it was requested",
		DecimalBytes(part_bytes),
		DecimalBytes(out_bytes)
	)
}

fn decimal(text: &str) -> Option<f64> {
	let digits = text.bytes().filter(u8::is_ascii_digit).count();
	let points = text.bytes().filter(|byte| *byte == b'.').count();
	(digits > 0 && points <= 1 && digits + points == text.len())
		.then(|| text.parse().ok())
		.flatten()
}

#[cfg(test)]
mod tests {
	use super::*;

	fn trim(start_s: f64, end_s: Option<f64>) -> Trim {
		Trim { start_s, end_s }
	}

	#[test]
	fn not_smaller_warnings_print_sizes_like_every_other_size() {
		assert_eq!(
			kept_as_requested(12_866_260, 34_335_604),
			"the output is not smaller than the source (12.87 MB → 34.34 MB); it was written because it was requested"
		);
		assert!(kept_as_requested_part(900, 1_200).contains("(900 B → 1.20 kB)"));
	}

	#[test]
	fn times_are_seconds_minutes_or_hours() {
		assert_eq!(parse_time("90"), Ok(90.0));
		assert_eq!(parse_time("1:30"), Ok(90.0));
		assert_eq!(parse_time("00:01:30.500"), Ok(90.5));
		assert_eq!(parse_time("90:00"), Ok(5400.0));
		assert_eq!(parse_time(" 2.25 "), Ok(2.25));
		for bad in [
			"", "-5", "+5", "1:75", "1:60:00", "1:2:3:4", "a", "1.2.3", ":30", "1e3", ".",
		] {
			assert!(parse_time(bad).is_err(), "{bad}");
		}
	}

	#[test]
	fn windows_lie_inside_the_file() {
		assert_eq!(
			trim(10.0, Some(25.0)).window(Some(60.0)),
			Ok(TrimWindow {
				start_s: 10.0,
				length_s: 15.0
			})
		);
		assert_eq!(
			trim(10.0, None).window(Some(60.0)),
			Ok(TrimWindow {
				start_s: 10.0,
				length_s: 50.0
			})
		);
		assert_eq!(
			trim(10.0, Some(60.02)).window(Some(60.0)),
			Ok(TrimWindow {
				start_s: 10.0,
				length_s: 50.0
			})
		);
		assert_eq!(
			trim(5.0, Some(8.0)).window(None),
			Ok(TrimWindow {
				start_s: 5.0,
				length_s: 3.0
			})
		);
		assert!(
			trim(10.0, Some(10.0))
				.window(Some(60.0))
				.unwrap_err()
				.contains("later than")
		);
		assert!(
			trim(61.0, None)
				.window(Some(60.0))
				.unwrap_err()
				.contains("past the end")
		);
		assert!(
			trim(0.0, Some(61.0))
				.window(Some(60.0))
				.unwrap_err()
				.contains("past the end")
		);
		assert!(trim(5.0, None).window(None).unwrap_err().contains("--to is needed"));
	}

	#[test]
	fn a_trim_must_last_a_tenth_of_a_second() {
		let too_short = trim(1.01, Some(1.03)).window(Some(60.0)).unwrap_err();

		assert!(too_short.contains("at least 0.1 s"), "{too_short}");
		assert!(trim(59.95, None).window(Some(60.0)).is_err());
		assert!(trim(5.0, Some(5.05)).window(None).is_err());
		assert!(trim(1.0, Some(1.1)).window(Some(60.0)).is_ok());
	}

	#[test]
	fn a_video_trim_must_hold_a_frame() {
		let five_fps = trim(1.0, Some(1.15)).video_window(Some(60.0), Some(5.0)).unwrap_err();

		assert!(five_fps.contains("it must last at least 0.2 s"), "{five_fps}");
		assert!(trim(1.0, Some(1.2)).video_window(Some(60.0), Some(5.0)).is_ok());
		assert!(
			trim(1.0, Some(1.05))
				.video_window(Some(60.0), Some(50.0))
				.unwrap_err()
				.contains("at least 0.1 s")
		);
		assert!(
			trim(1.0, Some(1.12))
				.video_window(None, Some(7.0))
				.unwrap_err()
				.contains("at least 0.143 s")
		);
		assert!(trim(1.0, Some(1.1)).video_window(Some(60.0), None).is_ok());
	}

	#[test]
	fn a_window_is_moved_onto_the_source_timestamps() {
		let window = TrimWindow {
			start_s: 4.0,
			length_s: 2.0,
		};

		assert_eq!(
			window.on_source(Some(1.4)),
			TrimWindow {
				start_s: 5.4,
				length_s: 2.0
			}
		);
		assert_eq!(window.on_source(None), window);
	}

	#[test]
	fn a_window_that_decodes_short_is_damage() {
		let window = TrimWindow {
			start_s: 10.0,
			length_s: 30.0,
		};

		assert_eq!(window.shortfall(29.5), None);
		assert!(window.shortfall(20.0).unwrap().contains("20.0 s"));
	}

	#[test]
	fn only_transformations_force_an_output() {
		assert!(!JobOptions::default().forced(false));
		assert!(
			JobOptions {
				trim: Some(trim(1.0, None)),
				..JobOptions::default()
			}
			.forced(false)
		);
	}

	#[test]
	fn merge_audio_forces_an_output_only_when_the_merge_is_possible() {
		let merge = JobOptions {
			merge_audio: true,
			..JobOptions::default()
		};

		assert!(!merge.forced(false));
		assert!(merge.forced(true));
		assert!(!JobOptions::default().forced(true));
	}

	#[test]
	fn subtitle_picks_are_a_number_or_a_language() {
		assert_eq!(SubtitlePick::parse(""), SubtitlePick::Default);
		assert_eq!(SubtitlePick::parse("2"), SubtitlePick::Track(2));
		assert_eq!(SubtitlePick::parse("ENG"), SubtitlePick::Language("eng".into()));
	}
}
