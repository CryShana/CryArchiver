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

use crate::outcome::{ErrorKind, JobError};
use crate::probe::MediaInfo;
use crate::process::{self, Progress, StdoutMode, ToolCommand, ToolOutput};

/// Demuxers with discontinuous timestamps (TS and PS), for which ffmpeg starts an input no earlier than the
/// streams a command reads.
const START_FROM_READ_STREAMS: &[&str] = &["mpegts", "mpegtsraw", "mpeg"];

/// Shorter `-itsoffset` values than this aren't passed.
const NO_DELAY_S: f64 = 1e-6;

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
	pub fn offset_arg(self, seek_s: Option<f64>) -> Option<String> {
		let delay_s = self.delay_s(seek_s.unwrap_or(0.0));
		(delay_s > NO_DELAY_S).then(|| format!("{delay_s:.6}"))
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
