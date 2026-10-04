use indicatif::DecimalBytes;

use crate::format::OutputFormat;
use crate::images::plan::ImageRoute;

const DURATION_SLACK_S: f64 = 0.05;
const SHORTEST_TRIM_S: f64 = 0.1;
const SHORTEST_SPAN_S: f64 = 0.001;
const TIME_EPSILON_S: f64 = 1e-9;
const SIXTY: f64 = 60.0;
const MAX_SIDE: u32 = 65_536;

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

/// `--resize W:H`; a side left out keeps the aspect ratio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resize {
	pub width: Option<u32>,
	pub height: Option<u32>,
}

impl Resize {
	pub fn parse(text: &str) -> Result<Self, String> {
		let parts: Vec<&str> = text.trim().split(':').collect();
		let [width, height] = parts[..] else {
			return Err(format!(
				"\"{text}\" is not a size: use W:H, W:-1 or -1:H, such as 1280:720 or 1280:-1"
			));
		};

		let resize = Self {
			width: side(width)?,
			height: side(height)?,
		};
		if resize.width.is_none() && resize.height.is_none() {
			return Err("give at least one side: 1280:-1 or -1:720 keeps the aspect ratio".to_owned());
		}
		Ok(resize)
	}

	/// The output size for a picture shown `shown` (its display width and height, which can be fractional under a
	/// sample aspect ratio). `even` rounds a computed side to even, as 4:2:0 needs, and refuses an odd side given.
	#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
	pub fn size(self, (shown_width, shown_height): (f64, f64), even: bool) -> Result<(u32, u32), String> {
		let computed = |length: f64| {
			let rounded = if even {
				((length / 2.0).round() * 2.0).max(2.0)
			} else {
				length.round().max(1.0)
			};
			if rounded > f64::from(MAX_SIDE) {
				return Err(format!(
					"--resize keeps the aspect ratio at more than {MAX_SIDE} pixels on a side ({rounded:.0})"
				));
			}
			Ok(rounded as u32)
		};
		let aspect = shown_width / shown_height;

		let size = match (self.width, self.height) {
			(Some(width), Some(height)) => (width, height),
			(Some(width), None) => (width, computed(f64::from(width) / aspect)?),
			(None, Some(height)) => (computed(f64::from(height) * aspect)?, height),
			(None, None) => (computed(shown_width)?, computed(shown_height)?),
		};
		let given_odd = [self.width, self.height].iter().flatten().any(|side| side % 2 == 1);
		if even && given_odd {
			return Err(format!(
				"--resize gives {}×{}, but 4:2:0 video needs an even width and height (use an even size, or -1 for \
				 one side)",
				size.0, size.1
			));
		}
		Ok(size)
	}
}

/// The warning for an output larger than the picture on either side.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn upscaled((shown_width, shown_height): (f64, f64), (width, height): (u32, u32)) -> Option<String> {
	let shown = (shown_width.round() as u32, shown_height.round() as u32);
	(width > shown.0 || height > shown.1).then(|| format!("upscaled from {}x{}", shown.0, shown.1))
}

fn side(text: &str) -> Result<Option<u32>, String> {
	if text.is_empty() || text == "-1" {
		return Ok(None);
	}

	let pixels: u32 = text
		.parse()
		.ok()
		.filter(|_| text.bytes().all(|byte| byte.is_ascii_digit()))
		.ok_or_else(|| {
			format!("\"{text}\" is not a side: use a whole number of pixels, or -1 to keep the aspect ratio")
		})?;
	if pixels == 0 {
		return Err("a side must be at least 1 pixel".to_owned());
	}
	if pixels > MAX_SIDE {
		return Err(format!(
			"a side can be at most {MAX_SIDE} pixels, the largest AV1 holds"
		));
	}
	Ok(Some(pixels))
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
	pub threads: Option<usize>,
	pub max_bitrate_kbps: Option<u32>,
	pub resize: Option<Resize>,
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

	/// `forced`, and also a bitrate cap or a resize, which ask for a video output: audio jobs ignore both.
	pub fn forced_video(&self, merge_possible: bool) -> bool {
		self.forced(merge_possible) || self.max_bitrate_kbps.is_some() || self.resize.is_some()
	}

	/// `forced`, and also a resize, which scales still images and animations too.
	pub fn forced_image(&self) -> bool {
		self.forced(false) || self.resize.is_some()
	}

	/// What forced an audio output that misses its target or isn't smaller: a trim alone says so, since the same run on
	/// the whole file would have kept the original.
	pub fn written_by(&self, merge_possible: bool) -> WrittenBy {
		WrittenBy::of(self.trim.is_some() && !self.without_trim().forced(merge_possible))
	}

	/// `written_by` for a video output, which `forced_video` forces.
	pub fn video_written_by(&self, merge_possible: bool) -> WrittenBy {
		WrittenBy::of(self.trim.is_some() && !self.without_trim().forced_video(merge_possible))
	}

	fn without_trim(&self) -> Self {
		Self {
			trim: None,
			..self.clone()
		}
	}
}

/// What wrote an output the size and quality rules alone would have kept the original for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrittenBy {
	Request,
	/// A trim and nothing else.
	Trim,
}

impl WrittenBy {
	fn of(trim_alone: bool) -> Self {
		if trim_alone { Self::Trim } else { Self::Request }
	}

	pub fn because(self) -> &'static str {
		match self {
			Self::Request => "because it was requested",
			Self::Trim => "because it was trimmed",
		}
	}

	pub fn note(self) -> String {
		format!("it was written {}", self.because())
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

pub fn kept_as_requested(src_bytes: u64, out_bytes: u64, by: WrittenBy) -> String {
	format!(
		"the output is not smaller than the source ({} → {}); {}",
		DecimalBytes(src_bytes),
		DecimalBytes(out_bytes),
		by.note()
	)
}

pub fn kept_as_requested_part(part_bytes: u64, out_bytes: u64, by: WrittenBy) -> String {
	format!(
		"the output is not smaller than the same part of the source ({} → {}); {}",
		DecimalBytes(part_bytes),
		DecimalBytes(out_bytes),
		by.note()
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
			kept_as_requested(12_866_260, 34_335_604, WrittenBy::Request),
			"the output is not smaller than the source (12.87 MB → 34.34 MB); it was written because it was requested"
		);
		assert_eq!(
			kept_as_requested_part(900, 1_200, WrittenBy::Trim),
			"the output is not smaller than the same part of the source (900 B → 1.20 kB); it was written because it \
			 was trimmed"
		);
	}

	#[test]
	fn a_trim_alone_is_named_as_what_wrote_the_output() {
		let trimmed = JobOptions {
			trim: Some(trim(800.0, Some(860.0))),
			..JobOptions::default()
		};
		let with = |options: JobOptions| JobOptions {
			trim: trimmed.trim,
			..options
		};

		assert_eq!(trimmed.video_written_by(false), WrittenBy::Trim);
		assert_eq!(trimmed.written_by(false), WrittenBy::Trim);
		assert_eq!(
			with(JobOptions {
				format: Some(OutputFormat::Mkv),
				..JobOptions::default()
			})
			.video_written_by(false),
			WrittenBy::Request
		);
		assert_eq!(
			with(JobOptions {
				max_bitrate_kbps: Some(12_500),
				..JobOptions::default()
			})
			.video_written_by(false),
			WrittenBy::Request
		);
		assert_eq!(
			with(JobOptions {
				max_bitrate_kbps: Some(12_500),
				..JobOptions::default()
			})
			.written_by(false),
			WrittenBy::Trim,
			"audio ignores a bitrate cap"
		);
		assert_eq!(
			with(JobOptions {
				merge_audio: true,
				..JobOptions::default()
			})
			.video_written_by(false),
			WrittenBy::Trim,
			"nothing to merge"
		);
		assert_eq!(JobOptions::default().video_written_by(false), WrittenBy::Request);
		assert_eq!(WrittenBy::Trim.note(), "it was written because it was trimmed");
		assert_eq!(WrittenBy::Request.because(), "because it was requested");
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
	fn a_bitrate_cap_forces_only_a_video_output() {
		let capped = JobOptions {
			max_bitrate_kbps: Some(12_500),
			..JobOptions::default()
		};
		let stripped = JobOptions {
			strip: true,
			..JobOptions::default()
		};

		assert!(!capped.forced(false));
		assert!(capped.forced_video(false));
		assert!(stripped.forced_video(false));
		assert!(!JobOptions::default().forced_video(true));
	}

	#[test]
	fn a_resize_forces_video_and_image_outputs_but_not_audio() {
		let resized = JobOptions {
			resize: Some(Resize {
				width: Some(1280),
				height: None,
			}),
			..JobOptions::default()
		};

		assert!(!resized.forced(true));
		assert!(resized.forced_video(false));
		assert!(resized.forced_image());
		assert!(!JobOptions::default().forced_image());
		assert!(
			JobOptions {
				strip: true,
				..JobOptions::default()
			}
			.forced_image()
		);
	}

	#[test]
	fn resize_sizes_are_written_like_ffmpeg_scale() {
		let resize = |width, height| Ok(Resize { width, height });

		assert_eq!(Resize::parse("1280:720"), resize(Some(1280), Some(720)));
		assert_eq!(Resize::parse("1280:-1"), resize(Some(1280), None));
		assert_eq!(Resize::parse("1280:"), resize(Some(1280), None));
		assert_eq!(Resize::parse("-1:720"), resize(None, Some(720)));
		assert_eq!(Resize::parse(" :720 "), resize(None, Some(720)));
		for bad in [
			":", "-1:-1", ":-1", "-1:", "0:720", "1280:0", "-2:720", "1280:-5", "a:720", "1280:7.5", "1280", "1:2:3",
			"", "+5:720", "1280x720",
		] {
			assert!(Resize::parse(bad).is_err(), "{bad}");
		}
		assert!(Resize::parse("-1:-1").unwrap_err().contains("at least one side"));
		assert!(Resize::parse("0:720").unwrap_err().contains("at least 1 pixel"));
		assert!(Resize::parse("1:2:3").unwrap_err().contains("W:H"));
		assert_eq!(Resize::parse("65536:-1"), resize(Some(65_536), None));
		assert!(Resize::parse("65537:-1").unwrap_err().contains("at most 65536"));
		assert!(Resize::parse("-1:4000000000").unwrap_err().contains("at most 65536"));
		assert!(Resize::parse("99999999999:-1").is_err());
	}

	#[test]
	fn a_computed_side_past_the_largest_is_refused_rather_than_overflowing() {
		let wide = Resize {
			width: Some(65_536),
			height: None,
		};
		let tall = Resize {
			width: None,
			height: Some(65_536),
		};

		for even in [true, false] {
			assert!(
				wide.size((10.0, 4000.0), even).unwrap_err().contains("more than 65536"),
				"{even}"
			);
			assert!(tall.size((4000.0, 10.0), even).is_err(), "{even}");
		}
		assert_eq!(wide.size((4000.0, 10.0), true), Ok((65_536, 164)));
	}

	#[test]
	fn an_odd_side_for_video_says_how_to_fix_it() {
		let odd = Resize {
			width: Some(161),
			height: Some(90),
		}
		.size((320.0, 240.0), true)
		.unwrap_err();

		assert!(odd.ends_with("(use an even size, or -1 for one side)"), "{odd}");
	}

	#[test]
	fn a_missing_side_follows_the_shown_aspect_ratio_and_video_rounds_it_to_even() {
		let width = |width| Resize {
			width: Some(width),
			height: None,
		};
		let height = |height| Resize {
			width: None,
			height: Some(height),
		};

		assert_eq!(width(202).size((320.0, 240.0), true), Ok((202, 152)));
		assert_eq!(width(202).size((320.0, 240.0), false), Ok((202, 152)));
		assert_eq!(width(201).size((320.0, 240.0), false), Ok((201, 151)));
		assert_eq!(height(100).size((320.0, 240.0), true), Ok((134, 100)));
		assert_eq!(height(100).size((320.0, 240.0), false), Ok((133, 100)));
		assert_eq!(height(576).size((1024.0, 576.0), true), Ok((1024, 576)));
		assert!(width(1).size((320.0, 240.0), true).unwrap_err().contains("even"));
		assert_eq!(width(2).size((4000.0, 10.0), true), Ok((2, 2)));
		assert_eq!(width(2).size((4000.0, 10.0), false), Ok((2, 1)));
		assert_eq!(
			Resize {
				width: Some(160),
				height: Some(90)
			}
			.size((320.0, 240.0), true),
			Ok((160, 90))
		);
		assert!(
			Resize {
				width: Some(161),
				height: Some(90)
			}
			.size((320.0, 240.0), true)
			.unwrap_err()
			.contains("161×90")
		);
		assert_eq!(
			Resize {
				width: Some(161),
				height: Some(91)
			}
			.size((320.0, 240.0), false),
			Ok((161, 91))
		);
	}

	#[test]
	fn a_larger_side_than_shown_is_an_upscale() {
		assert_eq!(
			upscaled((320.0, 240.0), (640, 480)),
			Some("upscaled from 320x240".to_owned())
		);
		assert_eq!(
			upscaled((320.0, 240.0), (321, 100)),
			Some("upscaled from 320x240".to_owned())
		);
		assert_eq!(upscaled((1024.0, 576.0), (1024, 576)), None);
		assert_eq!(upscaled((426.667, 240.0), (426, 240)), None);
		assert_eq!(
			upscaled((426.667, 240.0), (428, 240)),
			Some("upscaled from 427x240".to_owned())
		);
	}

	#[test]
	fn subtitle_picks_are_a_number_or_a_language() {
		assert_eq!(SubtitlePick::parse(""), SubtitlePick::Default);
		assert_eq!(SubtitlePick::parse("2"), SubtitlePick::Track(2));
		assert_eq!(SubtitlePick::parse("ENG"), SubtitlePick::Language("eng".into()));
	}
}
