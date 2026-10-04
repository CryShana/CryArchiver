use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;

use crate::audio::plan::TrackPlan;
use crate::container::{Container, MP4_VIDEO};
use crate::options::TrimWindow;
use crate::outcome::JobError;
use crate::probe::{MediaInfo, Stream};
use crate::process::{self, ToolCommand};
use crate::video::plan::VideoPlan;
use crate::video::streams::{CueFormat, StreamPlan};
use crate::video::{run_ffmpeg, titles};

const BRAND_TAGS: &[&str] = &["major_brand", "minor_version", "compatible_brands"];
pub(crate) const CLEAN_STREAM_TAGS: &[&str] = &["language", "title"];
const ATTACHMENT_TAGS: &[&str] = &["filename", "mimetype"];
const CUE_DISPOSITIONS: &[&str] = &["default", "forced", "hearing_impaired", "visual_impaired", "comment"];
const DVD_SUBTITLE: &str = "dvd_subtitle";
/// A copied track under a trim starts at the packet before the cut, whose negative time would move the file's start
/// to it, so that packet is dropped (see `bitmap_cut`).
const LEADING_PACKET_DROP: &str = "noise=drop=abs(pts)-pts";
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
	pub chapters: Option<&'a Path>,
	pub cues: &'a [CueFile],
}

/// A subtitle track cut to the trim, muxed in place of the source's stream `origin`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CueFile {
	pub origin: u32,
	pub path: PathBuf,
	pub format: CueFormat,
}

pub struct MuxPlan<'a> {
	pub info: &'a MediaInfo,
	pub video: &'a VideoPlan,
	pub streams: &'a StreamPlan,
	pub keep_metadata: bool,
	pub container: Container,
	pub chapters: ChapterSource,
	pub window: Option<TrimWindow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChapterSource {
	Source,
	File,
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
	if let Some(window) = plan.window {
		cmd.seek(window.start_s, window.length_s);
	}

	if let Some(offset) = source_offset(plan.info, plan.window) {
		cmd.arg("-itsoffset").arg(offset);
	}
	cmd.arg("-i").arg(process::tool_path(inputs.source));

	let chapter_file = source + 1;
	let mut next_input = chapter_file;
	if plan.chapters == ChapterSource::File
		&& let Some(chapters) = inputs.chapters
	{
		cmd.args(["-f", "ffmetadata", "-i"]).arg(process::tool_path(chapters));
		next_input += 1;
	}

	let mut cue_inputs = Vec::new();
	for (cue, input) in inputs.cues.iter().zip(next_input..) {
		cmd.args(["-f", cue.format.extension(), "-i"])
			.arg(process::tool_path(&cue.path));
		cue_inputs.push((cue.origin, input));
	}

	let outputs = output_streams(plan.video, plan.streams);
	push_maps(&mut cmd, &outputs, &plan.streams.attachments, source, &cue_inputs);
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

	if let Some(window) = plan.window {
		push_copied_cuts(&mut cmd, plan, window);
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
		push_statistics_reset(&mut cmd, plan, &outputs);
		push_encoded_titles(&mut cmd, plan, &outputs);
	} else {
		push_clean_metadata(&mut cmd, plan, &outputs);
	}
	push_cue_tags(&mut cmd, plan, &outputs, &cue_inputs);
	if plan.container.muxer == MP4_VIDEO.muxer {
		push_track_names(&mut cmd, plan, &outputs);
	}

	match plan.chapters {
		ChapterSource::Source => cmd.arg("-map_chapters").arg(source.to_string()),
		ChapterSource::File => cmd.arg("-map_chapters").arg(chapter_file.to_string()),
		ChapterSource::Dropped => cmd.args(["-map_chapters", "-1"]),
	};
	if plan.container.muxer == MP4_VIDEO.muxer {
		cmd.args(["-movflags", "+faststart"]);
	} else {
		cmd.args(["-cues_to_front", "1"]);
	}

	// Under `-copyts` the source's `-t` doesn't end at the window's end, so the output is cut here.
	if let Some(window) = plan.window {
		cmd.arg("-t").arg(format!("{:.3}", window.length_s));
	}
	cmd.arg("-f").arg(plan.container.muxer).arg(process::tool_path(output));
	cmd
}

pub async fn mux(cmd: &ToolCommand, cancel: &CancellationToken) -> Result<(), JobError> {
	run_ffmpeg(cmd, |_| {}, cancel).await.map(drop)
}

// The temps were cut from the source shifted by its start time and the trim's start; `-copyts` keeps them there, so
// the source itself must be shifted the same way to stay in sync with them.
fn source_offset(info: &MediaInfo, window: Option<TrimWindow>) -> Option<String> {
	let shift = info.format.start_time.unwrap_or(0.0) + window.map_or(0.0, |window| window.start_s);
	(shift.abs() > f64::EPSILON).then(|| format!("{:.6}", -shift))
}

/// Under a trim, every subtitle track that isn't cut to a file is a bitmap one, copied from the seeked source. The
/// seek can read back to a cue still on screen at the cut, which would move the file's start to that cue, so a packet
/// that starts before the window (a negative `pts` here) is dropped, and one running past the end is cut short there.
///
/// A DVD packet (`own_stop`) also carries its stop time in its payload, which decoders honour over the container's
/// duration, so one running past the end is dropped instead; its container duration stands for that stop time.
///
/// `noise` drops a packet on any nonzero value, negative too, so `abs(pts)-pts` is 0 unless `pts` is negative, and
/// `x+abs(x)` is 0 unless `x` is positive. `lt`, `max` and `min` are spelled out because a comma would split the
/// filter list.
fn bitmap_cut(window: TrimWindow, own_stop: bool) -> String {
	if own_stop {
		let end = format!("({:.3}/tb+1)", window.length_s);
		return format!("noise=drop=abs(pts)+duration-{end}+abs(pts+duration-{end})");
	}

	let end = format!("{:.3}/TB", window.length_s);
	format!("noise=drop=abs(pts)-pts,setts=duration=(DURATION+{end}-PTS-abs(DURATION-{end}+PTS))/2")
}

fn push_copied_cuts(cmd: &mut ToolCommand, plan: &MuxPlan<'_>, window: TrimWindow) {
	for (ordinal, _) in plan
		.streams
		.audio
		.iter()
		.enumerate()
		.filter(|(_, track)| track.plan == TrackPlan::Copy)
	{
		cmd.arg(format!("-bsf:a:{ordinal}")).arg(LEADING_PACKET_DROP);
	}

	for (ordinal, subtitle) in plan
		.streams
		.subtitles
		.iter()
		.enumerate()
		.filter(|(_, subtitle)| subtitle.clipped.is_none())
	{
		let own_stop =
			source_stream(plan.info, subtitle.stream_index).is_some_and(|stream| stream.codec_name == DVD_SUBTITLE);
		cmd.arg(format!("-bsf:s:{ordinal}")).arg(bitmap_cut(window, own_stop));
	}
}

fn push_maps(
	cmd: &mut ToolCommand,
	outputs: &[OutputStream],
	attachments: &[u32],
	source: usize,
	cue_inputs: &[(u32, usize)],
) {
	for output in outputs {
		let cut = cue_inputs.iter().find(|(origin, _)| *origin == output.origin);
		let spec = match (output.input, cut) {
			(MuxInput::VideoTemp(ordinal), _) => format!("0:v:{ordinal}"),
			(MuxInput::AudioTemp(ordinal), _) => format!("1:a:{ordinal}"),
			(MuxInput::Source, Some((_, input))) => format!("{input}:0"),
			(MuxInput::Source, None) => format!("{source}:{}", output.origin),
		};
		cmd.arg("-map").arg(spec);
	}

	for attachment in attachments {
		cmd.arg("-map").arg(format!("{source}:{attachment}"));
	}
}

/// The statistics of an encoded stream, or of any stream under a trim, describe the source's and not the output's.
fn push_statistics_reset(cmd: &mut ToolCommand, plan: &MuxPlan<'_>, outputs: &[OutputStream]) {
	let trimmed = plan.window.is_some();
	for (position, output) in outputs
		.iter()
		.enumerate()
		.filter(|(_, output)| trimmed || output.is_encoded())
	{
		let Some(stream) = source_stream(plan.info, output.origin) else {
			continue;
		};

		for key in stream.tags.keys().filter(|key| is_statistics_tag(key)) {
			cmd.arg(format!("-metadata:s:{position}")).arg(format!("{key}="));
		}
	}
}

fn push_encoded_titles(cmd: &mut ToolCommand, plan: &MuxPlan<'_>, outputs: &[OutputStream]) {
	for (position, output) in outputs.iter().enumerate() {
		let Some(stream) = source_stream(plan.info, output.origin) else {
			continue;
		};

		if let Some(title) = encoded_title(plan.streams, *output, stream) {
			cmd.arg(format!("-metadata:s:{position}")).arg(format!("title={title}"));
		}
	}
}

/// The title of an encoded audio track, rewritten for Opus and its new channels; `None` when it stays as it is.
fn encoded_title(streams: &StreamPlan, output: OutputStream, stream: &Stream) -> Option<String> {
	if !matches!(output.input, MuxInput::AudioTemp(_)) || streams.merge_from.first() == Some(&output.origin) {
		return None;
	}

	let encoded = streams.audio.iter().find_map(|track| match &track.plan {
		TrackPlan::Encode(encoded) if track.stream_index == output.origin => Some(encoded),
		_ => None,
	})?;
	titles::encoded_title(stream, encoded)
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
		if plan.streams.merge_from.first() == Some(&output.origin) {
			if let Some(language) = &plan.streams.merge_language {
				cmd.arg(format!("-metadata:s:{position}"))
					.arg(format!("language={language}"));
			}
			continue;
		}

		let Some(stream) = source_stream(plan.info, output.origin) else {
			continue;
		};
		let title = encoded_title(plan.streams, *output, stream);
		for key in CLEAN_STREAM_TAGS {
			let value = match (*key, &title) {
				("title", Some(title)) => Some(title.as_str()),
				_ => stream.tag(key),
			};
			if let Some(value) = value {
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

/// A cut file has none of the source stream's tags or flags, so they are set from the source here. Clean mode has
/// already set the tags it keeps.
fn push_cue_tags(cmd: &mut ToolCommand, plan: &MuxPlan<'_>, outputs: &[OutputStream], cue_inputs: &[(u32, usize)]) {
	for (position, output) in outputs.iter().enumerate() {
		if !cue_inputs.iter().any(|(origin, _)| *origin == output.origin) {
			continue;
		}
		let Some(stream) = source_stream(plan.info, output.origin) else {
			continue;
		};

		if plan.keep_metadata {
			for key in CLEAN_STREAM_TAGS {
				if let Some(value) = stream.tag(key) {
					cmd.arg(format!("-metadata:s:{position}")).arg(format!("{key}={value}"));
				}
			}
		}

		let flags: Vec<&str> = CUE_DISPOSITIONS
			.iter()
			.copied()
			.filter(|flag| stream.disposition.get(*flag).is_some_and(|set| *set != 0))
			.collect();
		let flags = if flags.is_empty() {
			"0".to_owned()
		} else {
			flags.join("+")
		};
		cmd.arg(format!("-disposition:{position}")).arg(flags);
	}
}

/// The mp4 muxer stores a `title` where readers see it only as `name`; `handler_name` is read back as it was written.
fn push_track_names(cmd: &mut ToolCommand, plan: &MuxPlan<'_>, outputs: &[OutputStream]) {
	for (position, output) in outputs.iter().enumerate() {
		let Some(stream) = source_stream(plan.info, output.origin) else {
			continue;
		};

		let merged = plan.streams.merge_from.first() == Some(&output.origin);
		let title = match stream.codec_type.as_str() {
			"subtitle" => stream.tag("title").map(str::to_owned),
			"audio" if !merged => {
				encoded_title(plan.streams, *output, stream).or_else(|| stream.tag("title").map(str::to_owned))
			}
			_ => None,
		};
		if let Some(title) = title {
			cmd.arg(format!("-metadata:s:{position}"))
				.arg(format!("handler_name={title}"));
		}
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
					clipped: None,
				},
				SubtitleTrack {
					stream_index: 4,
					codec: Some("srt"),
					clipped: None,
				},
			],
			attachments: vec![5],
			extracted: Vec::new(),
			warnings: Vec::new(),
			merge_from: Vec::new(),
			merge_language: None,
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
			chapters: None,
			cues: &[],
		};
		let plan = MuxPlan {
			info,
			video: &video,
			streams: &streams,
			keep_metadata,
			container: MATROSKA_VIDEO,
			chapters: ChapterSource::Source,
			window: None,
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
	fn a_trimmed_source_is_seeked_shifted_and_gets_its_chapters_from_a_file() {
		let mut delayed = info();
		delayed.format.start_time = Some(1.4);
		let (video, streams) = (test_plan(), stream_plan());
		let inputs = MuxInputs {
			video: Path::new("video.a1.mkv"),
			audio: Some(Path::new("audio.mka")),
			source: Path::new("source.mp4"),
			source_bytes: 0,
			attachments: &[],
			chapters: Some(Path::new("chapters.txt")),
			cues: &[],
		};
		let plan = MuxPlan {
			info: &delayed,
			video: &video,
			streams: &streams,
			keep_metadata: false,
			container: MATROSKA_VIDEO,
			chapters: ChapterSource::File,
			window: Some(TrimWindow {
				start_s: 4.0,
				length_s: 4.0,
			}),
		};

		let args = joined(&command(Path::new("ffmpeg"), &inputs, &plan, Path::new("mux.mkv")));

		assert!(args.contains(" -ss 4.000 -t 4.000 -itsoffset -5.400000 -i "), "{args}");
		assert!(args.contains(" -f ffmetadata -i "), "{args}");
		assert!(args.contains("-map_chapters 3 -cues_to_front 1"), "{args}");
		assert!(args.contains(" -t 4.000 -f matroska "), "{args}");
		assert!(args.contains(" -bsf:a:1 noise=drop=abs(pts)-pts "), "{args}");
		assert!(!args.contains("-bsf:a:0"), "{args}");
	}

	#[test]
	fn a_trimmed_dvd_track_drops_a_cue_running_past_the_end() {
		let mut dvd = info();
		dvd.streams[3].codec_name = "dvd_subtitle".into();
		let (video, streams) = (test_plan(), stream_plan());
		let inputs = MuxInputs {
			video: Path::new("video.a1.mkv"),
			audio: Some(Path::new("audio.mka")),
			source: Path::new("source.mkv"),
			source_bytes: 0,
			attachments: &[],
			chapters: None,
			cues: &[],
		};
		let plan = MuxPlan {
			info: &dvd,
			video: &video,
			streams: &streams,
			keep_metadata: false,
			container: MATROSKA_VIDEO,
			chapters: ChapterSource::Dropped,
			window: Some(TrimWindow {
				start_s: 4.0,
				length_s: 4.0,
			}),
		};

		let args = joined(&command(Path::new("ffmpeg"), &inputs, &plan, Path::new("mux.mkv")));

		assert!(
			args.contains(
				"-bsf:s:0 noise=drop=abs(pts)+duration-(4.000/tb+1)+abs(pts+duration-(4.000/tb+1)) -bsf:s:1 \
				 noise=drop=abs(pts)-pts,setts="
			),
			"{args}"
		);
	}

	#[test]
	fn cut_cues_are_muxed_from_their_own_files_with_the_source_tags() {
		let mut tagged = info();
		tagged.streams[4].disposition.insert("forced".into(), 1);
		tagged.streams[4].tags.insert("language".into(), "deu".into());
		let video = test_plan();
		let streams = StreamPlan {
			subtitles: vec![
				SubtitleTrack {
					stream_index: 3,
					codec: None,
					clipped: None,
				},
				SubtitleTrack {
					stream_index: 4,
					codec: None,
					clipped: Some(CueFormat::Ass),
				},
			],
			..StreamPlan::default()
		};
		let cues = [CueFile {
			origin: 4,
			path: PathBuf::from("cues-4.ass"),
			format: CueFormat::Ass,
		}];
		let inputs = MuxInputs {
			video: Path::new("video.a1.mkv"),
			audio: None,
			source: Path::new("source.mkv"),
			source_bytes: 0,
			attachments: &[],
			chapters: None,
			cues: &cues,
		};
		let plan = |keep_metadata| MuxPlan {
			info: &tagged,
			video: &video,
			streams: &streams,
			keep_metadata,
			container: MATROSKA_VIDEO,
			chapters: ChapterSource::Dropped,
			window: Some(TrimWindow {
				start_s: 4.0,
				length_s: 4.0,
			}),
		};

		let kept = joined(&command(
			Path::new("ffmpeg"),
			&inputs,
			&plan(true),
			Path::new("mux.mkv"),
		));
		let clean = joined(&command(
			Path::new("ffmpeg"),
			&inputs,
			&plan(false),
			Path::new("mux.mkv"),
		));

		let cue_input = format!(" -f ass -i {} ", process::tool_path(Path::new("cues-4.ass")).display());
		assert!(kept.contains(&cue_input), "{kept}");
		assert!(kept.contains("-map 0:v:0 -map 1:3 -map 2:0 -c copy"), "{kept}");
		assert!(
			kept.contains("-metadata:s:2 language=deu -disposition:2 forced"),
			"{kept}"
		);
		assert!(!kept.contains("-disposition:1"), "{kept}");
		assert!(
			kept.contains(
				"-bsf:s:0 noise=drop=abs(pts)-pts,setts=duration=(DURATION+4.000/TB-PTS-abs(DURATION-4.000/TB+PTS))/2"
			),
			"{kept}"
		);
		assert!(!kept.contains("-bsf:s:1"), "{kept}");
		assert!(!build(true, false, &[]).contains("-bsf"));
		assert!(clean.contains("-metadata:s:2 language=deu"), "{clean}");
		assert_eq!(clean.matches("language=deu").count(), 1, "{clean}");
		assert!(clean.contains("-disposition:2 forced"), "{clean}");
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
	fn keep_mode_under_a_trim_clears_the_statistics_of_copied_streams_too() {
		let (info, video, streams) = (info(), test_plan(), stream_plan());
		let inputs = MuxInputs {
			video: Path::new("video.a1.mkv"),
			audio: Some(Path::new("audio.mka")),
			source: Path::new("source.mkv"),
			source_bytes: 0,
			attachments: &[],
			chapters: None,
			cues: &[],
		};
		let plan = MuxPlan {
			info: &info,
			video: &video,
			streams: &streams,
			keep_metadata: true,
			container: MATROSKA_VIDEO,
			chapters: ChapterSource::Dropped,
			window: Some(TrimWindow {
				start_s: 4.0,
				length_s: 4.0,
			}),
		};

		let args = joined(&command(Path::new("ffmpeg"), &inputs, &plan, Path::new("mux.mkv")));

		assert!(
			args.contains("-metadata:s:1 _STATISTICS_TAGS= -metadata:s:2 BPS="),
			"{args}"
		);
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
	fn a_merged_track_keeps_its_language_but_drops_its_title() {
		let merged_track = TrackJob {
			stream_index: 1,
			sample_rate: 48_000,
			plan: TrackPlan::Encode(EncodePlan {
				bitrate_kbps: 128,
				channels: 2,
				layout: LayoutFix::Keep("stereo"),
			}),
		};
		let streams = StreamPlan {
			audio: vec![merged_track],
			merge_from: vec![1, 2],
			merge_language: Some("eng".into()),
			..stream_plan()
		};
		let video = test_plan();
		let inputs = MuxInputs {
			video: Path::new("video.a1.mkv"),
			audio: Some(Path::new("audio.mka")),
			source: Path::new("source.mp4"),
			source_bytes: 0,
			attachments: &[],
			chapters: None,
			cues: &[],
		};
		let plan = MuxPlan {
			info: &info(),
			video: &video,
			streams: &streams,
			keep_metadata: false,
			container: MATROSKA_VIDEO,
			chapters: ChapterSource::Source,
			window: None,
		};

		let args = joined(&command(Path::new("ffmpeg"), &inputs, &plan, Path::new("mux.mkv")));

		assert!(args.contains("-metadata:s:1 language=eng"), "{args}");
		assert!(!args.contains("title=Main"), "{args}");
	}

	#[test]
	fn encoded_tracks_get_their_titles_rewritten_in_both_modes() {
		let mut tagged = info();
		tagged.streams[1].codec_name = "ac3".into();
		tagged.streams[1].channel_layout = Some("5.1(side)".into());
		tagged.streams[1].tags.insert("title".into(), "English 5.1 AC3".into());
		tagged.streams[2]
			.tags
			.insert("title".into(), "Japanese 2.0 FLAC".into());

		let clean = build_with(&tagged, false, false, &[]);
		let kept = build_with(&tagged, true, false, &[]);

		assert!(clean.contains("-metadata:s:1 title=English 2.0 Opus"), "{clean}");
		assert!(!clean.contains("title=English 5.1 AC3"), "{clean}");
		assert!(clean.contains("-metadata:s:2 title=Japanese 2.0 FLAC"), "{clean}");
		assert!(kept.contains("-metadata:s:1 title=English 2.0 Opus"), "{kept}");
		assert!(!kept.contains("title=Japanese"), "{kept}");
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
			chapters: None,
			cues: &[],
		};
		let plan = MuxPlan {
			info: &info,
			video: &video,
			streams: &streams,
			keep_metadata: true,
			container: MATROSKA_VIDEO,
			chapters: ChapterSource::Source,
			window: None,
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
			chapters: None,
			cues: &[],
		};
		let plan = MuxPlan {
			info: &info,
			video: &video,
			streams: &streams,
			keep_metadata: false,
			container: MATROSKA_VIDEO,
			chapters: ChapterSource::Dropped,
			window: None,
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
				clipped: None,
			}],
			..StreamPlan::default()
		};
		let inputs = MuxInputs {
			video: Path::new("video.a1.mkv"),
			audio: None,
			source: Path::new("source.mkv"),
			source_bytes: 0,
			attachments: &[],
			chapters: None,
			cues: &[],
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
					window: None,
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
