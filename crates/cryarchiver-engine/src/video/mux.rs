use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;

use crate::audio::plan::TrackPlan;
use crate::container::{Container, MP4_VIDEO};
use crate::outcome::JobError;
use crate::probe::{MediaInfo, Stream};
use crate::process::{self, ToolCommand};
use crate::video::plan::VideoPlan;
use crate::video::run_ffmpeg;
use crate::video::streams::StreamPlan;

const BRAND_TAGS: &[&str] = &["major_brand", "minor_version", "compatible_brands"];
const CLEAN_STREAM_TAGS: &[&str] = &["language", "title"];
const ATTACHMENT_TAGS: &[&str] = &["filename", "mimetype"];
const STATISTICS_TAGS: &[&str] = &[
	"BPS",
	"DURATION",
	"NUMBER_OF_FRAMES",
	"NUMBER_OF_BYTES",
	"_STATISTICS_TAGS",
	"_STATISTICS_WRITING_APP",
	"_STATISTICS_WRITING_DATE_UTC",
];

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
	pub source_bytes: u64,
	pub attachments: &'a [Attachment],
}

pub struct MuxPlan<'a> {
	pub info: &'a MediaInfo,
	pub video: &'a VideoPlan,
	pub streams: &'a StreamPlan,
	pub keep_metadata: bool,
	pub container: Container,
	pub chapters: ChapterSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChapterSource {
	Source,
	Dropped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MuxInput {
	VideoTemp(usize),
	AudioTemp(usize),
	Source,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputStream {
	pub origin: u32,
	pub input: MuxInput,
}

impl OutputStream {
	pub fn is_encoded(self) -> bool {
		self.input != MuxInput::Source
	}
}

pub fn output_streams(video: &VideoPlan, streams: &StreamPlan) -> Vec<OutputStream> {
	let mut outputs: Vec<OutputStream> = video
		.streams
		.iter()
		.enumerate()
		.map(|(ordinal, &origin)| OutputStream {
			origin,
			input: MuxInput::VideoTemp(ordinal),
		})
		.collect();

	let mut encoded = 0;
	for track in &streams.audio {
		let input = if matches!(track.plan, TrackPlan::Encode(_)) {
			encoded += 1;
			MuxInput::AudioTemp(encoded - 1)
		} else {
			MuxInput::Source
		};
		outputs.push(OutputStream {
			origin: track.stream_index,
			input,
		});
	}

	outputs.extend(streams.subtitles.iter().map(|subtitle| OutputStream {
		origin: subtitle.stream_index,
		input: MuxInput::Source,
	}));
	outputs
}

pub fn command(ffmpeg: &Path, inputs: &MuxInputs<'_>, plan: &MuxPlan<'_>, output: &Path) -> ToolCommand {
	let mut cmd = ToolCommand::new(ffmpeg, process::scaled_timeout(inputs.source_bytes));
	cmd.args(["-hide_banner", "-nostdin", "-y", "-copyts", "-i"])
		.arg(process::tool_path(inputs.video));
	if let Some(audio) = inputs.audio {
		cmd.arg("-i").arg(process::tool_path(audio));
	}

	let source = if inputs.audio.is_some() { 2 } else { 1 };
	if plan.keep_metadata && plan.info.format.format_name.contains("mov") {
		cmd.args(["-export_xmp", "1"]);
	}
	if let Some(offset) = source_offset(plan.info) {
		cmd.arg("-itsoffset").arg(offset);
	}
	cmd.arg("-i").arg(process::tool_path(inputs.source));

	let outputs = output_streams(plan.video, plan.streams);
	push_maps(&mut cmd, &outputs, &plan.streams.attachments, source);
	cmd.args(["-c", "copy"]);
	for (ordinal, codec) in plan
		.streams
		.subtitles
		.iter()
		.enumerate()
		.filter_map(|(ordinal, subtitle)| Some((ordinal, subtitle.codec?)))
	{
		cmd.arg(format!("-c:s:{ordinal}")).arg(codec);
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
		push_statistics_reset(&mut cmd, plan.info, &outputs);
	} else {
		push_clean_metadata(&mut cmd, plan, &outputs);
	}

	match plan.chapters {
		ChapterSource::Source => cmd.arg("-map_chapters").arg(source.to_string()),
		ChapterSource::Dropped => cmd.args(["-map_chapters", "-1"]),
	};
	if plan.container.muxer == MP4_VIDEO.muxer {
		cmd.args(["-movflags", "+faststart"]);
	} else {
		cmd.args(["-cues_to_front", "1"]);
	}
	cmd.arg("-f").arg(plan.container.muxer).arg(process::tool_path(output));
	cmd
}

pub async fn mux(cmd: &ToolCommand, cancel: &CancellationToken) -> Result<(), JobError> {
	run_ffmpeg(cmd, |_| {}, cancel).await.map(drop)
}

// The temps were cut from the source shifted by its start time; `-copyts` keeps them there, so the source itself
// must be shifted the same way to stay in sync with them.
fn source_offset(info: &MediaInfo) -> Option<String> {
	info.format
		.start_time
		.filter(|start| *start != 0.0)
		.map(|start| format!("{:.6}", -start))
}

fn push_maps(cmd: &mut ToolCommand, outputs: &[OutputStream], attachments: &[u32], source: usize) {
	for output in outputs {
		let spec = match output.input {
			MuxInput::VideoTemp(ordinal) => format!("0:v:{ordinal}"),
			MuxInput::AudioTemp(ordinal) => format!("1:a:{ordinal}"),
			MuxInput::Source => format!("{source}:{}", output.origin),
		};
		cmd.arg("-map").arg(spec);
	}

	for attachment in attachments {
		cmd.arg("-map").arg(format!("{source}:{attachment}"));
	}
}

fn push_statistics_reset(cmd: &mut ToolCommand, info: &MediaInfo, outputs: &[OutputStream]) {
	for (position, output) in outputs.iter().enumerate().filter(|(_, output)| output.is_encoded()) {
		let Some(stream) = source_stream(info, output.origin) else {
			continue;
		};

		for key in stream.tags.keys().filter(|key| is_statistics_tag(key)) {
			cmd.arg(format!("-metadata:s:{position}")).arg(format!("{key}="));
		}
	}
}

fn is_statistics_tag(key: &str) -> bool {
	let name = match key.rsplit_once('-') {
		Some((name, language))
			if (2..=3).contains(&language.len()) && language.chars().all(|c| c.is_ascii_alphabetic()) =>
		{
			name
		}
		_ => key,
	};
	STATISTICS_TAGS.iter().any(|tag| tag.eq_ignore_ascii_case(name))
}

fn push_clean_metadata(cmd: &mut ToolCommand, plan: &MuxPlan<'_>, outputs: &[OutputStream]) {
	cmd.args(["-map_metadata:g", "-1", "-map_metadata:s", "-1"]);

	for (position, output) in outputs.iter().enumerate() {
		let Some(stream) = source_stream(plan.info, output.origin) else {
			continue;
		};
		for key in CLEAN_STREAM_TAGS {
			if let Some(value) = stream.tag(key) {
				cmd.arg(format!("-metadata:s:{position}")).arg(format!("{key}={value}"));
			}
		}
	}

	for (ordinal, origin) in plan.streams.attachments.iter().enumerate() {
		let Some(stream) = source_stream(plan.info, *origin) else {
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

fn source_stream(info: &MediaInfo, index: u32) -> Option<&Stream> {
	info.streams.iter().find(|stream| stream.index == index)
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
	use crate::container::{MATROSKA_VIDEO, MP4_VIDEO, WEBM_VIDEO};
	use crate::video::joined;
	use crate::video::plan::test_plan;
	use crate::video::streams::SubtitleTrack;

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
					codec: None,
				},
				SubtitleTrack {
					stream_index: 4,
					codec: Some("srt"),
				},
			],
			attachments: vec![5],
			extracted: Vec::new(),
			warnings: Vec::new(),
		}
	}

	const INFO: &str = r#"{"streams":[
		{"index":0,"codec_type":"video","tags":{"title":"Angle 1","BPS-eng":"9000000","NUMBER_OF_FRAMES":"75"}},
		{"index":1,"codec_type":"audio","tags":{"language":"eng","title":"Main","_STATISTICS_TAGS":"BPS"}},
		{"index":2,"codec_type":"audio","tags":{"language":"jpn","BPS":"128000"}},
		{"index":3,"codec_type":"subtitle","tags":{"language":"slv","timecode":"01:02:03:04"}},
		{"index":4,"codec_type":"subtitle"},
		{"index":5,"codec_type":"attachment","tags":{"filename":"a.ttf","mimetype":"font/ttf"}}],
		"format":{"format_name":"mov,mp4,m4a,3gp,3g2,mj2"}}"#;

	fn info() -> MediaInfo {
		MediaInfo::parse(INFO.as_bytes()).unwrap()
	}

	fn build_with(info: &MediaInfo, keep_metadata: bool, alpha: bool, attachments: &[Attachment]) -> String {
		let video = VideoPlan { alpha, ..test_plan() };
		let streams = stream_plan();
		let inputs = MuxInputs {
			video: Path::new("video.a1.mkv"),
			audio: Some(Path::new("audio.mka")),
			source: Path::new("source.mp4"),
			source_bytes: 0,
			attachments,
		};
		let plan = MuxPlan {
			info,
			video: &video,
			streams: &streams,
			keep_metadata,
			container: MATROSKA_VIDEO,
			chapters: ChapterSource::Source,
		};
		joined(&command(Path::new("ffmpeg"), &inputs, &plan, Path::new("mux.mkv")))
	}

	fn build(keep_metadata: bool, alpha: bool, attachments: &[Attachment]) -> String {
		build_with(&info(), keep_metadata, alpha, attachments)
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
	fn output_streams_name_their_origin_and_input() {
		let video = VideoPlan {
			streams: vec![0, 6],
			..test_plan()
		};

		let outputs = output_streams(&video, &stream_plan());

		assert_eq!(
			outputs
				.iter()
				.map(|output| (output.origin, output.input))
				.collect::<Vec<_>>(),
			[
				(0, MuxInput::VideoTemp(0)),
				(6, MuxInput::VideoTemp(1)),
				(1, MuxInput::AudioTemp(0)),
				(2, MuxInput::Source),
				(3, MuxInput::Source),
				(4, MuxInput::Source),
			]
		);
		assert_eq!(
			outputs.iter().map(|output| output.is_encoded()).collect::<Vec<_>>(),
			[true, true, true, false, false, false]
		);
	}

	#[test]
	fn temps_keep_their_timestamps_and_the_source_is_shifted_by_its_start() {
		let mut delayed = info();
		delayed.format.start_time = Some(1.4);
		let mut zero = info();
		zero.format.start_time = Some(0.0);

		let shifted = build_with(&delayed, true, false, &[]);
		let unshifted = build_with(&zero, false, false, &[]);
		let unknown = build(false, false, &[]);

		assert!(shifted.starts_with("-hide_banner -nostdin -y -copyts -i "), "{shifted}");
		assert!(shifted.contains("-export_xmp 1 -itsoffset -1.400000 -i "), "{shifted}");
		assert!(unshifted.contains(" -copyts "), "{unshifted}");
		assert!(!unshifted.contains("-itsoffset"), "{unshifted}");
		assert!(!unknown.contains("-itsoffset"), "{unknown}");
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
	fn keep_mode_clears_source_statistics_on_encoded_streams_only() {
		let args = build(true, false, &[]);

		assert!(
			args.contains("-metadata:s:0 BPS-eng= -metadata:s:0 NUMBER_OF_FRAMES= -metadata:s:1 _STATISTICS_TAGS="),
			"{args}"
		);
		assert!(!args.contains("-metadata:s:2 BPS="), "{args}");
		assert!(!build(false, false, &[]).contains("BPS"));
	}

	#[test]
	fn statistics_tags_match_with_any_case_and_language_suffix() {
		assert!(is_statistics_tag("BPS"));
		assert!(is_statistics_tag("bps-eng"));
		assert!(is_statistics_tag("_STATISTICS_WRITING_DATE_UTC-de"));
		assert!(is_statistics_tag("Duration"));
		assert!(!is_statistics_tag("BPS-english"));
		assert!(!is_statistics_tag("title"));
		assert!(!is_statistics_tag("HANDLER_NAME"));
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
		let (info, video) = (info(), test_plan());
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
			source_bytes: 0,
			attachments: &[],
		};
		let plan = MuxPlan {
			info: &info,
			video: &video,
			streams: &streams,
			keep_metadata: true,
			container: MATROSKA_VIDEO,
			chapters: ChapterSource::Source,
		};

		let args = joined(&command(Path::new("ffmpeg"), &inputs, &plan, Path::new("mux.mkv")));

		assert!(args.contains("-map 0:v:0 -map 1:2 -c copy"), "{args}");
		assert!(args.contains("-map_metadata 1"), "{args}");
	}

	#[test]
	fn dropped_chapters_are_not_mapped() {
		let (info, video, streams) = (info(), test_plan(), StreamPlan::default());
		let inputs = MuxInputs {
			video: Path::new("video.a1.mkv"),
			audio: None,
			source: Path::new("source.mkv"),
			source_bytes: 0,
			attachments: &[],
		};
		let plan = MuxPlan {
			info: &info,
			video: &video,
			streams: &streams,
			keep_metadata: false,
			container: MATROSKA_VIDEO,
			chapters: ChapterSource::Dropped,
		};

		let args = joined(&command(Path::new("ffmpeg"), &inputs, &plan, Path::new("mux.mkv")));

		assert!(args.contains("-map_chapters -1 -cues_to_front 1"), "{args}");
	}

	#[test]
	fn webm_and_mp4_mux_with_their_own_options() {
		let (info, video) = (info(), test_plan());
		let streams = StreamPlan {
			subtitles: vec![SubtitleTrack {
				stream_index: 3,
				codec: Some("webvtt"),
			}],
			..StreamPlan::default()
		};
		let inputs = MuxInputs {
			video: Path::new("video.a1.mkv"),
			audio: None,
			source: Path::new("source.mkv"),
			source_bytes: 0,
			attachments: &[],
		};
		let muxed = |container| {
			joined(&command(
				Path::new("ffmpeg"),
				&inputs,
				&MuxPlan {
					info: &info,
					video: &video,
					streams: &streams,
					keep_metadata: true,
					container,
					chapters: ChapterSource::Source,
				},
				Path::new("mux.out"),
			))
		};

		let webm = muxed(WEBM_VIDEO);
		let mp4 = muxed(MP4_VIDEO);

		assert!(webm.contains("-c copy -c:s:0 webvtt"), "{webm}");
		assert!(webm.contains("-cues_to_front 1 -f webm"), "{webm}");
		assert!(mp4.contains("-movflags +faststart -f mp4"), "{mp4}");
		assert!(!mp4.contains("cues_to_front"), "{mp4}");
	}
}
