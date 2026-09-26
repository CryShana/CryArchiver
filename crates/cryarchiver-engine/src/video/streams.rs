use crate::audio::plan::{TrackJob, TrackPlan, plan_track};
use crate::outcome::KeepReason;
use crate::preset::Speed;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleTrack {
	pub stream_index: u32,
	pub to_srt: bool,
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

pub fn plan_streams(info: &MediaInfo, speed: Speed, keep_metadata: bool) -> Result<StreamPlan, KeepReason> {
	let mut plan = StreamPlan::default();
	let mut audio_tracks = 0;
	let mut dropped_data = 0;

	for stream in &info.streams {
		match stream.codec_type.as_str() {
			"audio" => {
				audio_tracks += 1;
				if stream.channels.unwrap_or(0) == 0 {
					plan.warnings
						.push(format!("audio track {audio_tracks} has no audio data and was dropped"));
				} else {
					plan.audio
						.push(audio_track(stream, audio_tracks, speed, &mut plan.warnings));
				}
			}
			"subtitle" => plan.subtitles.push(subtitle_track(stream)?),
			"attachment" => plan.attachments.push(stream.index),
			"video" if stream.is_attached_pic() => match cover(stream) {
				Some(cover) => plan.extracted.push(cover),
				None => plan.warnings.push(format!(
					"cover art not copied: {} pictures can't be attached",
					stream.codec_name
				)),
			},
			"data" if is_chapter_track(stream, info) => {}
			"data" if keep_metadata => plan.extracted.push(Extracted {
				stream_index: stream.index,
				file_name: format!("stream-{}-{}.bin", stream.index, data_codec(stream)),
				mime_type: DATA_MIME.into(),
			}),
			"data" => dropped_data += 1,
			_ => {}
		}
	}

	if dropped_data > 0 {
		plan.warnings.push(format!(
			"{dropped_data} data stream{} dropped (use --keep-metadata to keep them)",
			if dropped_data == 1 { "" } else { "s" }
		));
	}
	Ok(plan)
}

fn subtitle_track(stream: &Stream) -> Result<SubtitleTrack, KeepReason> {
	let codec = stream.codec_name.as_str();
	let to_srt = if MKV_SUBTITLES.contains(&codec) {
		false
	} else if TEXT_SUBTITLES.contains(&codec) {
		true
	} else {
		return Err(KeepReason::Unsupported(format!(
			"subtitle codec {codec} can't be stored in MKV"
		)));
	};

	Ok(SubtitleTrack {
		stream_index: stream.index,
		to_srt,
	})
}

fn audio_track(stream: &Stream, track: usize, speed: Speed, warnings: &mut Vec<String>) -> TrackJob {
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
	match plan_track(stream, None, speed) {
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

	fn info(json: &str) -> MediaInfo {
		MediaInfo::parse(json.as_bytes()).unwrap()
	}

	const IPHONE: &str = r#"{"streams":[
		{"index":0,"codec_type":"video","codec_name":"hevc"},
		{"index":1,"codec_type":"audio","codec_name":"aac","sample_rate":"44100","channels":2,"channel_layout":"stereo","bit_rate":"160000"},
		{"index":2,"codec_type":"data","codec_tag_string":"mebx"},
		{"index":3,"codec_type":"data","codec_tag_string":"mebx"}]}"#;

	#[test]
	fn audio_is_planned_per_track() {
		let plan = plan_streams(&info(IPHONE), Speed::Moderate, false).unwrap();

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
		let kept = plan_streams(&info(IPHONE), Speed::Moderate, true).unwrap();
		let clean = plan_streams(&info(IPHONE), Speed::Moderate, false).unwrap();

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
			Speed::Moderate,
			true,
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
			plan_streams(&mp4, Speed::Moderate, false).unwrap().extracted,
			vec![Extracted {
				stream_index: 2,
				file_name: "cover.jpg".into(),
				mime_type: "image/jpeg".into(),
			}]
		);
		let kept = plan_streams(&mkv, Speed::Moderate, true).unwrap();
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
			Speed::Moderate,
			false,
		)
		.unwrap();

		assert_eq!(
			plan.subtitles,
			vec![
				SubtitleTrack {
					stream_index: 1,
					to_srt: false
				},
				SubtitleTrack {
					stream_index: 2,
					to_srt: true
				},
				SubtitleTrack {
					stream_index: 3,
					to_srt: false
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
			Speed::Moderate,
			true,
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
			Speed::Moderate,
			false,
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
			Speed::Moderate,
			false,
		)
		.unwrap();

		assert_eq!(
			plan.subtitles.iter().map(|track| track.to_srt).collect::<Vec<_>>(),
			[false, true, false, true]
		);
	}

	#[test]
	fn subtitles_matroska_cannot_hold_keep_the_original() {
		let teletext = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"mpeg2video"},
			{"index":1,"codec_type":"subtitle","codec_name":"dvb_teletext"}]}"#,
		);

		assert_eq!(
			plan_streams(&teletext, Speed::Moderate, false),
			Err(KeepReason::Unsupported(
				"subtitle codec dvb_teletext can't be stored in MKV".into()
			))
		);
	}
}
