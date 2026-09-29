pub mod analyze;
pub mod burn;
pub mod chapters;
pub mod colour;
pub mod cues;
pub mod encode;
pub mod mux;
pub mod plan;
pub mod quality;
pub mod streams;
pub mod verify;

use tokio_util::sync::CancellationToken;

use crate::options::TrimWindow;
use crate::outcome::{ErrorKind, JobError};
use crate::probe::MediaInfo;
use crate::process::{self, Progress, StdoutMode, ToolCommand, ToolOutput};

/// Demuxers with discontinuous timestamps (TS and PS), for which ffmpeg starts an input no earlier than the
/// streams a command reads.
const START_FROM_READ_STREAMS: &[&str] = &["mpegts", "mpegtsraw", "mpeg"];

/// Shorter delays than this are no delay: no `-itsoffset` or skip is passed for them.
const NO_DELAY_S: f64 = 1e-6;
/// How long before a cut a TS or PS input is seeked. Their seek lands just before the cut, mid-GOP, so the video
/// would decode only from the next keyframe after it; seeked this much earlier, it decodes through the cut, which is
/// then made on the decoded frames.
const PRE_ROLL_S: f64 = 5.0;

/// How a command reads `start_s..start_s + length_s` of the source.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Seek {
	pub start_s: f64,
	/// The input's `-ss` and `-t`.
	pub input_s: f64,
	pub input_length_s: f64,
	/// For a TS or PS input, the part of what it decodes that is kept, on the command's own timeline. The input's
	/// `-t` counts from where ffmpeg starts it, which can be later than the seek, so the end is cut here too.
	pub kept: Option<TrimWindow>,
}

impl Seek {
	/// The cut on a video stream, which moves it onto the timeline of an exact seek to `start_s`. It is a filter
	/// rather than output options so that every command that reads the range keeps the same frames: an output's
	/// `-t` is applied after the encoder rounds the times.
	pub fn trim_filter(self) -> Option<String> {
		self.kept.map(|kept| {
			format!(
				"trim=start={:.6}:end={:.6},setpts=PTS-{:.6}/TB",
				kept.start_s,
				kept.start_s + kept.length_s,
				kept.start_s
			)
		})
	}
}

/// `delay_s`, when it is long enough to act on.
pub fn delay(delay_s: f64) -> Option<f64> {
	(delay_s > NO_DELAY_S).then_some(delay_s)
}

/// Where ffmpeg starts a source's timeline in a command that reads some of its streams (measured on ffmpeg 9.0.1).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct InputStart {
	pub file_s: f64,
	/// The earliest start of the streams read, for a demuxer in `START_FROM_READ_STREAMS`.
	pub streams_s: Option<f64>,
}

impl InputStart {
	pub fn of(info: &MediaInfo, streams: &[u32]) -> Self {
		let corrected = info
			.format
			.format_name
			.split(',')
			.any(|name| START_FROM_READ_STREAMS.contains(&name));
		let streams_s = info
			.streams
			.iter()
			.filter(|stream| corrected && streams.contains(&stream.index))
			.filter_map(|stream| stream.start_time)
			.reduce(f64::min);

		Self {
			file_s: info.format.start_time.unwrap_or(0.0),
			streams_s,
		}
	}

	/// The source timestamp that ffmpeg turns into 0 when the command seeks to `seek_s` (0 for no seek).
	pub fn at(self, seek_s: f64) -> f64 {
		let seeked = self.file_s + seek_s;
		self.streams_s.map_or(seeked, |streams_s| seeked.max(streams_s))
	}

	/// How much later than the seek itself that is.
	pub fn delay_s(self, seek_s: f64) -> f64 {
		self.at(seek_s) - self.file_s - seek_s
	}

	/// The `-itsoffset` that moves what the command reads back onto the file's own clock, which counts from the
	/// file's start as players do; `None` when ffmpeg already starts there.
	pub fn offset_s(self, seek_s: Option<f64>) -> Option<f64> {
		delay(self.delay_s(seek_s.unwrap_or(0.0)))
	}

	pub fn offset_arg(self, seek_s: Option<f64>) -> Option<String> {
		self.offset_s(seek_s).map(|delay_s| format!("{delay_s:.6}"))
	}

	/// How to read `start_s..start_s + length_s`. A TS or PS input is seeked `PRE_ROLL_S` earlier (not before its
	/// start), and the command drops what it decodes before `start_s`.
	pub fn seek(self, start_s: f64, length_s: f64) -> Seek {
		if self.streams_s.is_none() {
			return Seek {
				start_s,
				input_s: start_s,
				input_length_s: length_s,
				kept: None,
			};
		}

		let input_s = (start_s - PRE_ROLL_S).max(0.0);
		Seek {
			start_s,
			input_s,
			input_length_s: length_s + start_s - input_s,
			kept: Some(TrimWindow {
				start_s: start_s - input_s,
				length_s,
			}),
		}
	}
}

/// `InputStart` for a command that reads only the main video and for one that reads every planned video stream.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct VideoStarts {
	pub main: InputStart,
	pub every: InputStart,
}

impl VideoStarts {
	/// `streams` are the planned video streams, main first.
	pub fn of(info: &MediaInfo, streams: &[u32]) -> Self {
		Self {
			main: InputStart::of(info, streams.get(..1).unwrap_or_default()),
			every: InputStart::of(info, streams),
		}
	}
}

pub(crate) async fn run_ffmpeg(
	cmd: &ToolCommand,
	on_progress: impl FnMut(&Progress),
	cancel: &CancellationToken,
) -> Result<ToolOutput, JobError> {
	let out = process::run(cmd, StdoutMode::Progress, on_progress, cancel).await?;
	if out.completed() {
		return Ok(out);
	}

	let reason = process::failure_reason(&out.stderr_tail);
	Err(JobError::with_log(ErrorKind::EncodeFailed(reason), out.stderr_tail))
}

#[cfg(test)]
pub(crate) fn joined(cmd: &ToolCommand) -> String {
	cmd.args
		.iter()
		.map(|arg| arg.to_string_lossy().into_owned())
		.collect::<Vec<_>>()
		.join(" ")
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn ts_and_ps_start_no_earlier_than_the_streams_read_and_the_offset_puts_them_back_on_the_file_clock() {
		let m2ts = MediaInfo::parse(
			br#"{"format":{"format_name":"mpegts","start_time":"1.400000"},"streams":[
			{"index":0,"codec_type":"video","start_time":"1.743000"},
			{"index":1,"codec_type":"audio","start_time":"1.400000"}]}"#,
		)
		.unwrap();
		let mkv = MediaInfo {
			format: crate::probe::Format {
				format_name: "matroska,webm".into(),
				..m2ts.format.clone()
			},
			..m2ts.clone()
		};
		let video = InputStart::of(&m2ts, &[0]);
		let near = |actual: f64, expected: f64| (actual - expected).abs() < 1e-9;

		assert!(near(video.at(0.0), 1.743));
		assert!(near(video.at(0.1), 1.743));
		assert!(near(video.at(4.0), 5.4));
		assert_eq!(video.offset_arg(None).as_deref(), Some("0.343000"));
		assert_eq!(video.offset_arg(Some(0.1)).as_deref(), Some("0.243000"));
		assert_eq!(video.offset_arg(Some(4.0)), None);
		assert_eq!(InputStart::of(&m2ts, &[0, 1]).offset_arg(None), None);
		assert_eq!(InputStart::of(&mkv, &[0]).offset_arg(None), None);
		assert!(near(InputStart::of(&mkv, &[0]).at(0.1), 1.5));
	}
}
