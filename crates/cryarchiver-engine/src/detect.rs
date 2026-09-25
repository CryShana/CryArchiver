use std::fs::{File, Metadata};
use std::io::{self, Read};
use std::path::Path;

use infer::MatcherType;
use tokio_util::sync::CancellationToken;

use crate::outcome::{ErrorKind, IoSide, JobError};
use crate::probe::{self, MediaInfo, ProbeError};
use crate::toolchain::Toolchain;

const HEADER_BYTES: u64 = 8192;
const TEXT_LIKE_FORMATS: &[&str] = &[
	"lrc",
	"srt",
	"ass",
	"webvtt",
	"tty",
	"jacosub",
	"microdvd",
	"mpl2",
	"pjs",
	"realtext",
	"sami",
	"stl",
	"subviewer",
	"subviewer1",
	"vplayer",
	"aqtitle",
];

#[derive(Debug)]
pub enum MediaClass {
	Video(MediaInfo),
	Audio(MediaInfo),
	StillImage(MediaInfo),
	AnimatedImage(MediaInfo),
	Damaged(String),
	Other(String),
}

#[derive(Debug)]
pub struct Detection {
	pub meta: Metadata,
	pub class: MediaClass,
}

pub async fn detect(tools: &Toolchain, path: &Path, cancel: &CancellationToken) -> Result<Detection, JobError> {
	let (meta, header) = read_header(path).map_err(|err| JobError::io(IoSide::Source(path), &err))?;
	if let Some(class) = classify_header(meta.len(), &header) {
		return Ok(Detection { meta, class });
	}

	let info = match probe::probe(tools, path, cancel).await {
		Ok(info) => Some(info),
		Err(ProbeError::Failed(_)) => None,
		Err(ProbeError::Run(err)) => return Err(err.into()),
		Err(err @ ProbeError::Json(_)) => return Err(ErrorKind::Toolchain(err.to_string()).into()),
	};
	Ok(Detection {
		class: classify(&header, info),
		meta,
	})
}

pub fn read_header(path: &Path) -> io::Result<(Metadata, Vec<u8>)> {
	let file = File::open(path)?;
	let meta = file.metadata()?;

	let mut header = Vec::new();
	file.take(HEADER_BYTES).read_to_end(&mut header)?;
	Ok((meta, header))
}

pub fn classify_header(size: u64, header: &[u8]) -> Option<MediaClass> {
	if size == 0 {
		return Some(MediaClass::Other("empty file".into()));
	}

	let kind = infer::get(header)?;
	let media = matches!(
		kind.matcher_type(),
		MatcherType::Audio | MatcherType::Video | MatcherType::Image
	);
	(!media).then(|| MediaClass::Other(format!("not media: {}", kind.mime_type())))
}

pub fn classify(header: &[u8], info: Option<MediaInfo>) -> MediaClass {
	let magic = infer::get(header);
	let Some(info) = info else {
		return match magic {
			Some(kind) => MediaClass::Damaged(format!("{} signature but the file cannot be read", kind.mime_type())),
			None => MediaClass::Other("not a readable media file".into()),
		};
	};

	let format = info.format.format_name.as_str();
	let has_av = info.streams.iter().any(|stream| stream.is_audio() || stream.is_video());
	if magic.is_none() && (TEXT_LIKE_FORMATS.contains(&format) || !has_av) {
		return MediaClass::Other(format!("not media: detected only as {format}"));
	}

	let image_magic = magic.is_some_and(|kind| kind.matcher_type() == MatcherType::Image);
	let image_demuxer = format == "image2" || format.ends_with("_pipe");
	if image_demuxer && !image_magic {
		return MediaClass::Other("the extension says image, but the content does not match".into());
	}

	if image_magic {
		let animated = info.format.duration.is_some_and(|duration| duration > 0.05);
		return if animated {
			MediaClass::AnimatedImage(info)
		} else {
			MediaClass::StillImage(info)
		};
	}

	if info
		.streams
		.iter()
		.any(|stream| stream.is_video() && !stream.is_attached_pic())
	{
		return MediaClass::Video(info);
	}

	if info.audio_streams().next().is_some() {
		return MediaClass::Audio(info);
	}
	MediaClass::Other("no audio or video streams".into())
}

#[cfg(test)]
mod tests {
	use super::*;

	const FLAC: &[u8] = b"fLaC\0\0\0\x22\x10\x00\x10\x00";
	const ID3: &[u8] = b"ID3\x03\0\0\0\0\0\x21";
	const MP4: &[u8] = b"\0\0\0\x20ftypisom\0\0\x02\0isomiso2avc1mp41";
	const HEIC: &[u8] = b"\0\0\0\x18ftypheic\0\0\0\0mif1heic";
	const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
	const HTML: &[u8] = b"<html>\r\n<head><title>503 Service Temporarily Unavailable</title></head>";

	#[allow(clippy::unnecessary_wraps)]
	fn info(json: &str) -> Option<MediaInfo> {
		Some(MediaInfo::parse(json.as_bytes()).unwrap())
	}

	#[test]
	fn empty_files_and_documents_need_no_probe() {
		assert!(matches!(classify_header(0, b""), Some(MediaClass::Other(reason)) if reason == "empty file"));
		assert!(matches!(classify_header(190, HTML), Some(MediaClass::Other(reason)) if reason.contains("text/html")));
		assert!(classify_header(654_201, MP4).is_none());
		assert!(classify_header(73, b"This is plain text").is_none());
	}

	#[test]
	fn media_signature_without_probe_is_damaged() {
		assert!(matches!(classify(MP4, None), MediaClass::Damaged(_)));
		assert!(matches!(classify(b"This is plain text", None), MediaClass::Other(_)));
	}

	#[test]
	fn cover_art_does_not_make_audio_a_video() {
		let mp3 = info(
			r#"{"streams":[{"index":0,"codec_type":"audio","codec_name":"mp3"},
			{"index":1,"codec_type":"video","codec_name":"mjpeg","disposition":{"attached_pic":1}}],
			"format":{"format_name":"mp3"}}"#,
		);

		assert!(matches!(classify(ID3, mp3), MediaClass::Audio(_)));
	}

	#[test]
	fn content_beats_extension_for_video_named_mp3() {
		let mp4 = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"},
			{"index":1,"codec_type":"audio","codec_name":"aac"}],"format":{"format_name":"mov,mp4,m4a,3gp,3g2,mj2"}}"#,
		);

		assert!(matches!(classify(MP4, mp4), MediaClass::Video(_)));
	}

	#[test]
	fn weak_detections_are_not_trusted() {
		let lrc = info(
			r#"{"streams":[{"index":0,"codec_type":"subtitle","codec_name":"text"}],"format":{"format_name":"lrc"}}"#,
		);
		let fake_jpeg = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"mjpeg"}],"format":{"format_name":"image2"}}"#,
		);

		assert!(matches!(classify(&[0x13, 0x37, 0x42], lrc), MediaClass::Other(_)));
		assert!(matches!(classify(&[0xE0, 0x00, 0x10], fake_jpeg), MediaClass::Other(_)));
	}

	#[test]
	fn images_are_routed_by_content() {
		let png = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"png"}],"format":{"format_name":"png_pipe"}}"#,
		);
		let gif = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"gif"}],"format":{"format_name":"gif","duration":"3.0"}}"#,
		);
		let heic = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"hevc"}],"format":{"format_name":"mov,mp4,m4a,3gp,3g2,mj2"}}"#,
		);

		assert!(matches!(classify(PNG, png), MediaClass::StillImage(_)));
		assert!(matches!(classify(b"GIF89a\x40\x01", gif), MediaClass::AnimatedImage(_)));
		assert!(matches!(classify(HEIC, heic), MediaClass::StillImage(_)));
	}

	#[test]
	fn flac_is_audio() {
		let flac = info(
			r#"{"streams":[{"index":0,"codec_type":"audio","codec_name":"flac"}],"format":{"format_name":"flac"}}"#,
		);

		assert!(matches!(classify(FLAC, flac), MediaClass::Audio(_)));
	}
}
