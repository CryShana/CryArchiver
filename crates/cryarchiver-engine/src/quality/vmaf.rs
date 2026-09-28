//! Both branches get the timestamps 0, 1, 2… right before libvmaf: its frame sync pairs frames by time, and an
//! output's timestamps never match its source's exactly, so this makes it pair them by position.

use std::fs;
use std::path::Path;

use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::outcome::{ErrorKind, IoSide, JobError};
use crate::process::{self, Progress, STALL_TIMEOUT, ToolCommand};
use crate::video::plan::Resolution;
use crate::video::run_ffmpeg;

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
	pub filters: &'a str,
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
	cmd.arg("-ss")
		.arg(format!("{start_s:.3}"))
		.arg("-t")
		.arg(format!("{length_s:.3}"));
	push_reference(&mut cmd, reference);

	let distorted_chain = chain(&[SCORED_FORMAT, BY_POSITION]);
	let reference_chain = chain(&[reference.filters, SCORED_FORMAT, BY_POSITION]);
	cmd.arg("-filter_complex").arg(format!(
		"[0:v:0]{distorted_chain}[d];[1:{}]{reference_chain}[r];[d][r]{}",
		reference.stream,
		libvmaf(scoring)
	));
	cmd.args(["-f", "null", "-"]);
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
	push_reference(&mut cmd, reference);

	let select = select(windows);
	let distorted_chain = chain(&[&select, SCORED_FORMAT, BY_POSITION]);
	let reference_chain = chain(&[reference.filters, &select, SCORED_FORMAT, BY_POSITION]);
	cmd.arg("-filter_complex").arg(format!(
		"[0:v:0]split[pace][all];[all]{distorted_chain}[d];[1:{}]{reference_chain}[r];[d][r]{}[scored]",
		reference.stream,
		libvmaf(scoring)
	));

	// `pace` carries every decoded frame, so progress keeps moving, and the stall watchdog stays quiet, between
	// windows.
	cmd.args([
		"-map", "[pace]", "-f", "null", "-", "-map", "[scored]", "-f", "null", "-",
	]);
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
	let json = fs::read(&log).map_err(|err| JobError::io(IoSide::Destination(&log), &err))?;
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

fn push_reference(cmd: &mut ToolCommand, reference: &Reference<'_>) {
	if let Some(decoder) = reference.decoder {
		cmd.arg("-c:v").arg(decoder);
	}
	cmd.arg("-i").arg(process::tool_path(reference.source));
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
	use crate::video::joined;

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
			filters: "bwdif=mode=send_field:parity=tff:deint=all,setparams=field_mode=prog",
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
			filters: "scale=in_range=tv:out_range=tv,format=yuv420p10le",
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
}
