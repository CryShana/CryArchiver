//! Both branches get the timestamps 0, 1, 2… right before libvmaf: its frame sync pairs frames by time, and an
//! output's timestamps never match its source's exactly, so this makes it pair them by position.

use std::fs;
use std::path::Path;

use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::options::TrimWindow;
use crate::outcome::{ErrorKind, JobError};
use crate::process::{self, Progress, STALL_TIMEOUT, ToolCommand};
use crate::video::burn::{self, Overlay};
use crate::video::plan::Resolution;
use crate::video::{InputStart, Seek, run_ffmpeg};

pub const FILTER: &str = "libvmaf";
pub const MODELS: [&str; 2] = [HD_MODEL, UHD_MODEL];

const HD_MODEL: &str = "vmaf_v0.6.1";
const UHD_MODEL: &str = "vmaf_4k_v0.6.1";
const SCORED_FORMAT: &str = "format=yuv420p10le";
const BY_POSITION: &str = "settb=1/1000,setpts=N";
const LOW_PERCENTILE: f64 = 0.05;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Score {
	pub mean: f64,
	pub p5: f64,
	pub frames: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameWindow {
	pub first: u64,
	pub last: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct Reference<'a> {
	pub source: &'a Path,
	pub stream: u32,
	pub decoder: Option<&'static str>,
	/// `VideoPlan::shape` and `VideoPlan::filters`.
	pub shape: &'a str,
	pub filters: &'a str,
	pub window: Option<TrimWindow>,
	/// Where ffmpeg starts the source when it reads only `stream`.
	pub start: InputStart,
	pub overlay: Option<Overlay>,
	pub subtitles: Option<&'a str>,
}

#[derive(Debug, Clone, Copy)]
pub struct Scoring<'a> {
	pub model: &'static str,
	pub threads: usize,
	pub dir: &'a Path,
	pub log: &'a str,
}

#[derive(Deserialize)]
struct Log {
	frames: Vec<LogFrame>,
}

#[derive(Deserialize)]
struct LogFrame {
	metrics: Metrics,
}

#[derive(Deserialize)]
struct Metrics {
	vmaf: f64,
}

pub fn model_for(width: u32, height: u32) -> &'static str {
	match Resolution::of(width, height) {
		Resolution::Uhd4k | Resolution::Uhd8k => UHD_MODEL,
		Resolution::Sd | Resolution::Hd720 | Resolution::Hd1080 | Resolution::Qhd1440 => HD_MODEL,
	}
}

#[allow(
	clippy::cast_precision_loss,
	clippy::cast_possible_truncation,
	clippy::cast_sign_loss
)]
pub fn pool(frames: &[f64]) -> Option<Score> {
	if frames.is_empty() {
		return None;
	}

	let mut sorted = frames.to_vec();
	sorted.sort_by(f64::total_cmp);
	let low = sorted[(sorted.len() as f64 * LOW_PERCENTILE).floor() as usize];

	Some(Score {
		mean: sorted.iter().sum::<f64>() / sorted.len() as f64,
		p5: low,
		frames: sorted.len(),
	})
}

pub fn parse_log(json: &[u8]) -> Result<Vec<f64>, String> {
	let log: Log = serde_json::from_slice(json).map_err(|err| format!("unreadable VMAF log: {err}"))?;
	Ok(log.frames.into_iter().map(|frame| frame.metrics.vmaf).collect())
}

pub fn sample_command(
	ffmpeg: &Path,
	distorted: &Path,
	reference: &Reference<'_>,
	start_s: f64,
	length_s: f64,
	scoring: &Scoring<'_>,
) -> ToolCommand {
	let mut cmd = base(ffmpeg, distorted, scoring);
	let cut = reference.start.seek(start_s, length_s);
	cmd.seek(cut.input_s, cut.input_length_s);
	push_reference(&mut cmd, reference, scoring, Some(cut));

	let distorted_chain = chain(&[SCORED_FORMAT, BY_POSITION]);
	let reference_chain = chain(&[reference.filters, SCORED_FORMAT, BY_POSITION]);
	// A mapped label, not an unlabeled output: with an unlabeled one, ffmpeg 9 doesn't start a TS or PS reference at
	// the streams it reads (as `InputStart` has it), but at the file's start.
	cmd.arg("-filter_complex").arg(format!(
		"[0:v:0]{distorted_chain}[d];{};[d][r]{}[scored]",
		reference_branch(reference, &reference_chain, Some(cut)),
		libvmaf(scoring)
	));
	cmd.args(["-map", "[scored]", "-f", "null", "-"]);
	cmd
}

pub fn window_command(
	ffmpeg: &Path,
	distorted: &Path,
	reference: &Reference<'_>,
	windows: &[FrameWindow],
	scoring: &Scoring<'_>,
) -> ToolCommand {
	let mut cmd = base(ffmpeg, distorted, scoring);
	let cut = reference
		.window
		.map(|window| reference.start.seek(window.start_s, window.length_s));
	if let Some(cut) = cut {
		cmd.seek(cut.input_s, cut.input_length_s);
	}
	push_reference(&mut cmd, reference, scoring, cut);

	let select = select(windows);
	let distorted_chain = chain(&[&select, SCORED_FORMAT, BY_POSITION]);
	let reference_chain = chain(&[reference.filters, &select, SCORED_FORMAT, BY_POSITION]);
	cmd.arg("-filter_complex").arg(format!(
		"[0:v:0]split[pace][all];[all]{distorted_chain}[d];{};[d][r]{}[scored]",
		reference_branch(reference, &reference_chain, cut),
		libvmaf(scoring)
	));

	// `pace` carries every decoded frame, so progress keeps moving, and the stall watchdog stays quiet, between
	// windows.
	cmd.args([
		"-map", "[pace]", "-f", "null", "-", "-map", "[scored]", "-f", "null", "-",
	]);
	cmd
}

/// One window of a full check, found by seeking instead of decoding both files up to it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SeekedWindow {
	/// Where the distorted file is seeked, on its own timestamps.
	pub distorted_s: f64,
	/// Where the reference is seeked, as a search sample would be.
	pub source_s: f64,
	pub length_s: f64,
	/// Frames read past before the window, kept out of the score.
	pub skipped: u64,
	pub frames: u64,
}

/// Scores one window of `distorted` against the source: the distorted file is seeked on its own timestamps
/// (`-seek_timestamp`), the reference exactly like a search sample at the same source time, and each keeps `frames`
/// frames from there.
pub fn seeked_window_command(
	ffmpeg: &Path,
	distorted: &Path,
	window: SeekedWindow,
	reference: &Reference<'_>,
	scoring: &Scoring<'_>,
) -> ToolCommand {
	let mut cmd = ToolCommand::new(ffmpeg, STALL_TIMEOUT);
	cmd.in_dir(scoring.dir);
	cmd.args(["-hide_banner", "-nostdin", "-seek_timestamp", "1"]);
	cmd.seek(window.distorted_s, window.length_s);
	cmd.arg("-i").arg(process::tool_path(distorted));

	let cut = reference.start.seek(window.source_s, window.length_s);
	cmd.seek(cut.input_s, cut.input_length_s);
	push_reference(&mut cmd, reference, scoring, Some(cut));

	let keep = format!(
		"trim=start_frame={}:end_frame={}",
		window.skipped,
		window.skipped + window.frames
	);
	let distorted_chain = chain(&[&keep, SCORED_FORMAT, BY_POSITION]);
	let reference_chain = chain(&[reference.filters, &keep, SCORED_FORMAT, BY_POSITION]);
	cmd.arg("-filter_complex").arg(format!(
		"[0:v:0]{distorted_chain}[d];{};[d][r]{}[scored]",
		reference_branch(reference, &reference_chain, Some(cut)),
		libvmaf(scoring)
	));
	cmd.args(["-map", "[scored]", "-f", "null", "-"]);
	cmd
}

pub async fn frames(
	cmd: &ToolCommand,
	scoring: &Scoring<'_>,
	on_progress: impl FnMut(&Progress),
	cancel: &CancellationToken,
) -> Result<Vec<f64>, JobError> {
	let out = run_ffmpeg(cmd, on_progress, cancel).await?;

	let log = scoring.dir.join(scoring.log);
	let json = fs::read(&log).map_err(|err| {
		JobError::with_log(
			ErrorKind::EncodeFailed(format!("unreadable VMAF log: {err}")),
			out.stderr_tail.clone(),
		)
	})?;
	let _ = fs::remove_file(&log);

	parse_log(&json).map_err(|reason| JobError::with_log(ErrorKind::EncodeFailed(reason), out.stderr_tail))
}

fn base(ffmpeg: &Path, distorted: &Path, scoring: &Scoring<'_>) -> ToolCommand {
	let mut cmd = ToolCommand::new(ffmpeg, STALL_TIMEOUT);
	cmd.in_dir(scoring.dir);
	cmd.args(["-hide_banner", "-nostdin", "-i"])
		.arg(process::tool_path(distorted));
	cmd
}

fn push_reference(cmd: &mut ToolCommand, reference: &Reference<'_>, scoring: &Scoring<'_>, cut: Option<Seek>) {
	if let Some(offset) = reference.start.offset_arg(cut.map(|cut| cut.input_s)) {
		cmd.arg("-itsoffset").arg(offset);
	}
	if let Some(decoder) = reference.decoder {
		cmd.arg("-c:v").arg(decoder);
	}
	cmd.arg("-i").arg(process::tool_path(reference.source));

	if let Some(overlay) = reference.overlay {
		cmd.args(overlay.input_args())
			.arg("-i")
			.arg(process::tool_path(&scoring.dir.join(burn::BITMAP_FILE)));
	}
}

/// The reference makes the same cut and draws the same subtitles as the encode it is scored against, shifted to its
/// own seek.
fn reference_branch(reference: &Reference<'_>, chain: &str, cut: Option<Seek>) -> String {
	let video = format!("[1:{}]", reference.stream);
	let trim = cut.and_then(Seek::trim_filter).unwrap_or_default();
	let shaped = burn::chained(&[&trim, reference.shape]);
	let seek_s = cut.map(|cut| cut.start_s);
	match reference.overlay {
		Some(overlay) => burn::overlay_graph((&video, &shaped), "[2:0]", overlay, seek_s, chain, "[r]"),
		None => format!(
			"{video}{}[r]",
			burn::main_chain(&shaped, reference.subtitles, chain, seek_s)
		),
	}
}

fn chain(parts: &[&str]) -> String {
	parts
		.iter()
		.filter(|part| !part.is_empty())
		.copied()
		.collect::<Vec<_>>()
		.join(",")
}

fn libvmaf(scoring: &Scoring<'_>) -> String {
	format!(
		"{FILTER}=model=version={}:n_threads={}:log_fmt=json:log_path={}",
		scoring.model,
		scoring.threads.max(1),
		scoring.log
	)
}

fn select(windows: &[FrameWindow]) -> String {
	let ranges: Vec<String> = windows
		.iter()
		.map(|window| format!("between(n\\,{}\\,{})", window.first, window.last))
		.collect();
	format!("select='{}'", ranges.join("+"))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::video::{InputStart, joined};

	fn scoring() -> Scoring<'static> {
		Scoring {
			model: HD_MODEL,
			threads: 8,
			dir: Path::new("work"),
			log: "vmaf-probe0-1.json",
		}
	}

	#[test]
	fn pooling_takes_the_mean_and_the_fifth_percentile() {
		let frames: Vec<f64> = (0..100).map(f64::from).collect();

		let score = pool(&frames).unwrap();

		assert!((score.mean - 49.5).abs() < 1e-9, "{score:?}");
		assert!((score.p5 - 5.0).abs() < 1e-9, "{score:?}");
		assert_eq!(score.frames, 100);
		assert_eq!(pool(&[]), None);
	}

	#[test]
	fn reads_per_frame_scores_from_the_log() {
		let json = br#"{"version":"e80d6c5","fps":250.0,"frames":[
			{"frameNum":0,"metrics":{"integer_adm2":0.98,"vmaf":92.359168}},
			{"frameNum":1,"metrics":{"vmaf":95.608822}}],
			"pooled_metrics":{"vmaf":{"mean":93.98}}}"#;

		assert_eq!(parse_log(json).unwrap(), [92.359_168, 95.608_822]);
		assert!(parse_log(b"{}").is_err());
	}

	#[test]
	fn the_4k_model_is_for_the_largest_buckets() {
		assert_eq!(model_for(1920, 1080), HD_MODEL);
		assert_eq!(model_for(1080, 1920), HD_MODEL);
		assert_eq!(model_for(3440, 1440), HD_MODEL);
		assert_eq!(model_for(3840, 2160), UHD_MODEL);
		assert_eq!(model_for(7680, 4320), UHD_MODEL);
	}

	#[test]
	fn samples_seek_the_reference_like_the_sample_encode() {
		let reference = Reference {
			source: Path::new("in.webm"),
			stream: 0,
			decoder: Some("libvpx-vp9"),
			shape: "bwdif=mode=send_field:parity=tff:deint=all",
			filters: "setparams=field_mode=prog",
			window: None,
			start: InputStart::default(),
			overlay: None,
			subtitles: None,
		};

		let cmd = sample_command(
			Path::new("ffmpeg"),
			Path::new("probe0-1.mkv"),
			&reference,
			12.5,
			4.0,
			&scoring(),
		);

		let args = joined(&cmd);
		assert_eq!(cmd.current_dir.as_deref(), Some(Path::new("work")));
		assert!(args.contains(" -ss 12.500 -t 4.000 -c:v libvpx-vp9 -i "), "{args}");
		assert!(
			args.contains(
				"[0:v:0]format=yuv420p10le,settb=1/1000,setpts=N[d];\
				 [1:0]bwdif=mode=send_field:parity=tff:deint=all,setparams=field_mode=prog,format=yuv420p10le,\
				 settb=1/1000,setpts=N[r];\
				 [d][r]libvmaf=model=version=vmaf_v0.6.1:n_threads=8:log_fmt=json:log_path=vmaf-probe0-1.json"
			),
			"{args}"
		);
		assert!(args.ends_with(" -f null -"), "{args}");
	}

	#[test]
	fn windows_select_the_same_frames_after_the_reference_filters() {
		let reference = Reference {
			source: Path::new("in.mkv"),
			stream: 3,
			decoder: None,
			shape: "",
			filters: "scale=in_range=tv:out_range=tv,format=yuv420p10le",
			window: None,
			start: InputStart::default(),
			overlay: None,
			subtitles: None,
		};
		let windows = [
			FrameWindow { first: 10, last: 19 },
			FrameWindow { first: 300, last: 309 },
		];

		let args = joined(&window_command(
			Path::new("ffmpeg"),
			Path::new("video.a1.mkv"),
			&reference,
			&windows,
			&scoring(),
		));

		let select = "select='between(n\\,10\\,19)+between(n\\,300\\,309)'";
		assert!(!args.contains(" -ss "), "{args}");
		assert!(
			args.contains(&format!("[0:v:0]split[pace][all];[all]{select},format=yuv420p10le")),
			"{args}"
		);
		assert!(
			args.contains(&format!(
				"[1:3]scale=in_range=tv:out_range=tv,format=yuv420p10le,{select},format="
			)),
			"{args}"
		);
		assert!(
			args.ends_with("-map [pace] -f null - -map [scored] -f null -"),
			"{args}"
		);
	}

	#[test]
	fn a_seeked_window_reads_only_its_own_stretch_of_both_files() {
		let reference = Reference {
			source: Path::new("in.mkv"),
			stream: 3,
			decoder: None,
			shape: "bwdif=mode=send_field:parity=tff:deint=all",
			filters: "format=yuv420p10le",
			window: None,
			start: InputStart::default(),
			overlay: None,
			subtitles: None,
		};
		let window = SeekedWindow {
			distorted_s: 61.98,
			source_s: 65.98,
			length_s: 5.04,
			skipped: 1,
			frames: 250,
		};

		let args = joined(&seeked_window_command(
			Path::new("ffmpeg"),
			Path::new("video.a1.mkv"),
			window,
			&reference,
			&scoring(),
		));

		let reference_at = args.find(" -ss 65.980 -t 5.040 -i ").expect(&args);
		assert!(
			args.starts_with("-hide_banner -nostdin -seek_timestamp 1 -ss 61.980 -t 5.040 -i ")
				&& reference_at > args.find("video.a1.mkv").unwrap(),
			"{args}"
		);
		assert!(
			args.contains("[0:v:0]trim=start_frame=1:end_frame=251,format=yuv420p10le,settb=1/1000,setpts=N[d];"),
			"{args}"
		);
		assert!(
			args.contains(
				"[1:3]bwdif=mode=send_field:parity=tff:deint=all,format=yuv420p10le,trim=start_frame=1:end_frame=251,"
			),
			"{args}"
		);
		assert!(!args.contains("select=") && !args.contains("[pace]"), "{args}");
	}

	#[test]
	fn a_trimmed_reference_is_seeked_like_the_trimmed_encode() {
		let reference = Reference {
			source: Path::new("in.mkv"),
			stream: 0,
			decoder: None,
			shape: "",
			filters: "format=yuv420p10le",
			window: Some(TrimWindow {
				start_s: 4.0,
				length_s: 4.0,
			}),
			start: InputStart::default(),
			overlay: None,
			subtitles: None,
		};

		let args = joined(&window_command(
			Path::new("ffmpeg"),
			Path::new("video.a1.mkv"),
			&reference,
			&[FrameWindow { first: 0, last: 99 }],
			&scoring(),
		));

		assert!(args.contains(" -ss 4.000 -t 4.000 -i "), "{args}");
	}

	#[test]
	fn an_overlaid_reference_draws_the_same_subtitles() {
		let reference = Reference {
			source: Path::new("in.mkv"),
			stream: 0,
			decoder: None,
			shape: "",
			filters: "format=yuv420p10le",
			window: None,
			start: InputStart {
				file_s: 0.0,
				streams_s: Some(2.5),
			},
			overlay: Some(Overlay {
				offset_s: 1.0,
				canvas: Some((720, 480)),
				ten_bit: true,
			}),
			subtitles: None,
		};

		let args = joined(&sample_command(
			Path::new("ffmpeg"),
			Path::new("probe0-0.mkv"),
			&reference,
			2.0,
			4.0,
			&scoring(),
		));

		assert!(
			args.contains(" -ss 0.000 -t 6.000 -itsoffset 2.500000 -i ")
				&& args.contains(" -canvas_size 720x480 -itsoffset 1.000000 -i "),
			"{args}"
		);
		assert!(
			args.contains(
				"[1:0]trim=start=2.000000:end=6.000000,setpts=PTS-2.000000/TB,settb=AVTB,setpts=PTS+2.000/TB[bv];\
				 [2:0]setpts=PTS-0.002/TB[bs];[bs][bv]scale2ref[bss][bvs];\
				 [bss]format=yuva420p10le[bsd];[bvs][bsd]overlay=eof_action=pass:format=yuv420p10,\
				 setpts=PTS-2.000/TB,format=yuv420p10le,format=yuv420p10le,settb=1/1000,setpts=N[r];"
			),
			"{args}"
		);
		assert!(args.ends_with("[scored] -map [scored] -f null -"), "{args}");
	}

	#[test]
	fn a_matroska_reference_is_seeked_exactly_and_not_cut_again() {
		let reference = Reference {
			source: Path::new("in.mkv"),
			stream: 0,
			decoder: None,
			shape: "",
			filters: "format=yuv420p10le",
			window: None,
			start: InputStart::default(),
			overlay: None,
			subtitles: None,
		};

		let args = joined(&sample_command(
			Path::new("ffmpeg"),
			Path::new("probe0-0.mkv"),
			&reference,
			12.5,
			4.0,
			&scoring(),
		));

		assert!(args.contains(" -ss 12.500 -t 4.000 -i "), "{args}");
		assert!(!args.contains("trim="), "{args}");
	}

	#[test]
	fn the_reference_is_shaped_like_the_encode_before_its_subtitles_are_drawn() {
		let shaped = |subtitles: Option<&'static str>, overlay: Option<Overlay>| Reference {
			source: Path::new("in.mkv"),
			stream: 0,
			decoder: None,
			shape: "scale=w=640:h=480,setsar=1",
			filters: "format=yuv420p10le",
			window: None,
			start: InputStart::default(),
			overlay,
			subtitles,
		};
		let command = |reference: &Reference<'_>| {
			joined(&window_command(
				Path::new("ffmpeg"),
				Path::new("video.a1.mkv"),
				reference,
				&[FrameWindow { first: 0, last: 99 }],
				&scoring(),
			))
		};

		let text = command(&shaped(Some("subtitles=subs.ass"), None));
		let bitmap = command(&shaped(
			None,
			Some(Overlay {
				offset_s: 0.0,
				canvas: None,
				ten_bit: false,
			}),
		));
		let plain = command(&shaped(None, None));

		assert!(
			text.contains("[1:0]scale=w=640:h=480,setsar=1,subtitles=subs.ass,format=yuv420p10le,select="),
			"{text}"
		);
		assert!(
			bitmap.contains("[1:0]scale=w=640:h=480,setsar=1,settb=AVTB[bv];[2:0]setpts=PTS-0.002/TB[bs];"),
			"{bitmap}"
		);
		assert!(
			plain.contains("[1:0]scale=w=640:h=480,setsar=1,format=yuv420p10le,select="),
			"{plain}"
		);
	}

	#[test]
	fn burned_text_in_the_reference_follows_its_seek() {
		let burned = |window: Option<TrimWindow>| Reference {
			source: Path::new("in.mkv"),
			stream: 0,
			decoder: None,
			shape: "",
			filters: "format=yuv420p10le",
			window,
			start: InputStart::default(),
			overlay: None,
			subtitles: Some("subtitles=subs.ass"),
		};
		let trim = TrimWindow {
			start_s: 4.0,
			length_s: 20.0,
		};

		let sampled = joined(&sample_command(
			Path::new("ffmpeg"),
			Path::new("probe0-0.mkv"),
			&burned(Some(trim)),
			12.5,
			4.0,
			&scoring(),
		));
		let checked = joined(&window_command(
			Path::new("ffmpeg"),
			Path::new("video.a1.mkv"),
			&burned(Some(trim)),
			&[FrameWindow { first: 0, last: 99 }],
			&scoring(),
		));
		let untrimmed = joined(&window_command(
			Path::new("ffmpeg"),
			Path::new("video.a1.mkv"),
			&burned(None),
			&[FrameWindow { first: 0, last: 99 }],
			&scoring(),
		));

		let shifted = |start: &str| {
			format!(
				"[1:0]settb=AVTB,setpts=PTS+{start}/TB,subtitles=subs.ass,setpts=PTS-{start}/TB,format=yuv420p10le,"
			)
		};
		assert!(sampled.contains(&shifted("12.500")), "{sampled}");
		assert!(checked.contains(&shifted("4.000")), "{checked}");
		assert!(
			untrimmed.contains("[1:0]subtitles=subs.ass,format=yuv420p10le,select="),
			"{untrimmed}"
		);
	}
}
