use std::fs;
use std::path::Path;

use tokio_util::sync::CancellationToken;

use crate::audio::plan::{FilePlan, TrackPlan};
use crate::container::{Container, Cover, MATROSKA_AUDIO};
use crate::outcome::{ErrorKind, IoSide, JobError, KeepReason};
use crate::probe::{MediaInfo, Stream};
use crate::process::{self, STALL_TIMEOUT, StdoutMode, ToolCommand};
use crate::video::analyze;
use crate::video::plan::{Encoder, VideoPlan, svt_params};
use crate::video::run_ffmpeg;
use crate::video::streams::{Extracted, StreamPlan};
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
	let mut cmd = ToolCommand::new(ffmpeg, STALL_TIMEOUT);
	cmd.count_lines_containing(DECODE_ERRORS);
	cmd.args(["-hide_banner", "-nostdin", "-y", "-loglevel", "repeat+info"]);

	if let Some(sample) = sample {
		cmd.arg("-ss")
			.arg(format!("{:.3}", sample.start_s))
			.arg("-t")
			.arg(format!("{:.3}", sample.length_s));
	}

	if let Some(decoder) = plan.decoder {
		cmd.arg("-c:v").arg(decoder);
	}
	cmd.arg("-i").arg(process::tool_path(source));

	let streams = if sample.is_some() {
		plan.streams.get(..1).unwrap_or_default()
	} else {
		plan.streams.as_slice()
	};
	for index in streams {
		cmd.arg("-map").arg(format!("0:{index}"));
	}
	for ordinal in 0..streams.len() {
		cmd.arg(format!("-filter:v:{ordinal}")).arg(&plan.filters);
	}

	encoder_args(&mut cmd, &plan.encoder);

	cmd.args(["-fps_mode", "passthrough", "-f", "matroska"])
		.arg(process::tool_path(output));
	cmd
}

fn encoder_args(cmd: &mut ToolCommand, encoder: &Encoder) {
	match encoder {
		Encoder::SvtAv1 {
			preset,
			crf,
			film_grain,
		} => {
			cmd.args(["-c:v", "libsvtav1", "-preset"])
				.arg(preset.to_string())
				.args(["-err_detect", "explode", "-dolbyvision", "0", "-svtav1-params"])
				.arg(svt_params(*crf, *film_grain));
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
		(None, Some(declared)) if !lasts(encoded, start_s, declared) => Err(ErrorKind::EncodeFailed(format!(
			"the video lasts {:.2} s, {declared:.2} s expected",
			encoded.decoded_s - start_s
		))
		.into()),
		_ => Ok(None),
	}
}

// A declared duration is a length in MP4 but the end timestamp in Matroska's DURATION tag, and the video temp keeps
// the source's start offset, so either reading may be the one that matches.
fn lasts(encoded: &EncodedVideo, start_s: f64, declared_s: f64) -> bool {
	[encoded.decoded_s - start_s, encoded.decoded_s]
		.iter()
		.any(|decoded| (decoded - declared_s).abs() <= DURATION_TOLERANCE_S)
}

pub fn audio_plan(streams: &StreamPlan) -> Option<FilePlan> {
	let tracks = streams.encoded_audio();
	(!tracks.is_empty()).then_some(FilePlan {
		container: AUDIO_TEMP,
		tracks,
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
	use crate::video::joined;
	use crate::video::plan::test_plan;
	use crate::video::streams::SubtitleTrack;

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
					to_srt: false,
				},
				SubtitleTrack {
					stream_index: 4,
					to_srt: false,
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

		let temp = audio_plan(&mixed).unwrap();

		assert_eq!(temp.tracks, vec![encode]);
		assert_eq!(temp.container.extension, "mka");
		assert_eq!(temp.container.cover, Cover::Attachment);
		assert_eq!(audio_plan(&copies_only), None);
	}
}
