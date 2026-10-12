use crate::audio::plan::{
	TrackJob, TrackPlan, merge_language, mergeable, merged, mixed_languages_warning, plan_track_with,
};
use crate::container::{Container, MATROSKA_VIDEO};
use crate::outcome::{KeepReason, Refusal};
use crate::preset::{Overrides, Speed};
use crate::probe::{MediaInfo, Stream};
use crate::video::burn::Burn;

const MKV_SUBTITLES: &[&str] = &[
	"subrip",
	"srt",
	"ass",
	"ssa",
	"webvtt",
	"dvd_subtitle",
	"hdmv_pgs_subtitle",
	"dvb_subtitle",
	"hdmv_text_subtitle",
	"arib_caption",
];
const TEXT_SUBTITLES: &[&str] = &[
	"mov_text",
	"text",
	"microdvd",
	"sami",
	"realtext",
	"subviewer",
	"subviewer1",
	"jacosub",
	"mpl2",
	"pjs",
	"vplayer",
	"stl",
	"eia_608",
];
const DATA_MIME: &str = "application/octet-stream";
const MKV_TEXT: &[&str] = &["subrip", "srt", "ass", "ssa", "webvtt"];
const WEBM_AUDIO: &[&str] = &["opus", "vorbis"];
const MP4_AUDIO: &[&str] = &["aac", "ac3", "eac3", "mp3", "opus", "flac", "alac"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CueFormat {
	Srt,
	Ass,
}

impl CueFormat {
	pub fn extension(self) -> &'static str {
		match self {
			Self::Srt => "srt",
			Self::Ass => "ass",
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleTrack {
	pub stream_index: u32,
	pub codec: Option<&'static str>,
	/// Under a trim, a text track is cut in this format and muxed from the cut file instead of the source.
	pub clipped: Option<CueFormat>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct StreamRules {
	pub speed: Speed,
	pub overrides: Overrides,
	pub keep_metadata: bool,
	pub container: Container,
	pub strip: bool,
	pub trim: bool,
	/// Under a trim, whether it starts after the source's start and whether it ends before the source's end.
	pub cut_start: bool,
	pub cut_end: bool,
	pub merge: bool,
	pub burn: Option<Burn>,
	pub forced: bool,
}

impl StreamRules {
	pub fn new(speed: Speed, keep_metadata: bool) -> Self {
		Self {
			speed,
			overrides: Overrides::default(),
			keep_metadata,
			container: MATROSKA_VIDEO,
			strip: false,
			trim: false,
			cut_start: false,
			cut_end: false,
			merge: false,
			burn: None,
			forced: false,
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
	Matroska,
	Webm,
	Mp4,
}

impl Target {
	fn of(container: Container) -> Self {
		match container.muxer {
			"webm" => Self::Webm,
			"mp4" => Self::Mp4,
			_ => Self::Matroska,
		}
	}

	fn name(self) -> &'static str {
		match self {
			Self::Matroska => "MKV",
			Self::Webm => "WebM",
			Self::Mp4 => "MP4",
		}
	}

	fn holds_files(self) -> bool {
		self == Self::Matroska
	}

	fn copies(self, codec: &str) -> bool {
		match self {
			Self::Matroska => true,
			Self::Webm => WEBM_AUDIO.contains(&codec),
			Self::Mp4 => MP4_AUDIO.contains(&codec),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extracted {
	pub stream_index: u32,
	pub file_name: String,
	pub mime_type: String,
}

impl Extracted {
	pub fn work_file(&self) -> String {
		format!("stream-{}.bin", self.stream_index)
	}
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamPlan {
	pub audio: Vec<TrackJob>,
	pub subtitles: Vec<SubtitleTrack>,
	pub attachments: Vec<u32>,
	pub extracted: Vec<Extracted>,
	pub warnings: Vec<String>,
	pub merge_from: Vec<u32>,
	pub merge_language: Option<String>,
}

impl StreamPlan {
	pub fn encoded_audio(&self) -> Vec<TrackJob> {
		self.audio
			.iter()
			.filter(|track| matches!(track.plan, TrackPlan::Encode(_)))
			.cloned()
			.collect()
	}
}

pub fn plan_streams(info: &MediaInfo, rules: &StreamRules) -> Result<StreamPlan, Refusal> {
	let target = Target::of(rules.container);
	let mut plan = StreamPlan::default();
	let mut audio_tracks = 0;
	let mut dropped_data = 0;
	let mut dropped_files = 0;
	let mut stripped = 0;
	let will_merge = rules.merge && mergeable(info);

	for stream in &info.streams {
		if rules.strip && !(stream.is_audio() || stream.is_real_video()) {
			stripped += 1;
			continue;
		}

		match stream.codec_type.as_str() {
			"audio" => {
				audio_tracks += 1;
				plan_audio_stream(stream, audio_tracks, rules, target, will_merge, &mut plan)?;
			}
			"subtitle" if rules.burn.is_some_and(|burn| burn.stream == stream.index) => {}
			"subtitle" => match subtitle_track(stream, target, rules.forced)? {
				Some(track) => {
					plan.warnings.extend(styling_warning(stream, target));
					let track = if rules.trim {
						trimmed_subtitle(track, stream, (target, rules), &mut plan.warnings)
					} else {
						track
					};
					plan.subtitles.push(track);
				}
				None => plan.warnings.push(dropped_subtitle(stream, target)),
			},
			"attachment" if target.holds_files() => plan.attachments.push(stream.index),
			"attachment" => dropped_files += 1,
			"video" if stream.is_attached_pic() && !target.holds_files() => dropped_files += 1,
			"video" if stream.is_attached_pic() => match cover(stream) {
				Some(cover) => plan.extracted.push(cover),
				None => plan.warnings.push(format!(
					"cover art not copied: {} pictures can't be attached",
					stream.codec_name
				)),
			},
			"data" if is_chapter_track(stream, info) => {}
			"data" if rules.keep_metadata && target.holds_files() && !rules.trim => plan.extracted.push(Extracted {
				stream_index: stream.index,
				file_name: format!("stream-{}-{}.bin", stream.index, data_codec(stream)),
				mime_type: DATA_MIME.into(),
			}),
			"data" => dropped_data += 1,
			_ => {}
		}
	}

	if rules.merge
		&& let Some((track, sources)) = merged(info, &plan.audio, rules.speed, &rules.overrides)
	{
		plan.merge_language = merge_language(info, &sources);
		plan.warnings.extend(mixed_languages_warning(info, &sources));
		plan.audio = vec![track];
		plan.merge_from = sources;
	}

	let plural = |count: usize| if count == 1 { "" } else { "s" };
	if dropped_data > 0 {
		let why = if rules.trim {
			": data streams can't be trimmed".to_owned()
		} else if target.holds_files() {
			" (use --keep-metadata to keep them)".to_owned()
		} else {
			format!(": {} can't hold them", target.name())
		};
		plan.warnings.push(format!(
			"{dropped_data} data stream{} dropped{why}",
			plural(dropped_data)
		));
	}

	if dropped_files > 0 {
		plan.warnings.push(format!(
			"{dropped_files} attachment{} or cover{} dropped: {} can't hold them",
			plural(dropped_files),
			plural(dropped_files),
			target.name()
		));
	}

	if stripped > 0 {
		plan.warnings.push(format!(
			"--strip left out {stripped} subtitle, attachment, cover and data streams"
		));
	}
	Ok(plan)
}

/// A track that will be merged skips its own copy/encode decision entirely: no "copied as-is" warning and no
/// container refusal, since the merge replaces it with the mixed track regardless.
fn plan_audio_stream(
	stream: &Stream,
	audio_tracks: usize,
	rules: &StreamRules,
	target: Target,
	will_merge: bool,
	plan: &mut StreamPlan,
) -> Result<(), Refusal> {
	if stream.channels.unwrap_or(0) == 0 {
		plan.warnings
			.push(format!("audio track {audio_tracks} has no audio data and was dropped"));
		return Ok(());
	}

	if will_merge {
		plan.audio.push(TrackJob {
			stream_index: stream.index,
			sample_rate: stream.sample_rate.unwrap_or(0),
			plan: TrackPlan::Copy,
		});
		return Ok(());
	}

	let track = audio_track(stream, audio_tracks, rules, &mut plan.warnings);
	if track.plan == TrackPlan::Copy && !target.copies(&stream.codec_name) {
		return Err(Refusal::Impossible(format!(
			"audio track {audio_tracks} ({}) can't be stored in {} as it is",
			stream.codec_name,
			target.name()
		)));
	}
	plan.audio.push(track);
	Ok(())
}

fn is_text(codec: &str) -> bool {
	MKV_TEXT.contains(&codec) || TEXT_SUBTITLES.contains(&codec)
}

/// How a track is written: as it is, or converted to a codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Written {
	Copied,
	As(&'static str),
}

/// How a subtitle track is written to Matroska; `None` when Matroska can't hold it.
pub(crate) fn matroska_subtitle(codec: &str) -> Option<Written> {
	if MKV_SUBTITLES.contains(&codec) {
		Some(Written::Copied)
	} else if TEXT_SUBTITLES.contains(&codec) {
		Some(Written::As("srt"))
	} else {
		None
	}
}

/// A track Matroska can't hold keeps the original unless the job is `forced`; the other containers always drop what
/// they can't hold.
fn subtitle_track(stream: &Stream, target: Target, forced: bool) -> Result<Option<SubtitleTrack>, Refusal> {
	let codec = stream.codec_name.as_str();
	let text = is_text(codec);
	let track = |converted: Option<&'static str>| {
		Ok(Some(SubtitleTrack {
			stream_index: stream.index,
			codec: converted,
			clipped: None,
		}))
	};

	match target {
		Target::Matroska => match matroska_subtitle(codec) {
			Some(Written::Copied) => track(None),
			Some(Written::As(codec)) => track(Some(codec)),
			None if forced => Ok(None),
			None => Err(Refusal::Keep(KeepReason::Unsupported(format!(
				"subtitle codec {codec} can't be stored in MKV"
			)))),
		},
		Target::Webm if text => track((codec != "webvtt").then_some("webvtt")),
		Target::Mp4 if text => track((codec != "mov_text").then_some("mov_text")),
		Target::Webm | Target::Mp4 => Ok(None),
	}
}

fn dropped_subtitle(stream: &Stream, target: Target) -> String {
	let why = match target {
		Target::Matroska => "MKV can't hold it".to_owned(),
		Target::Webm | Target::Mp4 => format!(
			"{} holds only text subtitles; --burn-subs can draw it into the picture",
			target.name()
		),
	};

	format!(
		"subtitle stream {} ({}) dropped: {why}",
		stream.index, stream.codec_name
	)
}

fn styling_warning(stream: &Stream, target: Target) -> Option<String> {
	let format = match target {
		Target::Webm => "WebVTT",
		Target::Mp4 => "MP4 text",
		Target::Matroska => return None,
	};

	matches!(stream.codec_name.as_str(), "ass" | "ssa").then(|| {
		format!(
			"{} subtitles become {format}: styling and fonts can't be kept in WebM/MP4",
			stream.codec_name.to_ascii_uppercase()
		)
	})
}

/// A cue on screen at the start of a trim is dropped. At its end, a DVD cue is dropped too, because its stop time is
/// in its payload; any other is cut short, which needs no word.
fn bitmap_cut_warning(codec: &str, rules: &StreamRules) -> Option<String> {
	let end = rules.cut_end && codec == "dvd_subtitle";
	let when = match (rules.cut_start, end) {
		(true, true) => "already on screen at the start of the trim, or still on screen at its end,",
		(true, false) => "already on screen at the start of the trim",
		(false, true) => "still on screen at the end of the trim",
		(false, false) => return None,
	};
	Some(format!("bitmap subtitles ({codec}): a cue {when} isn't kept"))
}

fn trimmed_subtitle(
	track: SubtitleTrack,
	stream: &Stream,
	(target, rules): (Target, &StreamRules),
	warnings: &mut Vec<String>,
) -> SubtitleTrack {
	if !is_text(&stream.codec_name) {
		warnings.extend(bitmap_cut_warning(&stream.codec_name, rules));
		return track;
	}

	let format = if matches!(stream.codec_name.as_str(), "ass" | "ssa") {
		CueFormat::Ass
	} else {
		CueFormat::Srt
	};
	SubtitleTrack {
		codec: match target {
			Target::Matroska => None,
			Target::Webm => Some("webvtt"),
			Target::Mp4 => Some("mov_text"),
		},
		clipped: Some(format),
		..track
	}
}

fn audio_track(stream: &Stream, track: usize, rules: &StreamRules, warnings: &mut Vec<String>) -> TrackJob {
	let copy = |warnings: &mut Vec<String>, reason: &str| {
		let cut = if rules.trim {
			" and is cut at the nearest packet"
		} else {
			""
		};
		warnings.push(format!("audio track {track} copied as-is: {reason}{cut}"));
		TrackJob {
			stream_index: stream.index,
			sample_rate: stream.sample_rate.unwrap_or(0),
			plan: TrackPlan::Copy,
		}
	};

	let Some(sample_rate) = stream.sample_rate.filter(|&rate| rate > 0) else {
		return copy(warnings, "unknown sample rate");
	};
	match plan_track_with(stream, None, rules.speed, &rules.overrides, !rules.trim) {
		Ok(plan) => TrackJob {
			stream_index: stream.index,
			sample_rate,
			plan,
		},
		Err(reason) => copy(warnings, &reason),
	}
}

fn cover(stream: &Stream) -> Option<Extracted> {
	let (extension, mime_type) = match stream.codec_name.as_str() {
		"mjpeg" => ("jpg", "image/jpeg"),
		"png" => ("png", "image/png"),
		"bmp" => ("bmp", "image/bmp"),
		"webp" => ("webp", "image/webp"),
		"gif" => ("gif", "image/gif"),
		_ => return None,
	};

	Some(Extracted {
		stream_index: stream.index,
		file_name: stream
			.tag("filename")
			.map_or_else(|| format!("cover.{extension}"), str::to_owned),
		mime_type: stream.tag("mimetype").unwrap_or(mime_type).to_owned(),
	})
}

fn is_chapter_track(stream: &Stream, info: &MediaInfo) -> bool {
	stream.codec_name == "bin_data" && stream.codec_tag_string == "text" && !info.chapters.is_empty()
}

fn data_codec(stream: &Stream) -> String {
	let name = if stream.codec_name.is_empty() {
		stream.codec_tag_string.as_str()
	} else {
		stream.codec_name.as_str()
	};
	let clean: String = name.chars().filter(char::is_ascii_alphanumeric).collect();

	if clean.is_empty() { "data".to_owned() } else { clean }
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::audio::plan::{EncodePlan, LayoutFix};
	use crate::container::{MATROSKA_VIDEO, MP4_VIDEO, WEBM_VIDEO};
	use crate::video::burn::BurnKind;

	fn info(json: &str) -> MediaInfo {
		MediaInfo::parse(json.as_bytes()).unwrap()
	}

	fn rules(speed: Speed, keep_metadata: bool) -> StreamRules {
		StreamRules::new(speed, keep_metadata)
	}

	fn into(container: Container) -> StreamRules {
		StreamRules {
			container,
			..StreamRules::new(Speed::Moderate, true)
		}
	}

	const IPHONE: &str = r#"{"streams":[
		{"index":0,"codec_type":"video","codec_name":"hevc"},
		{"index":1,"codec_type":"audio","codec_name":"aac","sample_rate":"44100","channels":2,"channel_layout":"stereo","bit_rate":"160000"},
		{"index":2,"codec_type":"data","codec_tag_string":"mebx"},
		{"index":3,"codec_type":"data","codec_tag_string":"mebx"}]}"#;

	#[test]
	fn audio_is_planned_per_track() {
		let plan = plan_streams(&info(IPHONE), &rules(Speed::Moderate, false)).unwrap();

		assert_eq!(
			plan.audio,
			vec![TrackJob {
				stream_index: 1,
				sample_rate: 44_100,
				plan: TrackPlan::Encode(EncodePlan {
					bitrate_kbps: 128,
					channels: 2,
					layout: LayoutFix::Keep("stereo"),
				}),
			}]
		);
		assert_eq!(plan.encoded_audio().len(), 1);
	}

	#[test]
	fn data_streams_are_attached_only_when_keeping_metadata() {
		let kept = plan_streams(&info(IPHONE), &rules(Speed::Moderate, true)).unwrap();
		let clean = plan_streams(&info(IPHONE), &rules(Speed::Moderate, false)).unwrap();

		assert_eq!(
			kept.extracted,
			vec![
				Extracted {
					stream_index: 2,
					file_name: "stream-2-mebx.bin".into(),
					mime_type: "application/octet-stream".into(),
				},
				Extracted {
					stream_index: 3,
					file_name: "stream-3-mebx.bin".into(),
					mime_type: "application/octet-stream".into(),
				},
			]
		);
		assert!(kept.warnings.is_empty());
		assert!(clean.extracted.is_empty());
		assert_eq!(
			clean.warnings,
			["2 data streams dropped (use --keep-metadata to keep them)"]
		);
	}

	#[test]
	fn unencodable_audio_is_copied_with_a_warning() {
		let plan = plan_streams(
			&info(
				r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"},
				{"index":1,"codec_type":"audio","codec_name":"pcm_s16le","sample_rate":"48000","channels":10,"channel_layout":"5.1.4"},
				{"index":2,"codec_type":"audio","codec_name":"aac","channels":2},
				{"index":3,"codec_type":"audio","codec_name":"opus","sample_rate":"48000","channels":2,"channel_layout":"stereo"}]}"#,
			),
			&rules(Speed::Moderate, true),
		)
		.unwrap();

		assert!(plan.audio.iter().all(|track| track.plan == TrackPlan::Copy));
		assert_eq!(
			plan.audio.iter().map(|track| track.stream_index).collect::<Vec<_>>(),
			[1, 2, 3]
		);
		assert_eq!(plan.warnings.len(), 2, "{:?}", plan.warnings);
		assert!(plan.warnings[0].starts_with("audio track 1 copied as-is: 10 channels"));
		assert_eq!(plan.warnings[1], "audio track 2 copied as-is: unknown sample rate");
		assert!(plan.encoded_audio().is_empty());
	}

	#[test]
	fn covers_become_attachments_in_both_modes() {
		let mp4 = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"},
			{"index":2,"codec_type":"video","codec_name":"mjpeg","disposition":{"attached_pic":1}}]}"#,
		);
		let mkv = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"},
			{"index":4,"codec_type":"video","codec_name":"png","disposition":{"attached_pic":1},
			 "tags":{"filename":"small_cover.png","mimetype":"image/png"}}]}"#,
		);

		assert_eq!(
			plan_streams(&mp4, &rules(Speed::Moderate, false)).unwrap().extracted,
			vec![Extracted {
				stream_index: 2,
				file_name: "cover.jpg".into(),
				mime_type: "image/jpeg".into(),
			}]
		);
		let kept = plan_streams(&mkv, &rules(Speed::Moderate, true)).unwrap();
		assert_eq!(kept.extracted[0].file_name, "small_cover.png");
		assert_eq!(kept.extracted[0].work_file(), "stream-4.bin");
	}

	#[test]
	fn subtitles_and_fonts_follow_the_mkv_row() {
		let plan = plan_streams(
			&info(
				r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"},
				{"index":1,"codec_type":"subtitle","codec_name":"subrip"},
				{"index":2,"codec_type":"subtitle","codec_name":"mov_text"},
				{"index":3,"codec_type":"subtitle","codec_name":"hdmv_pgs_subtitle"},
				{"index":4,"codec_type":"attachment","codec_name":"ttf","tags":{"filename":"a.ttf"}}]}"#,
			),
			&rules(Speed::Moderate, false),
		)
		.unwrap();

		assert_eq!(
			plan.subtitles,
			vec![
				SubtitleTrack {
					stream_index: 1,
					codec: None,
					clipped: None
				},
				SubtitleTrack {
					stream_index: 2,
					codec: Some("srt"),
					clipped: None
				},
				SubtitleTrack {
					stream_index: 3,
					codec: None,
					clipped: None
				},
			]
		);
		assert_eq!(plan.attachments, [4]);
	}

	#[test]
	fn mp4_chapter_tracks_are_not_data() {
		let plan = plan_streams(
			&info(
				r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"},
				{"index":3,"codec_type":"data","codec_name":"bin_data","codec_tag_string":"text"}],
				"chapters":[{"id":1},{"id":2}]}"#,
			),
			&rules(Speed::Moderate, true),
		)
		.unwrap();

		assert!(plan.extracted.is_empty());
		assert!(plan.warnings.is_empty());
	}

	#[test]
	fn empty_audio_tracks_are_dropped_with_a_warning() {
		let plan = plan_streams(
			&info(
				r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"},
				{"index":1,"codec_type":"audio","codec_name":"mp2","sample_rate":"48000","channels":0},
				{"index":2,"codec_type":"audio","codec_name":"aac","sample_rate":"48000","channels":2,"channel_layout":"stereo"}]}"#,
			),
			&rules(Speed::Moderate, false),
		)
		.unwrap();

		assert_eq!(
			plan.audio.iter().map(|track| track.stream_index).collect::<Vec<_>>(),
			[2]
		);
		assert_eq!(plan.warnings, ["audio track 1 has no audio data and was dropped"]);
	}

	#[test]
	fn text_subtitles_become_srt_and_the_rest_of_the_mkv_row_is_copied() {
		let plan = plan_streams(
			&info(
				r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"},
				{"index":1,"codec_type":"subtitle","codec_name":"webvtt"},
				{"index":2,"codec_type":"subtitle","codec_name":"eia_608"},
				{"index":3,"codec_type":"subtitle","codec_name":"dvb_subtitle"},
				{"index":4,"codec_type":"subtitle","codec_name":"microdvd"}]}"#,
			),
			&rules(Speed::Moderate, false),
		)
		.unwrap();

		assert_eq!(
			plan.subtitles.iter().map(|track| track.codec).collect::<Vec<_>>(),
			[None, Some("srt"), None, Some("srt")]
		);
	}

	#[test]
	fn subtitles_matroska_cannot_hold_keep_the_original() {
		let teletext = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"mpeg2video"},
			{"index":1,"codec_type":"subtitle","codec_name":"dvb_teletext"}]}"#,
		);

		assert_eq!(
			plan_streams(&teletext, &rules(Speed::Moderate, false)),
			Err(Refusal::Keep(KeepReason::Unsupported(
				"subtitle codec dvb_teletext can't be stored in MKV".into()
			)))
		);
	}

	#[test]
	fn a_forced_job_drops_subtitles_matroska_cannot_hold() {
		let teletext = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"mpeg2video"},
			{"index":1,"codec_type":"subtitle","codec_name":"dvb_teletext"},
			{"index":2,"codec_type":"subtitle","codec_name":"subrip"}]}"#,
		);
		let forced = StreamRules {
			forced: true,
			..rules(Speed::Moderate, false)
		};

		let plan = plan_streams(&teletext, &forced).unwrap();

		assert_eq!(
			plan.subtitles
				.iter()
				.map(|track| track.stream_index)
				.collect::<Vec<_>>(),
			[2]
		);
		assert_eq!(
			plan.warnings,
			["subtitle stream 1 (dvb_teletext) dropped: MKV can't hold it"]
		);
	}

	#[test]
	fn webm_and_mp4_convert_text_subtitles_and_drop_what_they_can_not_hold() {
		let extras = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"},
			{"index":1,"codec_type":"subtitle","codec_name":"subrip"},
			{"index":2,"codec_type":"subtitle","codec_name":"webvtt"},
			{"index":3,"codec_type":"subtitle","codec_name":"hdmv_pgs_subtitle"},
			{"index":4,"codec_type":"attachment","codec_name":"ttf","tags":{"filename":"a.ttf"}},
			{"index":5,"codec_type":"video","codec_name":"mjpeg","disposition":{"attached_pic":1}},
			{"index":6,"codec_type":"data","codec_tag_string":"mebx"}]}"#,
		);
		let codecs = |plan: &StreamPlan| {
			plan.subtitles
				.iter()
				.map(|track| (track.stream_index, track.codec))
				.collect::<Vec<_>>()
		};

		let webm = plan_streams(&extras, &into(WEBM_VIDEO)).unwrap();
		let mp4 = plan_streams(&extras, &into(MP4_VIDEO)).unwrap();
		let mkv = plan_streams(&extras, &into(MATROSKA_VIDEO)).unwrap();

		assert_eq!(codecs(&webm), [(1, Some("webvtt")), (2, None)]);
		assert_eq!(codecs(&mp4), [(1, Some("mov_text")), (2, Some("mov_text"))]);
		assert!(webm.attachments.is_empty() && webm.extracted.is_empty());
		assert_eq!(webm.warnings.len(), 3, "{:?}", webm.warnings);
		assert!(webm.warnings.iter().any(|warning| warning.contains("--burn-subs")));
		assert_eq!(mkv.attachments, [4]);
		assert_eq!(mkv.extracted.len(), 2);
	}

	#[test]
	fn strip_keeps_only_audio_and_video() {
		let extras = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"},
			{"index":1,"codec_type":"audio","codec_name":"aac","sample_rate":"48000","channels":2,"channel_layout":"stereo"},
			{"index":2,"codec_type":"subtitle","codec_name":"subrip"},
			{"index":3,"codec_type":"attachment","codec_name":"ttf"},
			{"index":4,"codec_type":"video","codec_name":"mjpeg","disposition":{"attached_pic":1}},
			{"index":5,"codec_type":"data","codec_tag_string":"mebx"}]}"#,
		);
		let stripped = StreamRules {
			strip: true,
			..StreamRules::new(Speed::Moderate, true)
		};

		let plan = plan_streams(&extras, &stripped).unwrap();

		assert_eq!(plan.audio.len(), 1);
		assert!(plan.subtitles.is_empty() && plan.attachments.is_empty() && plan.extracted.is_empty());
		assert_eq!(
			plan.warnings,
			["--strip left out 4 subtitle, attachment, cover and data streams"]
		);
	}

	#[test]
	fn a_trim_encodes_every_audio_track_and_drops_data_streams() {
		let opus = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"},
			{"index":1,"codec_type":"audio","codec_name":"opus","sample_rate":"48000","channels":2,"channel_layout":"stereo"},
			{"index":2,"codec_type":"data","codec_tag_string":"mebx"}]}"#,
		);
		let trimmed = StreamRules {
			trim: true,
			..StreamRules::new(Speed::Moderate, true)
		};

		let plan = plan_streams(&opus, &trimmed).unwrap();

		assert!(matches!(plan.audio[0].plan, TrackPlan::Encode(_)), "{:?}", plan.audio);
		assert!(plan.extracted.is_empty());
		assert_eq!(plan.warnings, ["1 data stream dropped: data streams can't be trimmed"]);
	}

	fn trimmed(container: Container) -> StreamRules {
		StreamRules {
			trim: true,
			cut_start: true,
			cut_end: true,
			..into(container)
		}
	}

	#[test]
	fn bitmap_warnings_name_only_the_cuts_that_drop_cues() {
		let bitmaps = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"},
			{"index":1,"codec_type":"subtitle","codec_name":"hdmv_pgs_subtitle"},
			{"index":2,"codec_type":"subtitle","codec_name":"dvd_subtitle"}]}"#,
		);
		let cut = |cut_start, cut_end| {
			plan_streams(
				&bitmaps,
				&StreamRules {
					cut_start,
					cut_end,
					..trimmed(MATROSKA_VIDEO)
				},
			)
			.unwrap()
			.warnings
		};

		assert_eq!(
			cut(false, true),
			["bitmap subtitles (dvd_subtitle): a cue still on screen at the end of the trim isn't kept"]
		);
		assert!(cut(false, false).is_empty());
		assert_eq!(
			cut(true, true),
			[
				"bitmap subtitles (hdmv_pgs_subtitle): a cue already on screen at the start of the trim isn't kept",
				"bitmap subtitles (dvd_subtitle): a cue already on screen at the start of the trim, or still on screen \
				 at its end, isn't kept",
			]
		);
	}

	#[test]
	fn a_trim_clips_text_subtitles_and_warns_about_bitmap_ones() {
		let subtitles = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"},
			{"index":1,"codec_type":"subtitle","codec_name":"subrip"},
			{"index":2,"codec_type":"subtitle","codec_name":"ass"},
			{"index":3,"codec_type":"subtitle","codec_name":"hdmv_pgs_subtitle"},
			{"index":4,"codec_type":"subtitle","codec_name":"webvtt"}]}"#,
		);
		let tracks = |plan: &StreamPlan| {
			plan.subtitles
				.iter()
				.map(|track| (track.stream_index, track.codec, track.clipped))
				.collect::<Vec<_>>()
		};

		let mkv = plan_streams(&subtitles, &trimmed(MATROSKA_VIDEO)).unwrap();
		let webm = plan_streams(&subtitles, &trimmed(WEBM_VIDEO)).unwrap();
		let mp4 = plan_streams(&subtitles, &trimmed(MP4_VIDEO)).unwrap();

		assert_eq!(
			tracks(&mkv),
			[
				(1, None, Some(CueFormat::Srt)),
				(2, None, Some(CueFormat::Ass)),
				(3, None, None),
				(4, None, Some(CueFormat::Srt)),
			]
		);
		assert_eq!(
			mkv.warnings,
			["bitmap subtitles (hdmv_pgs_subtitle): a cue already on screen at the start of the trim isn't kept"]
		);
		assert_eq!(
			tracks(&webm),
			[
				(1, Some("webvtt"), Some(CueFormat::Srt)),
				(2, Some("webvtt"), Some(CueFormat::Ass)),
				(4, Some("webvtt"), Some(CueFormat::Srt)),
			]
		);
		assert!(
			tracks(&mp4)
				.iter()
				.all(|(_, codec, clipped)| *codec == Some("mov_text") && clipped.is_some())
		);
		assert!(
			plan_streams(&subtitles, &into(MATROSKA_VIDEO))
				.unwrap()
				.subtitles
				.iter()
				.all(|track| track.clipped.is_none())
		);
	}

	#[test]
	fn a_trimmed_audio_track_that_must_be_copied_says_where_it_is_cut() {
		let wide = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"},
			{"index":1,"codec_type":"audio","codec_name":"pcm_s16le","sample_rate":"48000","channels":10,"channel_layout":"5.1.4"}]}"#,
		);

		let plan = plan_streams(&wide, &trimmed(MATROSKA_VIDEO)).unwrap();

		assert_eq!(plan.audio[0].plan, TrackPlan::Copy);
		assert!(
			plan.warnings[0].starts_with("audio track 1 copied as-is: 10 channels")
				&& plan.warnings[0].ends_with(" and is cut at the nearest packet"),
			"{:?}",
			plan.warnings
		);
	}

	#[test]
	fn merging_leaves_one_encoded_track() {
		let two = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"},
			{"index":1,"codec_type":"audio","codec_name":"aac","sample_rate":"48000","channels":2,"channel_layout":"stereo"},
			{"index":2,"codec_type":"audio","codec_name":"aac","sample_rate":"48000","channels":1,"channel_layout":"mono"}]}"#,
		);
		let merge = StreamRules {
			merge: true,
			..StreamRules::new(Speed::Moderate, false)
		};

		let plan = plan_streams(&two, &merge).unwrap();

		assert_eq!(plan.audio.len(), 1);
		assert_eq!(plan.merge_from, [1, 2]);
		assert_eq!(plan.encoded_audio().len(), 1);
	}

	#[test]
	fn merging_skips_per_track_warnings_and_impossible_refusals() {
		let unencodable = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"},
			{"index":1,"codec_type":"audio","codec_name":"pcm_s16le","sample_rate":"48000","channels":10,"channel_layout":"5.1.4"},
			{"index":2,"codec_type":"audio","codec_name":"aac","sample_rate":"48000","channels":2,"channel_layout":"stereo"}]}"#,
		);
		let merge_webm = StreamRules {
			merge: true,
			..into(WEBM_VIDEO)
		};

		let plan = plan_streams(&unencodable, &merge_webm).unwrap();

		assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
		assert_eq!(plan.merge_from, [1, 2]);
		assert_eq!(plan.audio.len(), 1);
	}

	#[test]
	fn a_merged_track_takes_its_language_from_the_first_source() {
		let tagged = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"},
			{"index":1,"codec_type":"audio","codec_name":"aac","sample_rate":"48000","channels":2,"channel_layout":"stereo",
			 "tags":{"language":"eng","title":"Commentary"}},
			{"index":2,"codec_type":"audio","codec_name":"aac","sample_rate":"48000","channels":1,"channel_layout":"mono"}]}"#,
		);
		let merge = StreamRules {
			merge: true,
			..StreamRules::new(Speed::Moderate, false)
		};

		let plan = plan_streams(&tagged, &merge).unwrap();

		assert_eq!(plan.merge_language.as_deref(), Some("eng"));
		assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
	}

	#[test]
	fn merging_different_languages_warns_and_tags_the_mix_undetermined() {
		let tracks = |second: &str| {
			info(&format!(
				r#"{{"streams":[{{"index":0,"codec_type":"video","codec_name":"h264"}},
				{{"index":1,"codec_type":"audio","codec_name":"aac","sample_rate":"48000","channels":2,"channel_layout":"stereo",
				 "tags":{{"language":"eng"}}}},
				{{"index":2,"codec_type":"audio","codec_name":"aac","sample_rate":"48000","channels":2,"channel_layout":"stereo",
				 "tags":{{"language":"{second}"}}}}]}}"#
			))
		};
		let merge = StreamRules {
			merge: true,
			..StreamRules::new(Speed::Moderate, false)
		};

		let mixed = plan_streams(&tracks("jpn"), &merge).unwrap();
		let same = plan_streams(&tracks("eng"), &merge).unwrap();

		assert_eq!(mixed.merge_language.as_deref(), Some("und"));
		assert_eq!(mixed.warnings, ["mixing tracks in different languages: eng, jpn"]);
		assert_eq!(same.merge_language.as_deref(), Some("eng"));
		assert!(same.warnings.is_empty(), "{:?}", same.warnings);
		let equivalent = plan_streams(&tracks("en"), &merge).unwrap();
		assert_eq!(equivalent.merge_language.as_deref(), Some("eng"));
		assert!(equivalent.warnings.is_empty(), "{:?}", equivalent.warnings);
	}

	#[test]
	fn copied_audio_must_fit_the_container() {
		let pcm = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"},
			{"index":1,"codec_type":"audio","codec_name":"pcm_s16le","sample_rate":"48000","channels":10,"channel_layout":"5.1.4"}]}"#,
		);
		let opus = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"},
			{"index":1,"codec_type":"audio","codec_name":"opus","sample_rate":"48000","channels":2,"channel_layout":"stereo"}]}"#,
		);

		assert!(matches!(
			plan_streams(&pcm, &into(WEBM_VIDEO)),
			Err(Refusal::Impossible(reason)) if reason.contains("WebM")
		));
		assert!(matches!(
			plan_streams(&pcm, &into(MP4_VIDEO)),
			Err(Refusal::Impossible(reason)) if reason.contains("MP4")
		));
		assert!(plan_streams(&pcm, &into(MATROSKA_VIDEO)).is_ok());
		assert!(plan_streams(&opus, &into(WEBM_VIDEO)).is_ok());
		assert!(plan_streams(&opus, &into(MP4_VIDEO)).is_ok());
	}

	#[test]
	fn the_burned_track_is_left_out_without_a_warning() {
		let subtitles = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"},
			{"index":1,"codec_type":"subtitle","codec_name":"subrip"},
			{"index":2,"codec_type":"subtitle","codec_name":"hdmv_pgs_subtitle"}]}"#,
		);
		let burned = StreamRules {
			burn: Some(Burn {
				stream: 2,
				kind: BurnKind::Bitmap,
			}),
			..into(WEBM_VIDEO)
		};

		let plan = plan_streams(&subtitles, &burned).unwrap();

		assert_eq!(
			plan.subtitles
				.iter()
				.map(|track| track.stream_index)
				.collect::<Vec<_>>(),
			[1]
		);
		assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
	}
}
