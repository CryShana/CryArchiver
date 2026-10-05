use std::fs;
use std::path::Path;
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use crate::audio::plan::{FileOptions, FilePlan, TrackPlan, merge_delays};
use crate::container::{Container, Cover, MATROSKA_AUDIO};
use crate::options::TrimWindow;
use crate::outcome::{ErrorKind, IoSide, JobError, KeepReason};
use crate::probe::{MediaInfo, Stream};
use crate::process::{self, STALL_TIMEOUT, StdoutMode, ToolCommand};
use crate::video::analyze::ScanType;
use crate::video::plan::{Encoder, Threads, VideoPlan, scanned_frames, svt_params};
use crate::video::streams::{Extracted, StreamPlan};
use crate::video::{InputStart, Seek, analyze, burn, run_ffmpeg};
use crate::workspace::Workspace;

pub const DECODE_ERRORS: &[&str] = &[
	"error while decoding",
	"Invalid NAL unit",
	"concealing",
	"Invalid data found",
	"missing picture",
	"Error splitting the input into NAL units",
	"corrupt",
	"decode_slice_header error",
];
pub const MIN_PREDICTED_DURATION_S: f64 = 120.0;

const DURATION_TOLERANCE_S: f64 = 0.1;
/// A video that decodes this much shorter than it declares is damaged: a share of its length, but at least the floor
/// or one frame, whichever is longer, since the decoded time ends a frame early.
const DAMAGE_SHARE: f64 = 0.02;
const DAMAGE_FLOOR_S: f64 = 1.0;
const ROUNDING_S: f64 = 1e-9;
const PREDICTION_MARGIN: f64 = 0.05;
const RETRY_SIZE_SLACK: f64 = 1.10;
/// A retry's size is predicted at most this far below the lowest probe, as far as the retry follows the probe curve.
const MAX_SIZE_EXTRAPOLATION: f64 = 2.0;
/// Beyond the probes, the size curve's slope is only taken from two probes at least this far apart: the last steps of
/// a search can be a quarter CRF apart, and their slope carried several CRF would err far past the slack.
const MIN_EXTRAPOLATION_BASE: f64 = 2.0;
const MATROSKA_TIME_BASE: &str = "1/1000";
const AUDIO_TEMP: Container = Container {
	cover: Cover::Attachment,
	..MATROSKA_AUDIO
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EncodedVideo {
	pub frames: u64,
	pub decoded_s: f64,
	pub decode_errors: usize,
}

pub type Sample = TrimWindow;

/// What encoding a set of samples gave.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sampled {
	pub bytes: u64,
	/// The median sample's encoding seconds per second of source (`NaN` for none).
	pub rate: f64,
}

/// The middle of `rates`, so that one slow start-up doesn't set it; `NaN` for none.
pub fn median_rate(mut rates: Vec<f64>) -> f64 {
	rates.retain(|rate| rate.is_finite());
	rates.sort_by(f64::total_cmp);
	rates.get(rates.len() / 2).copied().unwrap_or(f64::NAN)
}

#[derive(Clone, Copy)]
enum Run<'a> {
	Whole,
	Sample(&'a Sample),
	/// The main video's frames through the same chain, stored losslessly for an animation.
	Frames,
}

pub fn video_command(ffmpeg: &Path, source: &Path, output: &Path, plan: &VideoPlan) -> ToolCommand {
	encode_command(ffmpeg, source, output, plan, plan.threads, Run::Whole)
}

pub fn sample_command(ffmpeg: &Path, source: &Path, output: &Path, plan: &VideoPlan, sample: &Sample) -> ToolCommand {
	encode_command(ffmpeg, source, output, plan, plan.threads, Run::Sample(sample))
}

pub fn frames_command(ffmpeg: &Path, source: &Path, output: &Path, plan: &VideoPlan) -> ToolCommand {
	encode_command(ffmpeg, source, output, plan, plan.threads, Run::Frames)
}

/// `plan` encoded on `threads` in place of its own.
fn encode_command(
	ffmpeg: &Path,
	source: &Path,
	output: &Path,
	plan: &VideoPlan,
	threads: Option<Threads>,
	run: Run<'_>,
) -> ToolCommand {
	let sample = match run {
		Run::Sample(sample) => Some(sample),
		Run::Whole | Run::Frames => None,
	};
	let main_only = !matches!(run, Run::Whole);
	let workspace = output.parent().unwrap_or(Path::new(""));
	let mut cmd = ToolCommand::new(ffmpeg, STALL_TIMEOUT);
	if plan.subtitles.is_some() && !workspace.as_os_str().is_empty() {
		cmd.in_dir(workspace);
	}
	cmd.count_lines_containing(DECODE_ERRORS);
	cmd.args(["-hide_banner", "-nostdin", "-y", "-loglevel", "repeat+info"]);

	cmd.threads(threads.map(|threads| threads.tool));

	let start = if main_only { plan.starts.main } else { plan.starts.every };
	let cut = sample
		.map(|sample| (sample.start_s, sample.length_s))
		.or(plan.window.map(|window| (window.start_s, window.length_s)))
		.map(|(start_s, length_s)| start.seek(start_s, length_s));
	if let Some(cut) = cut {
		cmd.seek(cut.input_s, cut.input_length_s);
	}
	if let Some(offset) = start.offset_arg(cut.map(|cut| cut.input_s)) {
		cmd.arg("-itsoffset").arg(offset);
	}
	let seek_s = cut.map(|cut| cut.start_s);
	let trim = cut.and_then(Seek::trim_filter).unwrap_or_default();
	let shaped = burn::chained(&[&trim, &plan.shape]);

	cmd.input(plan.decoder, None, source);
	if let Some(overlay) = plan.overlay {
		cmd.args(overlay.input_args())
			.arg("-i")
			.arg(process::tool_path(&workspace.join(burn::BITMAP_FILE)));
	}

	let streams = if main_only {
		plan.streams.get(..1).unwrap_or_default()
	} else {
		plan.streams.as_slice()
	};
	let graphed = if let Some(overlay) = plan.overlay {
		cmd.arg("-filter_complex").arg(burn::overlay_graph(
			(&format!("[0:{}]", streams[0]), &shaped),
			"[1:0]",
			overlay,
			seek_s,
			&burn::main_chain("", plan.subtitles.as_deref(), &plan.filters, seek_s),
			"[burned]",
		));
		cmd.args(["-map", "[burned]"]);
		1
	} else {
		0
	};
	for index in &streams[graphed..] {
		cmd.arg("-map").arg(format!("0:{index}"));
	}
	let main = burn::main_chain(&shaped, plan.subtitles.as_deref(), &plan.filters, seek_s);
	let others = burn::main_chain(&shaped, None, &plan.filters, None);
	for ordinal in graphed..streams.len() {
		let chain = if ordinal == 0 { &main } else { &others };
		cmd.arg(format!("-filter:v:{ordinal}")).arg(chain);
	}

	if matches!(run, Run::Frames) {
		cmd.args(["-c:v", "ffv1", "-level", "3"]);
	} else {
		encoder_args(&mut cmd, &plan.encoder, threads, plan.max_bitrate_kbps);
	}

	// On Matroska's millisecond clock: the encoder's default, one tick per frame at the declared rate, would put the
	// frames of a variable-rate video on the same times.
	cmd.args([
		"-enc_time_base:v",
		MATROSKA_TIME_BASE,
		"-fps_mode",
		"passthrough",
		"-f",
		"matroska",
	])
	.arg(process::tool_path(output));
	cmd
}

fn encoder_args(cmd: &mut ToolCommand, encoder: &Encoder, threads: Option<Threads>, max_kbps: Option<u32>) {
	match encoder {
		Encoder::SvtAv1 {
			preset,
			crf,
			film_grain,
		} => {
			cmd.args(["-c:v", "libsvtav1", "-preset"])
				.arg(preset.to_string())
				.args(["-err_detect", "explode", "-dolbyvision", "0", "-svtav1-params"])
				.arg(svt_params(
					*crf,
					*film_grain,
					threads.map(|threads| threads.svt_lp),
					max_kbps,
				));
		}
		Encoder::Aom { cpu_used, crf } => {
			cmd.args(["-c:v", "libaom-av1", "-crf"])
				.arg(crf.to_string())
				.arg("-b:v")
				.arg(video_rate(max_kbps))
				.arg("-cpu-used")
				.arg(cpu_used.to_string())
				.args(["-row-mt", "1"]);
		}
		Encoder::Vp9Alpha { cpu_used, crf } => {
			cmd.args(["-c:v", "libvpx-vp9", "-crf"])
				.arg(crf.to_string())
				.arg("-b:v")
				.arg(video_rate(max_kbps))
				.args(["-row-mt", "1", "-deadline", "good", "-cpu-used"])
				.arg(cpu_used.to_string());
		}
	}

	if let (Some(threads), Encoder::Aom { .. } | Encoder::Vp9Alpha { .. }) = (threads, encoder) {
		cmd.arg("-threads").arg(threads.tool.to_string());
	}
}

/// `-b:v` for libaom-av1 and libvpx-vp9: 0 is plain CRF, a bitrate with `-crf` is constrained quality.
fn video_rate(max_kbps: Option<u32>) -> String {
	max_kbps.map_or_else(|| "0".to_owned(), |kbps| format!("{kbps}k"))
}

pub fn samples(declared_s: Option<f64>) -> Vec<Sample> {
	let Some(duration) = declared_s.filter(|&duration| duration >= MIN_PREDICTED_DURATION_S) else {
		return Vec::new();
	};

	analyze::segments(Some(duration), analyze::DEFAULT_SEGMENTS)
		.into_iter()
		.map(|segment| {
			segment.length_s.map(|length_s| Sample {
				start_s: segment.start_s,
				length_s,
			})
		})
		.collect::<Option<Vec<_>>>()
		.unwrap_or_default()
}

/// Encodes `sample` of `plan` on `threads` in place of the plan's own.
pub async fn encode_sample(
	ffmpeg: &Path,
	source: &Path,
	output: &Path,
	plan: &VideoPlan,
	threads: Option<Threads>,
	sample: &Sample,
	cancel: &CancellationToken,
) -> Result<u64, JobError> {
	let cmd = encode_command(ffmpeg, source, output, plan, threads, Run::Sample(sample));
	let out = process::run(&cmd, StdoutMode::Progress, |_| {}, cancel).await?;
	if !out.completed() {
		return Err(failed_encode(out));
	}

	Ok(fs::metadata(output)
		.map_err(|err| JobError::io(IoSide::Destination(output), &err))?
		.len())
}

pub async fn encode_samples(
	ffmpeg: &Path,
	source: &Path,
	workspace: &Workspace,
	plan: &VideoPlan,
	samples: &[Sample],
	mut on_samples: impl FnMut(usize),
	cancel: &CancellationToken,
) -> Result<Sampled, JobError> {
	let mut bytes = 0;
	let mut rates = Vec::new();
	on_samples(0);

	for (index, sample) in samples.iter().enumerate() {
		let output = workspace.file(&format!("sample-{index}.mkv"));
		let started = Instant::now();
		bytes += encode_sample(ffmpeg, source, &output, plan, plan.threads, sample, cancel).await?;
		if sample.length_s > 0.0 {
			rates.push(started.elapsed().as_secs_f64() / sample.length_s);
		}
		on_samples(index + 1);
	}

	Ok(Sampled {
		bytes,
		rate: median_rate(rates),
	})
}

pub(crate) fn failed_encode(out: process::ToolOutput) -> JobError {
	let kind = if out.counted_lines > 0 {
		ErrorKind::SourceDamaged(format!(
			"the video cannot be decoded ({} decoding errors)",
			out.counted_lines
		))
	} else {
		ErrorKind::EncodeFailed(process::failure_reason(&out.stderr_tail))
	};
	JobError::with_log(kind, out.stderr_tail)
}

pub fn other_bitrate(info: &MediaInfo, streams: &StreamPlan) -> u64 {
	let source_rate = |index: u32| {
		info.streams
			.iter()
			.find(|stream| stream.index == index)
			.and_then(stream_bitrate)
			.unwrap_or(0)
	};

	let audio: u64 = streams
		.audio
		.iter()
		.map(|track| match &track.plan {
			TrackPlan::Encode(plan) => u64::from(plan.bitrate_kbps) * 1000,
			TrackPlan::Copy => source_rate(track.stream_index),
		})
		.sum();
	let subtitles: u64 = streams
		.subtitles
		.iter()
		.map(|track| source_rate(track.stream_index))
		.sum();

	audio + subtitles
}

fn stream_bitrate(stream: &Stream) -> Option<u64> {
	stream
		.bit_rate
		.or_else(|| stream.tag("BPS").or_else(|| stream.tag("BPS-eng"))?.parse().ok())
}

#[allow(
	clippy::cast_precision_loss,
	clippy::cast_possible_truncation,
	clippy::cast_sign_loss
)]
pub fn predicted_bytes(sample_bytes: u64, samples: &[Sample], duration_s: f64, other_bits_per_s: u64) -> u64 {
	let total_s: f64 = samples.iter().map(|sample| sample.length_s).sum();
	let video = sample_bytes as f64 / total_s * duration_s;
	let other = other_bits_per_s as f64 * duration_s / 8.0;
	(video + other).round() as u64
}

#[allow(clippy::cast_precision_loss)]
pub fn predicted_keep(src_bytes: u64, predicted_bytes: u64, min_savings: f64) -> Option<KeepReason> {
	let limit = src_bytes as f64 * (1.0 - min_savings + PREDICTION_MARGIN);
	(predicted_bytes as f64 >= limit).then_some(KeepReason::PredictedNotSmaller {
		src_bytes,
		predicted_bytes,
	})
}

/// The video bytes a full encode at `crf` should come to: the search probes' sample bytes at their CRFs
/// (`probe_sizes`), interpolated in log size to `crf`, scaled by how the first full encode (`first`: its CRF and bytes)
/// came out against the same interpolation at its own CRF. `None` without two probes at different CRFs, for a `crf`
/// more than `MAX_SIZE_EXTRAPOLATION` below the lowest probe, or beyond the probes when the two nearest are less than
/// `MIN_EXTRAPOLATION_BASE` apart: the retry then runs.
#[allow(
	clippy::cast_precision_loss,
	clippy::cast_possible_truncation,
	clippy::cast_sign_loss
)]
pub fn retry_bytes(probe_sizes: &[(f64, u64)], (first_crf, first_bytes): (f64, u64), crf: f64) -> Option<u64> {
	let mut points: Vec<(f64, f64)> = probe_sizes
		.iter()
		.filter(|(_, bytes)| *bytes > 0)
		.map(|&(probe_crf, bytes)| (probe_crf, (bytes as f64).ln()))
		.collect();
	points.sort_by(|a, b| a.0.total_cmp(&b.0));
	points.dedup_by(|a, b| (a.0 - b.0).abs() < f64::EPSILON);
	if points.len() < 2 || first_bytes == 0 {
		return None;
	}

	let (lowest, highest) = (points[0].0, points[points.len() - 1].0);
	let span = |pair: &[(f64, f64)]| pair[1].0 - pair[0].0;
	let extrapolated = (crf < lowest && span(&points[..2]) < MIN_EXTRAPOLATION_BASE)
		|| (crf > highest && span(&points[points.len() - 2..]) < MIN_EXTRAPOLATION_BASE);
	if crf < lowest - MAX_SIZE_EXTRAPOLATION || extrapolated {
		return None;
	}

	let log_bytes = |at: f64| {
		let after = points
			.iter()
			.position(|point| point.0 > at)
			.unwrap_or(points.len() - 1)
			.max(1);
		let ((low_crf, low), (high_crf, high)) = (points[after - 1], points[after]);
		low + (high - low) * (at - low_crf) / (high_crf - low_crf)
	};
	let scale = first_bytes as f64 / log_bytes(first_crf).exp();
	Some((log_bytes(crf).exp() * scale).round() as u64)
}

/// The original is kept without a retry predicted at `predicted_bytes`, when that already misses the size rule against
/// `src_bytes` by `RETRY_SIZE_SLACK`: the prediction comes from samples, so only a clear miss counts.
#[allow(clippy::cast_precision_loss)]
pub fn retry_keep(src_bytes: u64, predicted_bytes: u64, min_savings: f64) -> Option<KeepReason> {
	let limit = src_bytes as f64 * (1.0 - min_savings);
	(predicted_bytes as f64 >= limit * RETRY_SIZE_SLACK).then_some(KeepReason::PredictedNotSmaller {
		src_bytes,
		predicted_bytes,
	})
}

#[allow(clippy::cast_precision_loss)]
pub async fn encode_video(
	cmd: &ToolCommand,
	plan: &VideoPlan,
	declared_s: Option<f64>,
	mut on_fraction: impl FnMut(f64),
	cancel: &CancellationToken,
) -> Result<EncodedVideo, JobError> {
	let out = process::run(
		cmd,
		StdoutMode::Progress,
		|progress| {
			let fraction = match (plan.expected_frames, progress.frame, declared_s, progress.out_time) {
				(Some(expected), Some(frame), _, _) if expected > 0 => frame as f64 / expected as f64,
				(_, _, Some(total), Some(done)) if total > 0.0 => done.as_secs_f64() / total,
				_ => return,
			};
			on_fraction(fraction.clamp(0.0, 1.0));
		},
		cancel,
	)
	.await?;

	if !out.completed() {
		return Err(failed_encode(out));
	}

	let last = out.last_progress.unwrap_or_default();
	Ok(EncodedVideo {
		frames: last.frame.unwrap_or(0),
		decoded_s: last.out_time.map_or(0.0, |time| time.as_secs_f64()),
		decode_errors: out.counted_lines,
	})
}

/// What a video temp's check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TempCheck {
	Passed(Option<String>),
	/// Fewer frames than the plan expects, and nothing else wrong: only the source's decoded frames tell whether it
	/// holds packets that don't decode.
	FewerFrames {
		expected: u64,
	},
}

/// `inspect_video_temp`, with fewer frames than expected as a failure.
pub fn check_video_temp(
	plan: &VideoPlan,
	encoded: &EncodedVideo,
	declared_s: Option<f64>,
	start_s: f64,
	fps: Option<f64>,
) -> Result<Option<String>, JobError> {
	match inspect_video_temp(plan, encoded, declared_s, start_s, fps)? {
		TempCheck::Passed(warning) => Ok(warning),
		TempCheck::FewerFrames { expected } => Err(frame_count_error(encoded, expected)),
	}
}

/// `fps` is the video temp's declared rate.
pub fn inspect_video_temp(
	plan: &VideoPlan,
	encoded: &EncodedVideo,
	declared_s: Option<f64>,
	start_s: f64,
	fps: Option<f64>,
) -> Result<TempCheck, JobError> {
	if let Some(declared) = declared_s
		&& declared - encoded.decoded_s > (declared * DAMAGE_SHARE).max(DAMAGE_FLOOR_S.max(frame_s(fps)) + ROUNDING_S)
	{
		return Err(ErrorKind::SourceDamaged(format!(
			"the video decodes to {:.1} s but declares {declared:.1} s",
			encoded.decoded_s
		))
		.into());
	}

	if encoded.frames == 0 {
		return Err(ErrorKind::EncodeFailed("no video frames were encoded".into()).into());
	}

	if encoded.decode_errors > 0 {
		return Ok(TempCheck::Passed(Some(format!(
			"the source has {} decoding errors; glitches are carried over as they are",
			encoded.decode_errors
		))));
	}

	match (plan.expected_frames, declared_s) {
		(Some(expected), _) if encoded.frames + plan.frame_tolerance < expected => {
			Ok(TempCheck::FewerFrames { expected })
		}
		(Some(expected), _) if encoded.frames.abs_diff(expected) > plan.frame_tolerance => {
			Err(frame_count_error(encoded, expected))
		}
		(None, Some(declared)) if !lasts(plan, encoded, (start_s, declared), fps) => {
			Err(ErrorKind::EncodeFailed(format!(
				"the video lasts {:.2} s, {declared:.2} s expected",
				encoded.decoded_s - start_s
			))
			.into())
		}
		_ => Ok(TempCheck::Passed(None)),
	}
}

/// An encode with fewer frames than the `expected` is kept when the source's main video itself decodes to about as
/// many: `decoded` frames, counted by decoding only, which the plan's shape turns into frames as `scan` says.
pub fn undecodable_frames(
	plan: &VideoPlan,
	encoded: &EncodedVideo,
	expected: u64,
	decoded: u64,
	scan: ScanType,
) -> Result<String, JobError> {
	let decoded = scanned_frames(decoded, scan);
	if encoded.frames.abs_diff(decoded) > plan.frame_tolerance {
		return Err(frame_count_error(encoded, expected));
	}

	Ok(format!(
		"the source has {} video frames that don't decode; the {} that do were kept",
		expected.saturating_sub(decoded),
		encoded.frames
	))
}

fn frame_count_error(encoded: &EncodedVideo, expected: u64) -> JobError {
	ErrorKind::EncodeFailed(format!("the video has {} frames, {expected} expected", encoded.frames)).into()
}

/// Decodes only the main video of `plan` from `source`, on the plan's decoder and threads, to count its frames.
pub fn decode_command(ffmpeg: &Path, source: &Path, plan: &VideoPlan) -> ToolCommand {
	let mut cmd = ToolCommand::new(ffmpeg, STALL_TIMEOUT);
	cmd.args(["-hide_banner", "-nostdin"])
		.input(plan.decoder, plan.threads.map(|threads| threads.tool), source)
		.arg("-map")
		.arg(format!("0:{}", plan.streams[0]))
		.args(["-fps_mode", "passthrough", "-f", "null", "-"]);
	cmd
}

/// The frames the main video of `plan` decodes to from `source`.
pub async fn decoded_frames(
	ffmpeg: &Path,
	source: &Path,
	plan: &VideoPlan,
	cancel: &CancellationToken,
) -> Result<u64, JobError> {
	let out = run_ffmpeg(&decode_command(ffmpeg, source, plan), |_| {}, cancel).await?;
	Ok(out.last_progress.and_then(|progress| progress.frame).unwrap_or(0))
}

// A declared duration is a length in MP4 but the end timestamp in Matroska's DURATION tag, and the video temp keeps
// the source's start offset, so either reading may be the one that matches. A TS/PS video's declared length counts
// from its first packet, which the whole file's `-itsoffset` moved later on the temp, and which may not decode.
// The decoded time is the last frame's start, a frame short of the declared end.
fn lasts(plan: &VideoPlan, encoded: &EncodedVideo, (start_s, declared_s): (f64, f64), fps: Option<f64>) -> bool {
	let offset_s = plan
		.window
		.map_or_else(|| plan.starts.every.offset_s(None), |_| None)
		.unwrap_or(0.0);
	let tolerance_s = DURATION_TOLERANCE_S.max(frame_s(fps)) + ROUNDING_S;

	[
		encoded.decoded_s - start_s,
		encoded.decoded_s,
		encoded.decoded_s - offset_s,
	]
	.iter()
	.any(|decoded| (decoded - declared_s).abs() <= tolerance_s)
}

pub fn frame_s(fps: Option<f64>) -> f64 {
	fps.filter(|fps| *fps > 0.0).map_or(0.0, |fps| 1.0 / fps)
}

pub fn audio_plan(info: &MediaInfo, streams: &StreamPlan, window: Option<TrimWindow>) -> Option<FilePlan> {
	let tracks = streams.encoded_audio();
	let read: Vec<u32> = tracks
		.iter()
		.map(|track| track.stream_index)
		.chain(streams.merge_from.iter().copied())
		.collect();
	let start = InputStart::of(info, &read);
	let cut = window.map(|window| start.seek(window.start_s, window.length_s));

	(!tracks.is_empty()).then_some(FilePlan {
		container: AUDIO_TEMP,
		tracks,
		options: FileOptions {
			window: cut.map(|cut| TrimWindow {
				start_s: cut.input_s,
				length_s: cut.input_length_s,
			}),
			kept: cut.and_then(|cut| cut.kept),
			delay_s: start.offset_s(cut.map(|cut| cut.input_s)),
			merge_from: streams.merge_from.clone(),
			merge_delays_s: merge_delays(info, &streams.merge_from, start.at(cut.map_or(0.0, |cut| cut.input_s))),
			merge_language: streams.merge_language.clone(),
			..FileOptions::default()
		},
	})
}

pub async fn extract(
	ffmpeg: &Path,
	source: &Path,
	workspace: &Workspace,
	extracted: &[Extracted],
	cancel: &CancellationToken,
) -> Result<(), JobError> {
	if extracted.is_empty() {
		return Ok(());
	}

	let source_bytes = fs::metadata(source).map_or(0, |meta| meta.len());
	let mut cmd = ToolCommand::new(ffmpeg, process::scaled_timeout(source_bytes));
	cmd.args(["-hide_banner", "-nostdin", "-y", "-i"])
		.arg(process::tool_path(source));
	for stream in extracted {
		cmd.arg("-map")
			.arg(format!("0:{}", stream.stream_index))
			.args(["-c", "copy", "-f", "data"])
			.arg(process::tool_path(&workspace.file(&stream.work_file())));
	}

	run_ffmpeg(&cmd, |_| {}, cancel).await.map(drop)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::audio::plan::{EncodePlan, LayoutFix, TrackJob};
	use crate::video::burn::Overlay;
	use crate::video::plan::{test_plan, threads_for};
	use crate::video::streams::SubtitleTrack;
	use crate::video::{InputStart, VideoStarts, joined};

	fn plan(encoder: Encoder, expected_frames: Option<u64>) -> VideoPlan {
		VideoPlan {
			streams: vec![0, 2],
			filters: "format=yuv420p10le,setparams=field_mode=prog".into(),
			encoder,
			expected_frames,
			..test_plan()
		}
	}

	fn svt() -> Encoder {
		Encoder::SvtAv1 {
			preset: 8,
			crf: 32.0,
			film_grain: None,
		}
	}

	fn opus(stream_index: u32, bitrate_kbps: u32) -> TrackJob {
		TrackJob {
			stream_index,
			sample_rate: 48_000,
			plan: TrackPlan::Encode(EncodePlan {
				bitrate_kbps,
				channels: 2,
				layout: LayoutFix::Keep("stereo"),
			}),
		}
	}

	fn encoded(frames: u64, decoded_s: f64, decode_errors: usize) -> EncodedVideo {
		EncodedVideo {
			frames,
			decoded_s,
			decode_errors,
		}
	}

	#[test]
	fn svt_commands_map_every_video_stream_with_the_same_chain() {
		let cmd = video_command(
			Path::new("ffmpeg"),
			Path::new("in.mp4"),
			Path::new("video.a1.mkv"),
			&plan(svt(), None),
		);

		let args = joined(&cmd);
		assert!(
			args.starts_with("-hide_banner -nostdin -y -loglevel repeat+info -i "),
			"{args}"
		);
		assert!(
			args.contains(
				"-map 0:0 -map 0:2 -filter:v:0 format=yuv420p10le,setparams=field_mode=prog \
				 -filter:v:1 format=yuv420p10le,setparams=field_mode=prog"
			),
			"{args}"
		);
		assert!(
			args.contains(
				"-c:v libsvtav1 -preset 8 -err_detect explode -dolbyvision 0 -svtav1-params tune=0:keyint=5s:crf=32"
			),
			"{args}"
		);
		assert!(!args.contains("variance-boost"), "{args}");
		assert!(args.contains("-fps_mode passthrough -f matroska"), "{args}");
		assert_eq!(cmd.counted, DECODE_ERRORS);
	}

	#[test]
	fn a_thread_budget_reaches_the_decoder_the_filters_and_svt() {
		let budgeted = VideoPlan {
			threads: Some(Threads { tool: 4, svt_lp: 2 }),
			..plan(svt(), None)
		};

		let args = joined(&video_command(
			Path::new("ffmpeg"),
			Path::new("in.mp4"),
			Path::new("v.mkv"),
			&budgeted,
		));

		assert!(
			args.contains("-threads 4 -filter_threads 4 -filter_complex_threads 4 -i "),
			"{args}"
		);
		assert!(args.contains("-svtav1-params tune=0:keyint=5s:crf=32:lp=2"), "{args}");
	}

	#[test]
	fn six_threads_run_svt_at_a_level_that_fits_them_and_no_budget_leaves_svt_its_own() {
		let command = |threads: Option<Threads>| {
			joined(&video_command(
				Path::new("ffmpeg"),
				Path::new("in.mp4"),
				Path::new("v.mkv"),
				&VideoPlan {
					threads,
					..plan(svt(), None)
				},
			))
		};

		let six = command(Some(threads_for(6)));
		let free = command(None);

		assert!(
			six.contains("-threads 6 -filter_threads 6 -filter_complex_threads 6 -i "),
			"{six}"
		);
		assert!(six.contains("-svtav1-params tune=0:keyint=5s:crf=32:lp=2 "), "{six}");
		assert!(!free.contains("threads") && !free.contains(":lp="), "{free}");
	}

	#[test]
	fn a_burned_subtitle_graph_runs_on_the_thread_budget() {
		let burned = VideoPlan {
			threads: Some(Threads { tool: 3, svt_lp: 1 }),
			overlay: Some(Overlay {
				offset_s: 0.0,
				canvas: None,
				ten_bit: false,
				in_frame_format: false,
			}),
			..plan(svt(), None)
		};
		let unbudgeted = VideoPlan {
			threads: None,
			..burned.clone()
		};

		let args = joined(&video_command(
			Path::new("ffmpeg"),
			Path::new("in.mkv"),
			Path::new("work/v.mkv"),
			&burned,
		));
		let free = joined(&video_command(
			Path::new("ffmpeg"),
			Path::new("in.mkv"),
			Path::new("work/v.mkv"),
			&unbudgeted,
		));

		let complex = args.find("-filter_complex ").unwrap();
		assert!(args[..complex].contains("-filter_complex_threads 3 "), "{args}");
		assert!(!free.contains("threads"), "{free}");
	}

	#[test]
	fn animation_frames_are_the_main_video_through_the_same_chain_stored_losslessly() {
		let animated = VideoPlan {
			shape: "bwdif=mode=send_field:parity=tff:deint=all".into(),
			subtitles: Some("subtitles=subs.ass".into()),
			filters: "format=gbrp16le".into(),
			window: Some(TrimWindow {
				start_s: 4.0,
				length_s: 2.0,
			}),
			starts: VideoStarts {
				main: InputStart {
					file_s: 1.4,
					streams_s: Some(1.75),
				},
				every: InputStart {
					file_s: 1.4,
					streams_s: Some(1.7),
				},
			},
			..plan(svt(), None)
		};

		let cmd = frames_command(
			Path::new("ffmpeg"),
			Path::new("in.ts"),
			Path::new("work/frames.mkv"),
			&animated,
		);

		let args = joined(&cmd);
		assert_eq!(cmd.current_dir.as_deref(), Some(Path::new("work")));
		assert!(args.contains(" -ss 0.000 -t 6.000 -itsoffset 0.350000 -i "), "{args}");
		assert!(
			args.contains(
				"-map 0:0 -filter:v:0 trim=start=4.000000:end=6.000000,setpts=PTS-4.000000/TB,\
				 bwdif=mode=send_field:parity=tff:deint=all,settb=AVTB,setpts=PTS+4.000/TB,subtitles=subs.ass,\
				 setpts=PTS-4.000/TB,format=gbrp16le -c:v ffv1 -level 3 -enc_time_base:v 1/1000 -fps_mode passthrough \
				 -f matroska "
			),
			"{args}"
		);
		assert!(!args.contains("0:2") && !args.contains("libsvtav1"), "{args}");
		assert_eq!(cmd.counted, DECODE_ERRORS);
	}

	#[test]
	fn bitmap_subtitles_are_overlaid_from_their_own_input_in_a_complex_graph() {
		let burned = VideoPlan {
			starts: VideoStarts {
				main: InputStart {
					file_s: 1.4,
					streams_s: Some(1.75),
				},
				every: InputStart {
					file_s: 1.4,
					streams_s: Some(1.7),
				},
			},
			overlay: Some(burn::Overlay {
				offset_s: 5.3,
				canvas: None,
				ten_bit: false,
				in_frame_format: false,
			}),
			window: Some(TrimWindow {
				start_s: 4.0,
				length_s: 4.0,
			}),
			..plan(svt(), None)
		};
		let sample = Sample {
			start_s: 0.2,
			length_s: 1.0,
		};

		let cmd = video_command(
			Path::new("ffmpeg"),
			Path::new("in.mkv"),
			Path::new("work/video.a1.mkv"),
			&burned,
		);
		let sampled = joined(&sample_command(
			Path::new("ffmpeg"),
			Path::new("in.mkv"),
			Path::new("work/sample-0.mkv"),
			&burned,
			&sample,
		));
		let whole = joined(&video_command(
			Path::new("ffmpeg"),
			Path::new("in.mkv"),
			Path::new("work/video.a1.mkv"),
			&VideoPlan {
				window: None,
				..burned.clone()
			},
		));

		let args = joined(&cmd);
		assert_eq!(cmd.current_dir, None);
		assert!(args.contains(" -ss 0.000 -t 8.000 -itsoffset 0.300000 -i "), "{args}");
		assert!(
			sampled.contains(" -ss 0.000 -t 1.200 -itsoffset 0.350000 -i "),
			"{sampled}"
		);
		assert!(whole.contains("repeat+info -itsoffset 0.300000 -i "), "{whole}");
		for command in [&args, &sampled, &whole] {
			assert!(command.contains(" -itsoffset 5.300000 -i "), "{command}");
		}
		assert!(args.contains("subs.mkv -filter_complex "), "{args}");
		assert!(
			args.contains(
				"-filter_complex [0:0]trim=start=4.000000:end=8.000000,setpts=PTS-4.000000/TB,settb=AVTB,\
				 setpts=PTS+4.000/TB[bv];[1:0]setpts=PTS-0.002/TB[bs];\
				 [bs][bv]scale2ref[bss][bvs];[bss]null[bsd];[bvs][bsd]overlay=eof_action=pass,setpts=PTS-4.000/TB,\
				 format=yuv420p10le,setparams=field_mode=prog[burned] -map [burned] -map 0:2 \
				 -filter:v:1 trim=start=4.000000:end=8.000000,setpts=PTS-4.000000/TB,format=yuv420p10le,"
			),
			"{args}"
		);
		assert!(
			sampled.contains("[0:0]trim=start=0.200000:end=1.200000,setpts=PTS-0.200000/TB,settb=AVTB,"),
			"{sampled}"
		);
		assert!(!whole.contains("trim="), "{whole}");
	}

	#[test]
	fn a_matroska_trim_is_seeked_exactly_and_not_cut_again() {
		let trimmed = VideoPlan {
			window: Some(TrimWindow {
				start_s: 4.0,
				length_s: 4.0,
			}),
			..plan(svt(), None)
		};

		let args = joined(&video_command(
			Path::new("ffmpeg"),
			Path::new("in.mkv"),
			Path::new("work/video.a1.mkv"),
			&trimmed,
		));

		assert!(args.contains("repeat+info -ss 4.000 -t 4.000 -i "), "{args}");
		assert!(!args.contains("trim=") && !args.contains("-itsoffset"), "{args}");
	}

	#[test]
	fn only_a_text_burn_runs_in_the_workspace() {
		let text = VideoPlan {
			subtitles: Some("subtitles=subs.ass".into()),
			..plan(svt(), None)
		};

		let unburned = video_command(
			Path::new("ffmpeg"),
			Path::new("in.mkv"),
			Path::new("work/video.a1.mkv"),
			&plan(svt(), None),
		);
		let burned = video_command(
			Path::new("ffmpeg"),
			Path::new("in.mkv"),
			Path::new("work/video.a1.mkv"),
			&text,
		);

		assert_eq!(unburned.current_dir, None);
		assert_eq!(burned.current_dir.as_deref(), Some(Path::new("work")));
	}

	#[test]
	fn burned_text_goes_on_the_main_stream_only_and_follows_each_seek() {
		let burned = VideoPlan {
			subtitles: Some("subtitles=subs.ass".into()),
			window: Some(TrimWindow {
				start_s: 4.0,
				length_s: 20.0,
			}),
			..plan(svt(), None)
		};
		let sample = Sample {
			start_s: 12.5,
			length_s: 3.0,
		};

		let full = video_command(
			Path::new("ffmpeg"),
			Path::new("in.mkv"),
			Path::new("work/video.a1.mkv"),
			&burned,
		);
		let sampled = joined(&sample_command(
			Path::new("ffmpeg"),
			Path::new("in.mkv"),
			Path::new("work/sample-0.mkv"),
			&burned,
			&sample,
		));

		let args = joined(&full);
		assert_eq!(full.current_dir.as_deref(), Some(Path::new("work")));
		assert!(
			args.contains(
				"-filter:v:0 settb=AVTB,setpts=PTS+4.000/TB,subtitles=subs.ass,setpts=PTS-4.000/TB,format=yuv420p10le,\
				 setparams=field_mode=prog -filter:v:1 format=yuv420p10le,setparams=field_mode=prog -c:v "
			),
			"{args}"
		);
		assert!(
			sampled.contains(
				"-filter:v:0 settb=AVTB,setpts=PTS+12.500/TB,subtitles=subs.ass,setpts=PTS-12.500/TB,format=yuv420p10le,"
			),
			"{sampled}"
		);
	}

	#[test]
	fn the_shape_runs_after_the_cut_and_before_burned_subtitles_on_every_stream() {
		let shaped = |subtitles: Option<&str>, overlay: Option<burn::Overlay>| VideoPlan {
			shape: "bwdif=mode=send_field:parity=tff:deint=all,scale=w=640:h=480,setsar=1".into(),
			subtitles: subtitles.map(str::to_owned),
			overlay,
			window: Some(TrimWindow {
				start_s: 4.0,
				length_s: 4.0,
			}),
			starts: VideoStarts {
				every: InputStart {
					file_s: 1.4,
					streams_s: Some(1.7),
				},
				..VideoStarts::default()
			},
			..plan(svt(), None)
		};
		let args = |plan: &VideoPlan| {
			joined(&video_command(
				Path::new("ffmpeg"),
				Path::new("in.mkv"),
				Path::new("work/video.a1.mkv"),
				plan,
			))
		};

		let text = args(&shaped(Some("subtitles=subs.ass"), None));
		let bitmap = args(&shaped(
			None,
			Some(burn::Overlay {
				offset_s: 0.0,
				canvas: None,
				ten_bit: false,
				in_frame_format: false,
			}),
		));

		assert!(
			text.contains(
				"-filter:v:0 trim=start=4.000000:end=8.000000,setpts=PTS-4.000000/TB,\
				 bwdif=mode=send_field:parity=tff:deint=all,scale=w=640:h=480,setsar=1,settb=AVTB,setpts=PTS+4.000/TB,\
				 subtitles=subs.ass,setpts=PTS-4.000/TB,format=yuv420p10le,setparams=field_mode=prog \
				 -filter:v:1 trim=start=4.000000:end=8.000000,setpts=PTS-4.000000/TB,\
				 bwdif=mode=send_field:parity=tff:deint=all,scale=w=640:h=480,setsar=1,format=yuv420p10le,"
			),
			"{text}"
		);
		assert!(
			bitmap.contains(
				"-filter_complex [0:0]trim=start=4.000000:end=8.000000,setpts=PTS-4.000000/TB,\
				 bwdif=mode=send_field:parity=tff:deint=all,scale=w=640:h=480,setsar=1,settb=AVTB,setpts=PTS+4.000/TB[bv];\
				 [1:0]setpts=PTS-0.002/TB[bs];[bs][bv]scale2ref[bss][bvs];"
			),
			"{bitmap}"
		);
	}

	#[test]
	fn film_grain_comes_with_denoise_only_when_planned() {
		let grainy = plan(
			Encoder::SvtAv1 {
				preset: 5,
				crf: 29.25,
				film_grain: Some(20),
			},
			None,
		);

		let args = joined(&video_command(
			Path::new("ffmpeg"),
			Path::new("in.mp4"),
			Path::new("v.mkv"),
			&grainy,
		));

		assert!(
			args.contains("-svtav1-params tune=0:keyint=5s:crf=29.25:film-grain=20:film-grain-denoise=1 "),
			"{args}"
		);
	}

	#[test]
	fn other_encoders_and_forced_decoders() {
		let mut alpha = plan(Encoder::Vp9Alpha { cpu_used: 2, crf: 33 }, None);
		alpha.decoder = Some("libvpx-vp9");

		let aom = joined(&video_command(
			Path::new("ffmpeg"),
			Path::new("wide.avi"),
			Path::new("v.mkv"),
			&plan(Encoder::Aom { cpu_used: 6, crf: 38 }, None),
		));
		let vp9 = joined(&video_command(
			Path::new("ffmpeg"),
			Path::new("a.webm"),
			Path::new("v.mkv"),
			&alpha,
		));

		assert!(
			aom.contains("-c:v libaom-av1 -crf 38 -b:v 0 -cpu-used 6 -row-mt 1"),
			"{aom}"
		);
		assert!(vp9.contains("-c:v libvpx-vp9 -i "), "{vp9}");
		assert!(
			vp9.contains("-c:v libvpx-vp9 -crf 33 -b:v 0 -row-mt 1 -deadline good -cpu-used 2"),
			"{vp9}"
		);
	}

	#[test]
	fn every_encode_keeps_the_frame_times_on_the_matroska_clock() {
		let sample = Sample {
			start_s: 12.5,
			length_s: 3.0,
		};
		let encoders = [
			svt(),
			Encoder::Aom { cpu_used: 6, crf: 38 },
			Encoder::Vp9Alpha { cpu_used: 2, crf: 33 },
		];

		for encoder in encoders {
			let video = plan(encoder, None);
			let whole = joined(&video_command(
				Path::new("ffmpeg"),
				Path::new("in.mkv"),
				Path::new("v.mkv"),
				&video,
			));
			let sampled = joined(&sample_command(
				Path::new("ffmpeg"),
				Path::new("in.mkv"),
				Path::new("s.mkv"),
				&video,
				&sample,
			));

			for args in [&whole, &sampled] {
				assert!(
					args.contains(" -enc_time_base:v 1/1000 -fps_mode passthrough -f matroska "),
					"{args}"
				);
			}
		}
	}

	#[test]
	fn a_bitrate_cap_turns_libaom_and_libvpx_into_constrained_quality() {
		let capped = |encoder| VideoPlan {
			encoder,
			max_bitrate_kbps: Some(12_500),
			..crate::video::plan::test_plan()
		};
		let args = |plan: &VideoPlan| {
			joined(&video_command(
				Path::new("ffmpeg"),
				Path::new("in.mp4"),
				Path::new("v.mkv"),
				plan,
			))
		};

		let aom = args(&capped(Encoder::Aom { cpu_used: 4, crf: 29 }));
		let vp9 = args(&capped(Encoder::Vp9Alpha { cpu_used: 2, crf: 31 }));
		let free = args(&crate::video::plan::test_plan());

		assert!(aom.contains("-c:v libaom-av1 -crf 29 -b:v 12500k"), "{aom}");
		assert!(vp9.contains("-c:v libvpx-vp9 -crf 31 -b:v 12500k"), "{vp9}");
		assert!(!free.contains("mbr="), "{free}");
	}

	#[test]
	fn samples_seek_the_source_and_encode_only_the_main_stream_like_the_full_encode() {
		let mut video = plan(svt(), None);
		video.decoder = Some("libvpx-vp9");
		let sample = Sample {
			start_s: 12.5,
			length_s: 3.0,
		};

		let full = joined(&video_command(
			Path::new("ffmpeg"),
			Path::new("in.webm"),
			Path::new("v.mkv"),
			&video,
		));
		let sampled = joined(&sample_command(
			Path::new("ffmpeg"),
			Path::new("in.webm"),
			Path::new("v.mkv"),
			&video,
			&sample,
		));

		assert!(
			sampled
				.starts_with("-hide_banner -nostdin -y -loglevel repeat+info -ss 12.500 -t 3.000 -c:v libvpx-vp9 -i "),
			"{sampled}"
		);
		assert!(
			sampled.contains(" -map 0:0 -filter:v:0 format=yuv420p10le,setparams=field_mode=prog -c:v libsvtav1 "),
			"{sampled}"
		);
		assert_eq!(sampled.matches("-map").count(), 1, "{sampled}");
		assert_eq!(
			sampled.rsplit_once("-c:v ").map(|(_, encoder)| encoder),
			full.rsplit_once("-c:v ").map(|(_, encoder)| encoder)
		);
	}

	#[test]
	fn only_long_files_with_a_known_duration_are_sampled() {
		let long = samples(Some(200.0));

		assert_eq!(long.len(), 4);
		assert!((long[0].start_s - 10.0).abs() < 1e-9, "{long:?}");
		assert!(long.iter().all(|sample| (sample.length_s - 3.0).abs() < 1e-9));
		assert_eq!(samples(Some(MIN_PREDICTED_DURATION_S)).len(), 4);
		assert!(samples(Some(119.9)).is_empty());
		assert!(samples(None).is_empty());
	}

	#[test]
	fn the_encoding_rate_is_the_median_samples() {
		assert!((median_rate(vec![9.0, 1.0, 1.2, f64::NAN, 1.1]) - 1.2).abs() < 1e-9);
		assert!((median_rate(vec![2.0]) - 2.0).abs() < 1e-9);
		assert!(median_rate(Vec::new()).is_nan());
	}

	#[test]
	fn the_prediction_scales_the_samples_and_adds_the_other_streams() {
		let info = MediaInfo::parse(
			br#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"},
			{"index":1,"codec_type":"audio","codec_name":"flac","bit_rate":"900000"},
			{"index":2,"codec_type":"audio","codec_name":"ac3","bit_rate":"64000"},
			{"index":3,"codec_type":"subtitle","codec_name":"subrip","tags":{"BPS":"800"}},
			{"index":4,"codec_type":"subtitle","codec_name":"ass"}]}"#,
		)
		.unwrap();
		let streams = StreamPlan {
			audio: vec![
				opus(1, 128),
				TrackJob {
					stream_index: 2,
					sample_rate: 48_000,
					plan: TrackPlan::Copy,
				},
			],
			subtitles: vec![
				SubtitleTrack {
					stream_index: 3,
					codec: None,
					clipped: None,
				},
				SubtitleTrack {
					stream_index: 4,
					codec: None,
					clipped: None,
				},
			],
			..StreamPlan::default()
		};

		let other = other_bitrate(&info, &streams);

		assert_eq!(other, 128_000 + 64_000 + 800);
		assert_eq!(
			predicted_bytes(120_000, &samples(Some(240.0)), 240.0, other),
			2_400_000 + 5_784_000
		);
		assert_eq!(predicted_bytes(120_000, &samples(Some(240.0)), 240.0, 0), 2_400_000);
	}

	#[test]
	fn a_prediction_within_five_points_of_the_savings_target_keeps_the_original() {
		let kept = |predicted_bytes: u64, min_savings: f64| predicted_keep(1_000_000, predicted_bytes, min_savings);

		assert_eq!(
			kept(950_001, 0.10),
			Some(KeepReason::PredictedNotSmaller {
				src_bytes: 1_000_000,
				predicted_bytes: 950_001,
			})
		);
		assert_eq!(kept(949_999, 0.10), None);
		assert!(kept(1_050_001, 0.0).is_some());
		assert_eq!(kept(1_049_999, 0.0), None);
	}

	/// Sample bytes doubling every 6 CRF down from 1 MB at CRF 36.
	fn doubling(crf: f64) -> u64 {
		#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
		let bytes = (1_000_000.0 * 2f64.powf((36.0 - crf) / 6.0)).round() as u64;
		bytes
	}

	#[allow(
		clippy::cast_precision_loss,
		clippy::cast_possible_truncation,
		clippy::cast_sign_loss
	)]
	fn scaled(bytes: u64, crfs: f64) -> u64 {
		(bytes as f64 * 2f64.powf(crfs / 6.0)).round() as u64
	}

	#[test]
	fn a_retry_is_predicted_from_the_probes_in_log_size_and_the_first_encode() {
		let probes: Vec<(f64, u64)> = [28.0, 36.0, 37.0, 36.75].map(|crf| (crf, doubling(crf))).into();
		let near = |a: u64, b: u64| a.abs_diff(b) <= b / 10_000;

		let between = retry_bytes(&probes, (36.75, 19_710_000), 29.5).unwrap();
		let below = retry_bytes(&probes, (36.75, 19_710_000), 26.0).unwrap();
		let above = retry_bytes(&probes[..2], (36.0, 5_000_000), 40.0).unwrap();

		assert!(near(between, scaled(19_710_000, 7.25)), "{between}");
		assert!(
			near(below, scaled(19_710_000, 10.75)),
			"2 below the lowest probe: {below}"
		);
		assert!(
			near(above, scaled(5_000_000, -4.0)),
			"beyond the highest probe: {above}"
		);
		assert_eq!(retry_bytes(&probes[..1], (36.75, 19_710_000), 29.5), None);
		assert_eq!(retry_bytes(&[(30.0, 10), (30.0, 12)], (30.0, 100), 28.0), None);
		assert_eq!(retry_bytes(&probes, (36.75, 0), 29.5), None);
	}

	#[test]
	fn a_retry_size_is_not_guessed_far_beyond_the_probes_or_from_probes_close_together() {
		let spread: Vec<(f64, u64)> = [28.0, 36.0].map(|crf| (crf, doubling(crf))).into();
		let close: Vec<(f64, u64)> = [36.0, 37.0, 36.75].map(|crf| (crf, doubling(crf))).into();

		assert!(retry_bytes(&spread, (36.0, 5_000_000), 26.0).is_some());
		assert_eq!(
			retry_bytes(&spread, (36.0, 5_000_000), 25.75),
			None,
			"more than 2 below the lowest probe"
		);
		assert_eq!(
			retry_bytes(&close, (36.75, 5_000_000), 34.75),
			None,
			"the two lowest probes are 0.75 CRF apart"
		);
		assert_eq!(
			retry_bytes(&close, (36.0, 5_000_000), 38.0),
			None,
			"the two highest are 0.25 apart"
		);
		assert!(
			retry_bytes(&close, (36.75, 5_000_000), 36.5).is_some(),
			"between the probes"
		);
	}

	#[test]
	fn only_a_retry_clearly_past_the_size_rule_keeps_the_original() {
		let disc3 = retry_keep(32_040_000, scaled(20_290_000, 6.0), 0.10);
		let disc4 = retry_keep(31_730_000, scaled(19_710_000, 2.0), 0.10);

		assert!(
			matches!(
				disc3,
				Some(KeepReason::PredictedNotSmaller {
					src_bytes: 32_040_000,
					..
				})
			),
			"{disc3:?}"
		);
		assert_eq!(disc4, None);
		assert!(retry_keep(1_000_000, 990_001, 0.10).is_some());
		assert_eq!(
			retry_keep(1_000_000, 989_999, 0.10),
			None,
			"within the 10 % slack of 900 kB"
		);
		assert_eq!(retry_keep(1_000_000, 1_099_999, 0.0), None);
	}

	#[test]
	fn short_decodes_are_source_damage() {
		let video = plan(svt(), Some(505));

		let err = check_video_temp(&video, &encoded(291, 9.7, 0), Some(16.834), 0.0, None).unwrap_err();

		assert!(
			matches!(err.kind, ErrorKind::SourceDamaged(ref reason) if reason.contains("9.7")),
			"{err:?}"
		);
		assert_eq!(
			check_video_temp(&video, &encoded(505, 16.7, 0), Some(16.834), 0.0, None).unwrap(),
			None
		);
	}

	#[test]
	fn frame_counts_are_checked_only_without_decoder_errors() {
		let video = plan(svt(), Some(505));

		assert!(matches!(
			check_video_temp(&video, &encoded(496, 16.83, 0), Some(16.834), 0.0, None)
				.unwrap_err()
				.kind,
			ErrorKind::EncodeFailed(_)
		));
		assert_eq!(
			check_video_temp(&video, &encoded(503, 16.83, 0), Some(16.834), 0.0, None).unwrap(),
			None
		);
		let warning = check_video_temp(&video, &encoded(496, 16.83, 41), Some(16.834), 0.0, None)
			.unwrap()
			.unwrap();
		assert!(warning.contains("41 decoding errors"), "{warning}");
	}

	#[test]
	fn a_whole_ts_video_moved_onto_the_file_clock_lasts_its_declared_length() {
		let moved = VideoPlan {
			starts: VideoStarts {
				every: InputStart {
					file_s: 2.909,
					streams_s: Some(3.24),
				},
				..VideoStarts::default()
			},
			..plan(svt(), None)
		};

		assert_eq!(
			check_video_temp(&moved, &encoded(152, 6.571, 0), Some(6.24), 0.491, None).unwrap(),
			None
		);
		assert!(check_video_temp(&plan(svt(), None), &encoded(152, 6.571, 0), Some(6.24), 0.491, None).is_err());
		assert!(check_video_temp(&moved, &encoded(140, 6.1, 0), Some(6.24), 0.491, None).is_err());
	}

	#[test]
	fn without_a_trusted_frame_count_the_duration_must_match() {
		let video = plan(svt(), None);

		assert_eq!(
			check_video_temp(&video, &encoded(117, 4.66, 0), Some(4.7), 0.0, None).unwrap(),
			None
		);
		assert_eq!(
			check_video_temp(&video, &encoded(117, 5.98, 0), Some(4.7), 1.32, None).unwrap(),
			None
		);
		assert_eq!(
			check_video_temp(&video, &encoded(75, 3.5, 0), Some(3.5), 0.5, None).unwrap(),
			None
		);
		let err = check_video_temp(&video, &encoded(100, 4.4, 0), Some(4.7), 0.0, None).unwrap_err();
		assert!(
			matches!(err.kind, ErrorKind::EncodeFailed(ref reason) if reason == "the video lasts 4.40 s, 4.70 s expected"),
			"{err:?}"
		);
		assert_eq!(
			check_video_temp(&video, &encoded(100, 4.4, 3), Some(4.7), 0.0, None)
				.unwrap()
				.map(|warning| warning.contains("3 decoding errors")),
			Some(true)
		);
		assert_eq!(
			check_video_temp(&video, &encoded(100, 4.4, 0), None, 0.0, None).unwrap(),
			None
		);
	}

	#[test]
	fn a_low_rate_video_may_end_a_frame_short_of_its_declared_end() {
		let video = plan(svt(), None);
		let check = |decoded_s: f64, declared_s: f64, fps: Option<f64>| {
			check_video_temp(&video, &encoded(20, decoded_s, 0), Some(declared_s), 0.0, fps)
		};

		assert_eq!(check(1.4, 1.5, Some(10.0)).unwrap(), None);
		assert_eq!(check(1.4, 1.5, None).unwrap(), None);
		assert_eq!(check(3.8, 4.0, Some(5.0)).unwrap(), None);
		assert!(check(3.8, 4.0, None).is_err());
		assert!(check(3.6, 4.0, Some(5.0)).is_err());
		assert_eq!(check(7.5, 8.0, Some(2.0)).unwrap(), None);
	}

	#[test]
	fn a_video_under_one_frame_a_second_is_not_damaged_by_ending_a_frame_short() {
		let video = plan(svt(), Some(10));
		let check = |frames: u64, decoded_s: f64, fps: Option<f64>| {
			check_video_temp(&video, &encoded(frames, decoded_s, 0), Some(20.0), 0.0, fps)
		};

		assert_eq!(check(10, 18.0, Some(0.5)).unwrap(), None);
		assert!(matches!(
			check(9, 16.0, Some(0.5)).unwrap_err().kind,
			ErrorKind::SourceDamaged(_)
		));
		assert!(matches!(
			check(10, 18.0, None).unwrap_err().kind,
			ErrorKind::SourceDamaged(_)
		));
	}

	#[test]
	fn empty_encodes_fail() {
		let video = plan(svt(), None);

		assert!(matches!(
			check_video_temp(&video, &encoded(0, 0.0, 0), None, 0.0, None)
				.unwrap_err()
				.kind,
			ErrorKind::EncodeFailed(_)
		));
	}

	#[test]
	fn only_an_encode_short_of_the_expected_frames_asks_for_the_source_count() {
		let video = plan(svt(), Some(674));
		let inspect = |encoded: EncodedVideo| inspect_video_temp(&video, &encoded, Some(11.23), 0.0, Some(60.0));

		assert_eq!(
			inspect(encoded(668, 11.1, 0)).unwrap(),
			TempCheck::FewerFrames { expected: 674 }
		);
		assert_eq!(inspect(encoded(672, 11.2, 0)).unwrap(), TempCheck::Passed(None));
		assert!(matches!(
			inspect(encoded(680, 11.3, 0)).unwrap_err().kind,
			ErrorKind::EncodeFailed(ref reason) if reason == "the video has 680 frames, 674 expected"
		));
		assert!(matches!(
			inspect(encoded(668, 11.1, 3)).unwrap(),
			TempCheck::Passed(Some(_))
		));
		assert!(matches!(
			inspect(encoded(300, 5.0, 0)).unwrap_err().kind,
			ErrorKind::SourceDamaged(_)
		));
		assert!(matches!(
			check_video_temp(&video, &encoded(668, 11.1, 0), Some(11.23), 0.0, Some(60.0))
				.unwrap_err()
				.kind,
			ErrorKind::EncodeFailed(ref reason) if reason == "the video has 668 frames, 674 expected"
		));
	}

	#[test]
	fn a_short_encode_is_kept_when_the_source_decodes_to_as_few_frames() {
		let video = plan(svt(), Some(674));
		let decide =
			|decoded: u64| undecodable_frames(&video, &encoded(668, 11.1, 0), 674, decoded, ScanType::Progressive);

		assert_eq!(
			decide(668).unwrap(),
			"the source has 6 video frames that don't decode; the 668 that do were kept"
		);
		assert_eq!(
			decide(670).unwrap(),
			"the source has 4 video frames that don't decode; the 668 that do were kept"
		);
		assert!(matches!(
			decide(671).unwrap_err().kind,
			ErrorKind::EncodeFailed(ref reason) if reason == "the video has 668 frames, 674 expected"
		));
		assert!(decide(674).is_err());
	}

	#[test]
	fn the_source_count_is_scanned_like_the_expected_frames() {
		let deinterlaced = VideoPlan {
			frame_tolerance: 4,
			..plan(svt(), Some(1348))
		};
		let telecined = plan(svt(), Some(674));

		assert_eq!(
			undecodable_frames(
				&deinterlaced,
				&encoded(1336, 11.1, 0),
				1348,
				668,
				ScanType::Interlaced { top_first: true }
			)
			.unwrap(),
			"the source has 12 video frames that don't decode; the 1336 that do were kept"
		);
		assert!(undecodable_frames(&deinterlaced, &encoded(1336, 11.1, 0), 1348, 668, ScanType::Progressive).is_err());
		assert_eq!(
			undecodable_frames(&telecined, &encoded(668, 11.1, 0), 674, 835, ScanType::Telecined).unwrap(),
			"the source has 6 video frames that don't decode; the 668 that do were kept"
		);
	}

	#[test]
	fn the_source_count_decodes_only_the_main_video_on_the_plan_decoder_and_threads() {
		let video = VideoPlan {
			decoder: Some("libdav1d"),
			threads: Some(Threads { tool: 3, svt_lp: 1 }),
			..plan(svt(), Some(674))
		};

		let args = joined(&decode_command(Path::new("ffmpeg"), Path::new("in.mp4"), &video));

		assert!(
			args.starts_with(
				"-hide_banner -nostdin -c:v libdav1d -threads 3 -filter_threads 3 -filter_complex_threads 3 -i "
			),
			"{args}"
		);
		assert!(
			args.ends_with("in.mp4 -map 0:0 -fps_mode passthrough -f null -"),
			"{args}"
		);
	}

	#[test]
	fn audio_temp_holds_only_encoded_tracks() {
		let encode = TrackJob {
			stream_index: 1,
			sample_rate: 48_000,
			plan: TrackPlan::Encode(EncodePlan {
				bitrate_kbps: 128,
				channels: 2,
				layout: LayoutFix::Keep("stereo"),
			}),
		};
		let copy = TrackJob {
			stream_index: 2,
			sample_rate: 48_000,
			plan: TrackPlan::Copy,
		};
		let mixed = StreamPlan {
			audio: vec![copy.clone(), encode.clone()],
			..StreamPlan::default()
		};
		let copies_only = StreamPlan {
			audio: vec![copy],
			..StreamPlan::default()
		};

		let temp = audio_plan(&MediaInfo::default(), &mixed, None).unwrap();

		assert_eq!(temp.tracks, vec![encode]);
		assert_eq!(temp.container.extension, "mka");
		assert_eq!(temp.container.cover, Cover::Attachment);
		assert_eq!(temp.options.delay_s, None);
		assert_eq!(audio_plan(&MediaInfo::default(), &copies_only, None), None);
	}

	#[test]
	fn ts_audio_is_seeked_early_cut_on_the_decoded_audio_and_delayed_onto_the_file_clock() {
		let info = |format: &str| {
			MediaInfo::parse(
				format!(
					r#"{{"format":{{"format_name":"{format}","start_time":"1.400000"}},"streams":[
					{{"index":0,"codec_type":"video","start_time":"1.400000"}},
					{{"index":1,"codec_type":"audio","start_time":"1.900000"}}]}}"#
				)
				.as_bytes(),
			)
			.unwrap()
		};
		let streams = StreamPlan {
			audio: vec![opus(1, 128)],
			..StreamPlan::default()
		};
		let window = |start_s: f64, length_s: f64| TrimWindow { start_s, length_s };
		let options =
			|format: &str, trim: Option<TrimWindow>| audio_plan(&info(format), &streams, trim).unwrap().options;
		let near = |actual: Option<f64>, expected: f64| actual.is_some_and(|actual| (actual - expected).abs() < 1e-9);

		let whole = options("mpegts", None);
		let early = options("mpegts", Some(window(0.1, 4.0)));
		let late = options("mpegts", Some(window(8.0, 4.0)));
		let mkv = options("matroska,webm", Some(window(0.1, 4.0)));

		assert!(near(whole.delay_s, 0.5), "{whole:?}");
		assert_eq!((whole.window, whole.kept), (None, None));
		assert!(near(early.delay_s, 0.5), "{early:?}");
		assert_eq!(early.window, Some(window(0.0, 4.1)));
		assert!(near(early.kept.map(|kept| kept.start_s), 0.1), "{early:?}");
		assert!(near(early.kept.map(|kept| kept.length_s), 4.0), "{early:?}");
		assert_eq!(late.delay_s, None);
		assert_eq!(late.window, Some(window(3.0, 9.0)));
		assert_eq!(late.kept, Some(window(5.0, 4.0)));
		assert_eq!(
			(mkv.delay_s, mkv.window, mkv.kept),
			(None, Some(window(0.1, 4.0)), None)
		);
	}
}
