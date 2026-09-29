//! Text subtitles cut to a trim in Rust, because ffmpeg's seek drops a cue that is already on screen at the cut and
//! keeps one running past the end whole.

use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;

use crate::options::TrimWindow;
use crate::outcome::{IoSide, JobError};
use crate::process::{self, ToolCommand};
use crate::video::run_ffmpeg;
use crate::video::streams::CueFormat;
use crate::workspace::Workspace;

const ASS_DIALOGUE: &str = "Dialogue:";
const ASS_COMMENT: &str = "Comment:";

/// Writes the track's cues inside `window` (on the source's own timestamps) to a file the mux reads as
/// `-f <format extension>`.
pub async fn cut(
	ffmpeg: &Path,
	source: &Path,
	workspace: &Workspace,
	stream_index: u32,
	format: CueFormat,
	window: TrimWindow,
	cancel: &CancellationToken,
) -> Result<PathBuf, JobError> {
	let extension = format.extension();
	let whole = workspace.file(&format!("cues-{stream_index}.source.{extension}"));
	let source_bytes = fs::metadata(source).map_or(0, |meta| meta.len());
	let mut cmd = ToolCommand::new(ffmpeg, process::scaled_timeout(source_bytes));
	cmd.args(["-hide_banner", "-nostdin", "-y", "-copyts", "-i"])
		.arg(process::tool_path(source))
		.arg("-map")
		.arg(format!("0:{stream_index}"))
		.args(["-c:s", extension, "-f", extension])
		.arg(process::tool_path(&whole));
	run_ffmpeg(&cmd, |_| {}, cancel).await?;

	let text = fs::read(&whole).map_err(|err| JobError::io(IoSide::Destination(&whole), &err))?;
	let cut = workspace.file(&format!("cues-{stream_index}.{extension}"));
	fs::write(&cut, clip(&String::from_utf8_lossy(&text), format, window))
		.map_err(|err| JobError::io(IoSide::Destination(&cut), &err))?;
	Ok(cut)
}

/// `text` is the track as ffmpeg writes it with `-copyts`, so its times are the source's own timestamps.
pub fn clip(text: &str, format: CueFormat, window: TrimWindow) -> String {
	match format {
		CueFormat::Srt => clip_srt(text, window),
		CueFormat::Ass => clip_ass(text, window),
	}
}

fn clip_srt(text: &str, window: TrimWindow) -> String {
	let text = text.replace("\r\n", "\n");
	let mut clipped = String::new();
	let mut number = 0;

	for block in text.split("\n\n") {
		let mut lines = block.lines().skip_while(|line| !line.contains("-->"));
		let Some(timing) = lines.next() else {
			continue;
		};
		let Some((start, end)) = timing.split_once("-->") else {
			continue;
		};
		let (Some(start), Some(end)) = (srt_time(start), srt_time(end)) else {
			continue;
		};
		let Some((start, end)) = window.clip(start, end) else {
			continue;
		};

		number += 1;
		let _ = writeln!(clipped, "{number}\n{} --> {}", srt_stamp(start), srt_stamp(end));
		for line in lines {
			let _ = writeln!(clipped, "{line}");
		}
		clipped.push('\n');
	}
	clipped
}

fn clip_ass(text: &str, window: TrimWindow) -> String {
	let mut clipped = String::new();
	for line in text.lines() {
		if line.starts_with(ASS_COMMENT) {
			continue;
		}
		let Some(event) = line.strip_prefix(ASS_DIALOGUE) else {
			let _ = writeln!(clipped, "{line}");
			continue;
		};

		let mut fields = event.trim_start().splitn(4, ',');
		let (Some(layer), Some(start), Some(end), Some(rest)) =
			(fields.next(), fields.next(), fields.next(), fields.next())
		else {
			continue;
		};
		let (Some(start), Some(end)) = (ass_time(start), ass_time(end)) else {
			continue;
		};

		if let Some((start, end)) = window.clip(start, end) {
			let _ = writeln!(
				clipped,
				"{ASS_DIALOGUE} {layer},{},{},{rest}",
				ass_stamp(start),
				ass_stamp(end)
			);
		}
	}
	clipped
}

fn srt_time(text: &str) -> Option<f64> {
	let (clock, millis) = text.trim().split_once(',')?;
	Some(clock_seconds(clock)? + millis.parse::<f64>().ok()? / 1000.0)
}

fn ass_time(text: &str) -> Option<f64> {
	let (clock, centis) = text.trim().split_once('.')?;
	Some(clock_seconds(clock)? + centis.parse::<f64>().ok()? / 100.0)
}

fn clock_seconds(clock: &str) -> Option<f64> {
	let mut total = 0.0;
	for part in clock.split(':') {
		total = total * 60.0 + part.parse::<f64>().ok()?;
	}
	Some(total)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn srt_stamp(seconds: f64) -> String {
	let millis = (seconds * 1000.0).round() as u64;
	format!(
		"{:02}:{:02}:{:02},{:03}",
		millis / 3_600_000,
		millis / 60_000 % 60,
		millis / 1000 % 60,
		millis % 1000
	)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn ass_stamp(seconds: f64) -> String {
	let centis = (seconds * 100.0).round() as u64;
	format!(
		"{}:{:02}:{:02}.{:02}",
		centis / 360_000,
		centis / 6000 % 60,
		centis / 100 % 60,
		centis % 100
	)
}

#[cfg(test)]
mod tests {
	use super::*;

	const WINDOW: TrimWindow = TrimWindow {
		start_s: 4.0,
		length_s: 4.0,
	};

	#[test]
	fn srt_cues_are_clipped_to_the_window_and_renumbered() {
		let srt = "1\r\n00:00:03,500 --> 00:00:06,000\r\nacross the start\r\n\r\n\
			2\r\n00:00:06,500 --> 00:00:07,000\r\ninside\r\ntwo lines\r\n\r\n\
			3\r\n00:00:07,500 --> 00:00:09,000\r\nacross the end\r\n\r\n\
			4\r\n00:00:10,000 --> 00:00:11,000\r\nafter\r\n";

		assert_eq!(
			clip(srt, CueFormat::Srt, WINDOW),
			"1\n00:00:00,000 --> 00:00:02,000\nacross the start\n\n\
			 2\n00:00:02,500 --> 00:00:03,000\ninside\ntwo lines\n\n\
			 3\n00:00:03,500 --> 00:00:04,000\nacross the end\n\n"
		);
	}

	#[test]
	fn ass_events_are_clipped_and_everything_else_is_kept() {
		let ass = "[Script Info]\nScriptType: v4.00+\n\n[V4+ Styles]\nStyle: Sign,Arial,20,&Hffffff\n\n[Events]\n\
			Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
			Dialogue: 0,0:00:03.50,0:00:06.00,Sign,,0,0,0,,{\\b1}across, the start\n\
			Comment: 0,0:00:01.00,0:00:02.00,Sign,,0,0,0,,before\n\
			Dialogue: 1,0:00:07.50,1:00:09.00,Default,,0,0,0,,across the end\n";

		assert_eq!(
			clip(ass, CueFormat::Ass, WINDOW),
			"[Script Info]\nScriptType: v4.00+\n\n[V4+ Styles]\nStyle: Sign,Arial,20,&Hffffff\n\n[Events]\n\
			 Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
			 Dialogue: 0,0:00:00.00,0:00:02.00,Sign,,0,0,0,,{\\b1}across, the start\n\
			 Dialogue: 1,0:00:03.50,0:00:04.00,Default,,0,0,0,,across the end\n"
		);
	}

	#[test]
	fn a_track_with_no_cue_in_the_window_stays_valid() {
		assert_eq!(
			clip("1\n00:00:10,000 --> 00:00:11,000\nafter\n", CueFormat::Srt, WINDOW),
			""
		);
		assert_eq!(
			clip(
				"[Events]\nDialogue: 0,0:00:10.00,0:00:11.00,Default,,0,0,0,,after\n",
				CueFormat::Ass,
				WINDOW
			),
			"[Events]\n"
		);
	}
}
