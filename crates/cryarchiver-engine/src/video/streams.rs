use crate::audio::plan::{TrackJob, TrackPlan, plan_track_with};
use crate::container::{Container, MATROSKA_VIDEO};
use crate::outcome::{KeepReason, Refusal};
use crate::preset::{Overrides, Speed};
use crate::probe::{MediaInfo, Stream};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleTrack {
	pub stream_index: u32,
	pub codec: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StreamRules {
	pub speed: Speed,
	pub overrides: Overrides,
	pub keep_metadata: bool,
	pub container: Container,
	pub strip: bool,
	pub trim: bool,
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

	for stream in &info.streams {
		if rules.strip && !(stream.is_audio() || stream.is_real_video()) {
			stripped += 1;
			continue;
		}

		match stream.codec_type.as_str() {
			"audio" => {
				audio_tracks += 1;
				if stream.channels.unwrap_or(0) == 0 {
					plan.warnings
						.push(format!("audio track {audio_tracks} has no audio data and was dropped"));
					continue;
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
			}
			"subtitle" => match subtitle_track(stream, target)? {
				Some(track) => plan.subtitles.push(track),
				None => plan.warnings.push(format!(
					"subtitle stream {} ({}) dropped: {} holds only text subtitles; --burn-subs can draw it into the picture",
					stream.index,
					stream.codec_name,
					target.name()
				)),
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

fn subtitle_track(stream: &Stream, target: Target) -> Result<Option<SubtitleTrack>, Refusal> {
	let codec = stream.codec_name.as_str();
	let text = MKV_TEXT.contains(&codec) || TEXT_SUBTITLES.contains(&codec);
	let track = |converted: Option<&'static str>| {
		Ok(Some(SubtitleTrack {
			stream_index: stream.index,
			codec: converted,
		}))
	};

	match target {
		Target::Matroska if MKV_SUBTITLES.contains(&codec) => track(None),
		Target::Matroska if TEXT_SUBTITLES.contains(&codec) => track(Some("srt")),
		Target::Matroska => Err(Refusal::Keep(KeepReason::Unsupported(format!(
			"subtitle codec {codec} can't be stored in MKV"
		)))),
		Target::Webm if text => track((codec != "webvtt").then_some("webvtt")),
		Target::Mp4 if text => track((codec != "mov_text").then_some("mov_text")),
		Target::Webm | Target::Mp4 => Ok(None),
	}
}

fn audio_track(stream: &Stream, track: usize, rules: &StreamRules, warnings: &mut Vec<String>) -> TrackJob {
	let copy = |warnings: &mut Vec<String>, reason: &str| {
		warnings.push(format!("audio track {track} copied as-is: {reason}"));
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
					codec: None
				},
				SubtitleTrack {
					stream_index: 2,
					codec: Some("srt")
				},
				SubtitleTrack {
					stream_index: 3,
					codec: None
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
}
