use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;

use crate::audio::plan::TrackPlan;
use crate::outcome::JobError;
use crate::probe::MediaInfo;
use crate::process::{self, STALL_TIMEOUT, ToolCommand};
use crate::video::plan::VideoPlan;
use crate::video::run_ffmpeg;
use crate::video::streams::StreamPlan;

const BRAND_TAGS: &[&str] = &["major_brand", "minor_version", "compatible_brands"];
const CLEAN_STREAM_TAGS: &[&str] = &["language", "title"];
const ATTACHMENT_TAGS: &[&str] = &["filename", "mimetype"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
	pub path: PathBuf,
	pub file_name: String,
	pub mime_type: String,
}

pub struct MuxInputs<'a> {
	pub video: &'a Path,
	pub audio: Option<&'a Path>,
	pub source: &'a Path,
	pub attachments: &'a [Attachment],
}

pub struct MuxPlan<'a> {
	pub info: &'a MediaInfo,
	pub video: &'a VideoPlan,
	pub streams: &'a StreamPlan,
	pub keep_metadata: bool,
}

pub fn command(ffmpeg: &Path, inputs: &MuxInputs<'_>, plan: &MuxPlan<'_>, output: &Path) -> ToolCommand {
	let mut cmd = ToolCommand::new(ffmpeg, STALL_TIMEOUT);
	cmd.args(["-hide_banner", "-nostdin", "-y", "-i"])
		.arg(process::tool_path(inputs.video));
	if let Some(audio) = inputs.audio {
		cmd.arg("-i").arg(process::tool_path(audio));
	}

	let source = if inputs.audio.is_some() { 2 } else { 1 };
	if plan.keep_metadata && plan.info.format.format_name.contains("mov") {
		cmd.args(["-export_xmp", "1"]);
	}
	cmd.arg("-i").arg(process::tool_path(inputs.source));

	let origins = push_maps(&mut cmd, plan, source);
	cmd.args(["-c", "copy"]);
	for (ordinal, _) in plan
		.streams
		.subtitles
		.iter()
		.enumerate()
		.filter(|(_, subtitle)| subtitle.to_srt)
	{
		cmd.arg(format!("-c:s:{ordinal}")).arg("srt");
	}

	for (offset, attachment) in inputs.attachments.iter().enumerate() {
		let spec = format!("-metadata:s:t:{}", plan.streams.attachments.len() + offset);
		cmd.arg("-attach")
			.arg(process::tool_path(&attachment.path))
			.arg(&spec)
			.arg(format!("mimetype={}", attachment.mime_type))
			.arg(&spec)
			.arg(format!("filename={}", attachment.file_name));
	}

	if plan.keep_metadata {
		cmd.arg("-map_metadata").arg(source.to_string());
		for tag in BRAND_TAGS {
			cmd.arg("-metadata").arg(format!("{tag}="));
		}
	} else {
		push_clean_metadata(&mut cmd, plan, &origins);
	}

	cmd.arg("-map_chapters").arg(source.to_string());
	cmd.args(["-cues_to_front", "1", "-f", "matroska"])
		.arg(process::tool_path(output));
	cmd
}

pub async fn mux(cmd: &ToolCommand, cancel: &CancellationToken) -> Result<(), JobError> {
	run_ffmpeg(cmd, |_| {}, cancel).await.map(drop)
}

fn push_maps(cmd: &mut ToolCommand, plan: &MuxPlan<'_>, source: usize) -> Vec<u32> {
	let mut origins = Vec::new();
	for (ordinal, &index) in plan.video.streams.iter().enumerate() {
		cmd.arg("-map").arg(format!("0:v:{ordinal}"));
		origins.push(index);
	}

	let mut encoded = 0;
	for track in &plan.streams.audio {
		if matches!(track.plan, TrackPlan::Encode(_)) {
			cmd.arg("-map").arg(format!("1:a:{encoded}"));
			encoded += 1;
		} else {
			cmd.arg("-map").arg(format!("{source}:{}", track.stream_index));
		}
		origins.push(track.stream_index);
	}

	for subtitle in &plan.streams.subtitles {
		cmd.arg("-map").arg(format!("{source}:{}", subtitle.stream_index));
		origins.push(subtitle.stream_index);
	}
	for attachment in &plan.streams.attachments {
		cmd.arg("-map").arg(format!("{source}:{attachment}"));
	}
	origins
}

fn push_clean_metadata(cmd: &mut ToolCommand, plan: &MuxPlan<'_>, origins: &[u32]) {
	cmd.args(["-map_metadata:g", "-1", "-map_metadata:s", "-1"]);

	for (output, origin) in origins.iter().enumerate() {
		let Some(stream) = plan.info.streams.iter().find(|stream| stream.index == *origin) else {
			continue;
		};
		for key in CLEAN_STREAM_TAGS {
			if let Some(value) = stream.tag(key) {
				cmd.arg(format!("-metadata:s:{output}")).arg(format!("{key}={value}"));
			}
		}
	}

	for (ordinal, origin) in plan.streams.attachments.iter().enumerate() {
		let Some(stream) = plan.info.streams.iter().find(|stream| stream.index == *origin) else {
			continue;
		};
		for key in ATTACHMENT_TAGS {
			if let Some(value) = stream.tag(key) {
				cmd.arg(format!("-metadata:s:t:{ordinal}"))
					.arg(format!("{key}={value}"));
			}
		}
	}

	if plan.video.alpha {
		cmd.args(["-metadata:s:v:0", "alpha_mode=1"]);
	}
	if let Some(timecode) = timecode(plan.info) {
		cmd.arg("-metadata").arg(format!("timecode={timecode}"));
	}
}

fn timecode(info: &MediaInfo) -> Option<&str> {
	info.streams
		.iter()
		.find_map(|stream| stream.tag("timecode"))
		.or_else(|| info.format.tags.get("timecode").map(String::as_str))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::audio::plan::{EncodePlan, LayoutFix, TrackJob};
	use crate::video::colour::{ColourTags, Range};
	use crate::video::plan::Encoder;
	use crate::video::streams::SubtitleTrack;

	fn video_plan(alpha: bool) -> VideoPlan {
		VideoPlan {
			streams: vec![0],
			decoder: None,
			filters: String::new(),
			encoder: Encoder::SvtAv1 {
				preset: 8,
				params: String::new(),
			},
			expected_frames: None,
			width: 320,
			height: 240,
			colour: ColourTags {
				primaries: "bt709".into(),
				transfer: "bt709".into(),
				matrix: "bt709".into(),
				range: Range::Limited,
			},
			alpha,
			hdr10: false,
			warnings: Vec::new(),
		}
	}

	fn stream_plan() -> StreamPlan {
		StreamPlan {
			audio: vec![
				TrackJob {
					stream_index: 1,
					sample_rate: 48_000,
					plan: TrackPlan::Encode(EncodePlan {
						bitrate_kbps: 96,
						channels: 2,
						layout: LayoutFix::Keep("stereo"),
					}),
				},
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
					to_srt: true,
				},
			],
			attachments: vec![5],
			extracted: Vec::new(),
			warnings: Vec::new(),
		}
	}

	fn info() -> MediaInfo {
		MediaInfo::parse(
			br#"{"streams":[{"index":0,"codec_type":"video","tags":{"title":"Angle 1"}},
			{"index":1,"codec_type":"audio","tags":{"language":"eng","title":"Main"}},
			{"index":2,"codec_type":"audio","tags":{"language":"jpn"}},
			{"index":3,"codec_type":"subtitle","tags":{"language":"slv","timecode":"01:02:03:04"}},
			{"index":4,"codec_type":"subtitle"},
			{"index":5,"codec_type":"attachment","tags":{"filename":"a.ttf","mimetype":"font/ttf"}}],
			"format":{"format_name":"mov,mp4,m4a,3gp,3g2,mj2"}}"#,
		)
		.unwrap()
	}

	fn joined(cmd: &ToolCommand) -> String {
		cmd.args
			.iter()
			.map(|arg| arg.to_string_lossy().into_owned())
			.collect::<Vec<_>>()
			.join(" ")
	}

	fn build(keep_metadata: bool, alpha: bool, attachments: &[Attachment]) -> String {
		let (info, video, streams) = (info(), video_plan(alpha), stream_plan());
		let inputs = MuxInputs {
			video: Path::new("video.a1.mkv"),
			audio: Some(Path::new("audio.mka")),
			source: Path::new("source.mp4"),
			attachments,
		};
		let plan = MuxPlan {
			info: &info,
			video: &video,
			streams: &streams,
			keep_metadata,
		};
		joined(&command(Path::new("ffmpeg"), &inputs, &plan, Path::new("mux.mkv")))
	}

	#[test]
	fn maps_streams_in_output_order() {
		let args = build(true, false, &[]);

		assert!(
			args.contains("-map 0:v:0 -map 1:a:0 -map 2:2 -map 2:3 -map 2:4 -map 2:5 -c copy -c:s:1 srt"),
			"{args}"
		);
		assert!(args.contains("-map_chapters 2 -cues_to_front 1 -f matroska"), "{args}");
	}

	#[test]
	fn keep_mode_maps_global_metadata_and_exports_xmp() {
		let args = build(true, false, &[]);

		assert!(args.contains("-export_xmp 1 -i"), "{args}");
		assert!(
			args.contains(
				"-map_metadata 2 -metadata major_brand= -metadata minor_version= -metadata compatible_brands="
			)
		);
		assert!(!args.contains("-map_metadata:s"), "{args}");
	}

	#[test]
	fn clean_mode_keeps_language_title_timecode_and_alpha() {
		let args = build(false, true, &[]);

		assert!(!args.contains("-export_xmp"), "{args}");
		assert!(args.contains("-map_metadata:g -1 -map_metadata:s -1"), "{args}");
		assert!(args.contains("-metadata:s:0 title=Angle 1"), "{args}");
		assert!(
			args.contains("-metadata:s:1 language=eng -metadata:s:1 title=Main"),
			"{args}"
		);
		assert!(args.contains("-metadata:s:2 language=jpn"), "{args}");
		assert!(args.contains("-metadata:s:3 language=slv"), "{args}");
		assert!(args.contains("-metadata:s:v:0 alpha_mode=1"), "{args}");
		assert!(args.contains("-metadata timecode=01:02:03:04"), "{args}");
		assert!(
			args.contains("-metadata:s:t:0 filename=a.ttf -metadata:s:t:0 mimetype=font/ttf"),
			"{args}"
		);
	}

	#[test]
	fn attachments_are_numbered_after_the_mapped_ones() {
		let files = [Attachment {
			path: PathBuf::from("stream-9.bin"),
			file_name: "cover.jpg".into(),
			mime_type: "image/jpeg".into(),
		}];

		let args = build(true, false, &files);

		assert!(
			args.contains("-metadata:s:t:1 mimetype=image/jpeg -metadata:s:t:1 filename=cover.jpg"),
			"{args}"
		);
	}

	#[test]
	fn without_an_audio_temp_the_source_is_input_one() {
		let (info, video) = (info(), video_plan(false));
		let streams = StreamPlan {
			audio: vec![TrackJob {
				stream_index: 2,
				sample_rate: 48_000,
				plan: TrackPlan::Copy,
			}],
			..StreamPlan::default()
		};
		let inputs = MuxInputs {
			video: Path::new("video.a1.mkv"),
			audio: None,
			source: Path::new("source.mkv"),
			attachments: &[],
		};
		let plan = MuxPlan {
			info: &info,
			video: &video,
			streams: &streams,
			keep_metadata: true,
		};

		let args = joined(&command(Path::new("ffmpeg"), &inputs, &plan, Path::new("mux.mkv")));

		assert!(args.contains("-map 0:v:0 -map 1:2 -c copy"), "{args}");
		assert!(args.contains("-map_metadata 1"), "{args}");
	}
}
