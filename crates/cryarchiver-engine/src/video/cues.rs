//! Text subtitles cut to a trim in Rust, because ffmpeg's seek drops a cue that is already on screen at the cut and
//! keeps one running past the end whole.

use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;

use crate::options::TrimWindow;
use crate::outcome::{ErrorKind, IoSide, JobError};
use crate::process::{self, ToolCommand};
use crate::video::run_ffmpeg;
use crate::video::streams::CueFormat;
use crate::workspace::Workspace;

const ASS_DIALOGUE: &str = "Dialogue:";
const ASS_COMMENT: &str = "Comment:";
const BOM: char = '\u{feff}';

/// Writes each track's cues inside `window` (on the source's own timestamps) to a file the mux reads as
/// `-f <format extension>`, reading the source once for all of them. The files come back in `tracks` order.
pub async fn cut(
	ffmpeg: &Path,
	source: &Path,
	workspace: &Workspace,
	tracks: &[(u32, CueFormat)],
	window: TrimWindow,
	cancel: &CancellationToken,
) -> Result<Vec<PathBuf>, JobError> {
	if tracks.is_empty() {
		return Ok(Vec::new());
	}

	let whole = |stream_index: u32, format: CueFormat| {
		workspace.file(&format!("cues-{stream_index}.source.{}", format.extension()))
	};
	let source_bytes = fs::metadata(source).map_or(0, |meta| meta.len());
	let mut cmd = ToolCommand::new(ffmpeg, process::scaled_timeout(source_bytes));
	cmd.args(["-hide_banner", "-nostdin", "-y", "-copyts", "-i"])
		.arg(process::tool_path(source));
	for &(stream_index, format) in tracks {
		cmd.arg("-map")
			.arg(format!("0:{stream_index}"))
			.args(["-c:s", format.extension(), "-f", format.extension()])
			.arg(process::tool_path(&whole(stream_index, format)));
	}
	run_ffmpeg(&cmd, |_| {}, cancel).await?;

	let mut cut = Vec::with_capacity(tracks.len());
	for &(stream_index, format) in tracks {
		let path = whole(stream_index, format);
		let text = fs::read(&path).map_err(|err| JobError::io(IoSide::Destination(&path), &err))?;
		let clipped = clip(&String::from_utf8_lossy(&text), format, window).map_err(|unreadable| {
			ErrorKind::EncodeFailed(format!(
				"couldn't read subtitle track {stream_index}: {unreadable} cue{} couldn't be parsed",
				if unreadable == 1 { "" } else { "s" }
			))
		})?;

		let out = workspace.file(&format!("cues-{stream_index}.{}", format.extension()));
		fs::write(&out, clipped).map_err(|err| JobError::io(IoSide::Destination(&out), &err))?;
		cut.push(out);
	}
	Ok(cut)
}

/// `text` is the track as ffmpeg writes it with `-copyts`, so its times are the source's own timestamps. `Err`
/// counts the cues that couldn't be parsed: leaving them out would lose subtitles without a word.
pub fn clip(text: &str, format: CueFormat, window: TrimWindow) -> Result<String, usize> {
	let text = text.trim_start_matches(BOM).replace("\r\n", "\n");
	match format {
		CueFormat::Srt => clip_srt(&text, window),
		CueFormat::Ass => clip_ass(&text, window),
	}
}

// A cue is a number line, then a timing line, then text up to the next such pair: the text itself may hold empty
// lines (ffmpeg writes `\N\N` as one).
fn clip_srt(text: &str, window: TrimWindow) -> Result<String, usize> {
	let lines: Vec<&str> = text.lines().collect();
	let starts_cue = |at: usize| is_number(lines[at]) && lines.get(at + 1).is_some_and(|line| line.contains("-->"));
	let mut clipped = String::new();
	let mut number = 0;
	let mut unreadable = 0;

	let mut at = 0;
	while at < lines.len() && !starts_cue(at) {
		if !lines[at].trim().is_empty() {
			unreadable += 1;
		}
		at += 1;
	}

	while at < lines.len() {
		let timing = lines[at + 1];
		let mut end = at + 2;
		while end < lines.len() && !starts_cue(end) {
			end += 1;
		}
		let mut cue_text = &lines[at + 2..end];
		while let [rest @ .., last] = cue_text
			&& last.trim().is_empty()
		{
			cue_text = rest;
		}
		at = end;

		let Some((start, stop)) = srt_span(timing) else {
			unreadable += 1;
			continue;
		};
		let Some((start, stop)) = window.clip(start, stop) else {
			continue;
		};

		number += 1;
		let _ = writeln!(clipped, "{number}\n{} --> {}", srt_stamp(start), srt_stamp(stop));
		for line in cue_text {
			let _ = writeln!(clipped, "{line}");
		}
		clipped.push('\n');
	}

	if unreadable > 0 { Err(unreadable) } else { Ok(clipped) }
}

fn clip_ass(text: &str, window: TrimWindow) -> Result<String, usize> {
	let mut clipped = String::new();
	let mut unreadable = 0;

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
			unreadable += 1;
			continue;
		};
		let (Some(start), Some(end)) = (ass_time(start), ass_time(end)) else {
			unreadable += 1;
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

	if unreadable > 0 { Err(unreadable) } else { Ok(clipped) }
}

fn is_number(line: &str) -> bool {
	let line = line.trim();
	!line.is_empty() && line.bytes().all(|byte| byte.is_ascii_digit())
}

fn srt_span(timing: &str) -> Option<(f64, f64)> {
	let (start, end) = timing.split_once("-->")?;
	let end = end.split_whitespace().next()?;
	Some((srt_time(start)?, srt_time(end)?))
}

fn srt_time(text: &str) -> Option<f64> {
	let (clock, millis) = text.trim().split_once(',')?;
	Some(clock_seconds(clock)? + digits(millis)? / 1000.0)
}

fn ass_time(text: &str) -> Option<f64> {
	let (clock, centis) = text.trim().split_once('.')?;
	Some(clock_seconds(clock)? + digits(centis)? / 100.0)
}

fn clock_seconds(clock: &str) -> Option<f64> {
	clock
		.split(':')
		.try_fold(0.0, |total, part| Some(total * 60.0 + digits(part)?))
}

fn digits(text: &str) -> Option<f64> {
	(!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
		.then(|| text.parse().ok())
		.flatten()
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
			clip(srt, CueFormat::Srt, WINDOW).unwrap(),
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
			clip(ass, CueFormat::Ass, WINDOW).unwrap(),
			"[Script Info]\nScriptType: v4.00+\n\n[V4+ Styles]\nStyle: Sign,Arial,20,&Hffffff\n\n[Events]\n\
			 Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
			 Dialogue: 0,0:00:00.00,0:00:02.00,Sign,,0,0,0,,{\\b1}across, the start\n\
			 Dialogue: 1,0:00:03.50,0:00:04.00,Default,,0,0,0,,across the end\n"
		);
	}

	#[test]
	fn an_empty_line_inside_a_cue_stays_in_that_cue() {
		let srt = "1\n00:00:05,000 --> 00:00:06,000\n<font size=\"20\">one\n\ntwo</font>\n\n\
			2\n00:00:06,500 --> 00:00:07,000\nthree\n\n";

		assert_eq!(
			clip(srt, CueFormat::Srt, WINDOW).unwrap(),
			"1\n00:00:01,000 --> 00:00:02,000\n<font size=\"20\">one\n\ntwo</font>\n\n\
			 2\n00:00:02,500 --> 00:00:03,000\nthree\n\n"
		);
	}

	#[test]
	fn cues_that_can_not_be_read_fail_instead_of_vanishing() {
		let garbled_timing = "1\n00:00:05,000 --> 00:0x:06,000\none\n\n2\n00:00:06,000 --> 00:00:07,000\ntwo\n";
		let garbled_dialogue = "[Events]\nDialogue: 0,0:00:05.00,0:00:06.00,Default,,0,0,0,,fine\n\
			Dialogue: 0,five,0:00:06.00,Default,,0,0,0,,broken\nDialogue: 0\n";

		assert_eq!(clip(garbled_timing, CueFormat::Srt, WINDOW), Err(1));
		assert_eq!(clip(garbled_dialogue, CueFormat::Ass, WINDOW), Err(2));
		assert_eq!(clip("not subtitles at all\n", CueFormat::Srt, WINDOW), Err(1));
	}

	#[test]
	fn commas_in_ass_text_are_kept() {
		let ass = "[Events]\nDialogue: 0,0:00:05.00,0:00:06.00,Default,Narrator,0,0,0,,well, then, {\\i1}fine{\\i0}\n";

		assert_eq!(
			clip(ass, CueFormat::Ass, WINDOW).unwrap(),
			"[Events]\nDialogue: 0,0:00:01.00,0:00:02.00,Default,Narrator,0,0,0,,well, then, {\\i1}fine{\\i0}\n"
		);
	}

	#[test]
	fn a_byte_order_mark_and_crlf_are_read() {
		let srt = "\u{feff}1\r\n00:00:05,000 --> 00:00:06,000\r\none\r\n";
		let ass = "\u{feff}[Script Info]\r\nScriptType: v4.00+\r\n[Events]\r\n\
			Dialogue: 0,0:00:05.00,0:00:06.00,Default,,0,0,0,,one\r\n";

		assert_eq!(
			clip(srt, CueFormat::Srt, WINDOW).unwrap(),
			"1\n00:00:01,000 --> 00:00:02,000\none\n\n"
		);
		assert_eq!(
			clip(ass, CueFormat::Ass, WINDOW).unwrap(),
			"[Script Info]\nScriptType: v4.00+\n[Events]\nDialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,one\n"
		);
	}

	#[test]
	fn a_track_with_no_cue_in_the_window_stays_valid() {
		assert_eq!(
			clip("1\n00:00:10,000 --> 00:00:11,000\nafter\n", CueFormat::Srt, WINDOW),
			Ok(String::new())
		);
		assert_eq!(clip("", CueFormat::Srt, WINDOW), Ok(String::new()));
		assert_eq!(
			clip(
				"[Events]\nDialogue: 0,0:00:10.00,0:00:11.00,Default,,0,0,0,,after\n",
				CueFormat::Ass,
				WINDOW
			),
			Ok("[Events]\n".to_owned())
		);
	}
}
