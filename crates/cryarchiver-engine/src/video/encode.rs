use std::fs;
use std::path::Path;

use tokio_util::sync::CancellationToken;

use crate::audio::plan::{FileOptions, FilePlan, TrackPlan};
use crate::container::{Container, Cover, MATROSKA_AUDIO};
use crate::options::TrimWindow;
use crate::outcome::{ErrorKind, IoSide, JobError, KeepReason};
use crate::probe::{MediaInfo, Stream};
use crate::process::{self, STALL_TIMEOUT, StdoutMode, ToolCommand};
use crate::video::plan::{Encoder, Threads, VideoPlan, svt_params};
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
const PREDICTION_MARGIN: f64 = 0.05;
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

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
	pub start_s: f64,
	pub length_s: f64,
}

pub fn video_command(ffmpeg: &Path, source: &Path, output: &Path, plan: &VideoPlan) -> ToolCommand {
	encode_command(ffmpeg, source, output, plan, None)
}

pub fn sample_command(ffmpeg: &Path, source: &Path, output: &Path, plan: &VideoPlan, sample: &Sample) -> ToolCommand {
	encode_command(ffmpeg, source, output, plan, Some(sample))
}

fn encode_command(
	ffmpeg: &Path,
	source: &Path,
	output: &Path,
	plan: &VideoPlan,
	sample: Option<&Sample>,
) -> ToolCommand {
	let workspace = output.parent().unwrap_or(Path::new(""));
	let mut cmd = ToolCommand::new(ffmpeg, STALL_TIMEOUT);
	if plan.subtitles.is_some() && !workspace.as_os_str().is_empty() {
		cmd.in_dir(workspace);
	}
	cmd.count_lines_containing(DECODE_ERRORS);
	cmd.args(["-hide_banner", "-nostdin", "-y", "-loglevel", "repeat+info"]);

	if let Some(threads) = plan.threads {
		cmd.arg("-threads")
			.arg(threads.tool.to_string())
			.arg("-filter_threads")
			.arg(threads.tool.to_string());
	}

	let start = if sample.is_some() {
		plan.starts.main
	} else {
		plan.starts.every
	};
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

	if let Some(decoder) = plan.decoder {
		cmd.arg("-c:v").arg(decoder);
	}
	cmd.arg("-i").arg(process::tool_path(source));
	if let Some(overlay) = plan.overlay {
		cmd.args(overlay.input_args())
			.arg("-i")
			.arg(process::tool_path(&workspace.join(burn::BITMAP_FILE)));
	}

	let streams = if sample.is_some() {
		plan.streams.get(..1).unwrap_or_default()
	} else {
		plan.streams.as_slice()
	};
	let graphed = if let Some(overlay) = plan.overlay {
		cmd.arg("-filter_complex").arg(burn::overlay_graph(
			(&format!("[0:{}]", streams[0]), &trim),
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
	let main = burn::main_chain(&trim, plan.subtitles.as_deref(), &plan.filters, seek_s);
	let others = burn::main_chain(&trim, None, &plan.filters, None);
	for ordinal in graphed..streams.len() {
		let chain = if ordinal == 0 { &main } else { &others };
		cmd.arg(format!("-filter:v:{ordinal}")).arg(chain);
	}

	encoder_args(&mut cmd, &plan.encoder, plan.threads);

	cmd.args(["-fps_mode", "passthrough", "-f", "matroska"])
		.arg(process::tool_path(output));
	cmd
}

fn encoder_args(cmd: &mut ToolCommand, encoder: &Encoder, threads: Option<Threads>) {
	match encoder {
		Encoder::SvtAv1 {
			preset,
			crf,
			film_grain,
		} => {
			cmd.args(["-c:v", "libsvtav1", "-preset"])
				.arg(preset.to_string())
				.args(["-err_detect", "explode", "-dolbyvision", "0", "-svtav1-params"])
				.arg(svt_params(*crf, *film_grain, threads.map(|threads| threads.svt_lp)));
		}
		Encoder::Aom { cpu_used, crf } => {
			cmd.args(["-c:v", "libaom-av1", "-crf"])
				.arg(crf.to_string())
				.args(["-b:v", "0", "-cpu-used"])
				.arg(cpu_used.to_string())
				.args(["-row-mt", "1"]);
		}
		Encoder::Vp9Alpha { cpu_used, crf } => {
			cmd.args(["-c:v", "libvpx-vp9", "-crf"])
				.arg(crf.to_string())
				.args(["-b:v", "0", "-row-mt", "1", "-deadline", "good", "-cpu-used"])
				.arg(cpu_used.to_string());
		}
	}

	if let (Some(threads), Encoder::Aom { .. } | Encoder::Vp9Alpha { .. }) = (threads, encoder) {
		cmd.arg("-threads").arg(threads.tool.to_string());
	}
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

pub async fn encode_sample(
	ffmpeg: &Path,
	source: &Path,
	output: &Path,
	plan: &VideoPlan,
	sample: &Sample,
	cancel: &CancellationToken,
) -> Result<u64, JobError> {
	let cmd = sample_command(ffmpeg, source, output, plan, sample);
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
	cancel: &CancellationToken,
) -> Result<u64, JobError> {
	let mut total = 0;
	for (index, sample) in samples.iter().enumerate() {
		let output = workspace.file(&format!("sample-{index}.mkv"));
		total += encode_sample(ffmpeg, source, &output, plan, sample, cancel).await?;
	}
	Ok(total)
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

pub fn check_video_temp(
	plan: &VideoPlan,
	encoded: &EncodedVideo,
	declared_s: Option<f64>,
	start_s: f64,
) -> Result<Option<String>, JobError> {
	if let Some(declared) = declared_s
		&& declared - encoded.decoded_s > (declared * 0.02).max(1.0)
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
		return Ok(Some(format!(
			"the source has {} decoding errors; glitches are carried over as they are",
			encoded.decode_errors
		)));
	}

	match (plan.expected_frames, declared_s) {
		(Some(expected), _) if encoded.frames.abs_diff(expected) > plan.frame_tolerance => {
			Err(ErrorKind::EncodeFailed(format!("the video has {} frames, {expected} expected", encoded.frames)).into())
		}
		(None, Some(declared)) if !lasts(plan, encoded, start_s, declared) => Err(ErrorKind::EncodeFailed(format!(
			"the video lasts {:.2} s, {declared:.2} s expected",
			encoded.decoded_s - start_s
		))
		.into()),
		_ => Ok(None),
	}
}

// A declared duration is a length in MP4 but the end timestamp in Matroska's DURATION tag, and the video temp keeps
// the source's start offset, so either reading may be the one that matches. A TS/PS video's declared length counts
// from its first packet, which the whole file's `-itsoffset` moved later on the temp, and which may not decode.
fn lasts(plan: &VideoPlan, encoded: &EncodedVideo, start_s: f64, declared_s: f64) -> bool {
	let offset_s = plan
		.window
		.map_or_else(|| plan.starts.every.offset_s(None), |_| None)
		.unwrap_or(0.0);

	[
		encoded.decoded_s - start_s,
		encoded.decoded_s,
		encoded.decoded_s - offset_s,
	]
	.iter()
	.any(|decoded| (decoded - declared_s).abs() <= DURATION_TOLERANCE_S)
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
	use crate::video::plan::test_plan;
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

		assert!(args.contains("-threads 4 -filter_threads 4 -i "), "{args}");
		assert!(args.contains("-svtav1-params tune=0:keyint=5s:crf=32:lp=2"), "{args}");
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

	#[test]
	fn short_decodes_are_source_damage() {
		let video = plan(svt(), Some(505));

		let err = check_video_temp(&video, &encoded(291, 9.7, 0), Some(16.834), 0.0).unwrap_err();

		assert!(
			matches!(err.kind, ErrorKind::SourceDamaged(ref reason) if reason.contains("9.7")),
			"{err:?}"
		);
		assert_eq!(
			check_video_temp(&video, &encoded(505, 16.7, 0), Some(16.834), 0.0).unwrap(),
			None
		);
	}

	#[test]
	fn frame_counts_are_checked_only_without_decoder_errors() {
		let video = plan(svt(), Some(505));

		assert!(matches!(
			check_video_temp(&video, &encoded(496, 16.83, 0), Some(16.834), 0.0)
				.unwrap_err()
				.kind,
			ErrorKind::EncodeFailed(_)
		));
		assert_eq!(
			check_video_temp(&video, &encoded(503, 16.83, 0), Some(16.834), 0.0).unwrap(),
			None
		);
		let warning = check_video_temp(&video, &encoded(496, 16.83, 41), Some(16.834), 0.0)
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
			check_video_temp(&moved, &encoded(152, 6.571, 0), Some(6.24), 0.491).unwrap(),
			None
		);
		assert!(check_video_temp(&plan(svt(), None), &encoded(152, 6.571, 0), Some(6.24), 0.491).is_err());
		assert!(check_video_temp(&moved, &encoded(140, 6.1, 0), Some(6.24), 0.491).is_err());
	}

	#[test]
	fn without_a_trusted_frame_count_the_duration_must_match() {
		let video = plan(svt(), None);

		assert_eq!(
			check_video_temp(&video, &encoded(117, 4.66, 0), Some(4.7), 0.0).unwrap(),
			None
		);
		assert_eq!(
			check_video_temp(&video, &encoded(117, 5.98, 0), Some(4.7), 1.32).unwrap(),
			None
		);
		assert_eq!(
			check_video_temp(&video, &encoded(75, 3.5, 0), Some(3.5), 0.5).unwrap(),
			None
		);
		let err = check_video_temp(&video, &encoded(100, 4.4, 0), Some(4.7), 0.0).unwrap_err();
		assert!(
			matches!(err.kind, ErrorKind::EncodeFailed(ref reason) if reason == "the video lasts 4.40 s, 4.70 s expected"),
			"{err:?}"
		);
		assert_eq!(
			check_video_temp(&video, &encoded(100, 4.4, 3), Some(4.7), 0.0)
				.unwrap()
				.map(|warning| warning.contains("3 decoding errors")),
			Some(true)
		);
		assert_eq!(
			check_video_temp(&video, &encoded(100, 4.4, 0), None, 0.0).unwrap(),
			None
		);
	}

	#[test]
	fn empty_encodes_fail() {
		let video = plan(svt(), None);

		assert!(matches!(
			check_video_temp(&video, &encoded(0, 0.0, 0), None, 0.0)
				.unwrap_err()
				.kind,
			ErrorKind::EncodeFailed(_)
		));
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
