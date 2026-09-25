use std::path::Path;

use tokio_util::sync::CancellationToken;

use crate::audio::plan::FilePlan;
use crate::container::{Container, Cover};
use crate::outcome::{ErrorKind, JobError};
use crate::process::{self, STALL_TIMEOUT, StdoutMode, ToolCommand};
use crate::video::plan::{Encoder, VideoPlan};
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

const FRAME_TOLERANCE: u64 = 2;
const AUDIO_TEMP: Container = Container {
	extension: "mka",
	muxer: "matroska",
	cover: Cover::Attachment,
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EncodedVideo {
	pub frames: u64,
	pub decoded_s: f64,
	pub decode_errors: usize,
}

pub fn video_command(ffmpeg: &Path, source: &Path, output: &Path, plan: &VideoPlan) -> ToolCommand {
	let mut cmd = ToolCommand::new(ffmpeg, STALL_TIMEOUT);
	cmd.count_lines_containing(DECODE_ERRORS);
	cmd.args(["-hide_banner", "-nostdin", "-y", "-loglevel", "repeat+info"]);
	if let Some(decoder) = plan.decoder {
		cmd.arg("-c:v").arg(decoder);
	}
	cmd.arg("-i").arg(process::tool_path(source));

	for index in &plan.streams {
		cmd.arg("-map").arg(format!("0:{index}"));
	}
	for ordinal in 0..plan.streams.len() {
		cmd.arg(format!("-filter:v:{ordinal}")).arg(&plan.filters);
	}

	match &plan.encoder {
		Encoder::SvtAv1 { preset, params } => {
			cmd.args(["-c:v", "libsvtav1", "-preset"])
				.arg(preset.to_string())
				.args(["-err_detect", "explode", "-dolbyvision", "0", "-svtav1-params"])
				.arg(params);
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

	cmd.args(["-fps_mode", "passthrough", "-f", "matroska"])
		.arg(process::tool_path(output));
	cmd
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
		let kind = if out.counted_lines > 0 {
			ErrorKind::SourceDamaged(format!(
				"the video cannot be decoded ({} decoding errors)",
				out.counted_lines
			))
		} else {
			ErrorKind::EncodeFailed(process::failure_reason(&out.stderr_tail))
		};
		return Err(JobError::with_log(kind, out.stderr_tail));
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

	match plan.expected_frames {
		Some(expected) if encoded.frames.abs_diff(expected) > FRAME_TOLERANCE => {
			Err(ErrorKind::EncodeFailed(format!("the video has {} frames, {expected} expected", encoded.frames)).into())
		}
		_ => Ok(None),
	}
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

	let mut cmd = ToolCommand::new(ffmpeg, STALL_TIMEOUT);
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
	use crate::audio::plan::{EncodePlan, LayoutFix, TrackJob, TrackPlan};
	use crate::video::colour::{ColourTags, Range};

	fn plan(encoder: Encoder, expected_frames: Option<u64>) -> VideoPlan {
		VideoPlan {
			streams: vec![0, 2],
			decoder: None,
			filters: "format=yuv420p10le,setparams=field_mode=prog".into(),
			encoder,
			expected_frames,
			width: 320,
			height: 240,
			colour: ColourTags {
				primaries: "bt709".into(),
				transfer: "bt709".into(),
				matrix: "bt709".into(),
				range: Range::Limited,
			},
			alpha: false,
			hdr10: false,
			warnings: Vec::new(),
		}
	}

	fn svt() -> Encoder {
		Encoder::SvtAv1 {
			preset: 8,
			params: "tune=0:crf=32".into(),
		}
	}

	fn joined(cmd: &ToolCommand) -> String {
		cmd.args
			.iter()
			.map(|arg| arg.to_string_lossy().into_owned())
			.collect::<Vec<_>>()
			.join(" ")
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
			args.contains("-c:v libsvtav1 -preset 8 -err_detect explode -dolbyvision 0 -svtav1-params tune=0:crf=32"),
			"{args}"
		);
		assert!(args.contains("-fps_mode passthrough -f matroska"), "{args}");
		assert_eq!(cmd.counted, DECODE_ERRORS);
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
	fn short_decodes_are_source_damage() {
		let video = plan(svt(), Some(505));

		let err = check_video_temp(&video, &encoded(291, 9.7, 0), Some(16.834)).unwrap_err();

		assert!(
			matches!(err.kind, ErrorKind::SourceDamaged(ref reason) if reason.contains("9.7")),
			"{err:?}"
		);
		assert_eq!(
			check_video_temp(&video, &encoded(505, 16.7, 0), Some(16.834)).unwrap(),
			None
		);
	}

	#[test]
	fn frame_counts_are_checked_only_without_decoder_errors() {
		let video = plan(svt(), Some(505));

		assert!(matches!(
			check_video_temp(&video, &encoded(496, 16.83, 0), Some(16.834))
				.unwrap_err()
				.kind,
			ErrorKind::EncodeFailed(_)
		));
		assert_eq!(
			check_video_temp(&video, &encoded(503, 16.83, 0), Some(16.834)).unwrap(),
			None
		);
		let warning = check_video_temp(&video, &encoded(496, 16.83, 41), Some(16.834))
			.unwrap()
			.unwrap();
		assert!(warning.contains("41 decoding errors"), "{warning}");
	}

	#[test]
	fn empty_encodes_fail() {
		let video = plan(svt(), None);

		assert!(matches!(
			check_video_temp(&video, &encoded(0, 0.0, 0), None).unwrap_err().kind,
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
