use std::fmt;
use std::path::Path;

use crate::container::{Container, MATROSKA_AUDIO, MATROSKA_VIDEO, MP4_VIDEO, OGG_AUDIO, OGG_OPUS, WEBM_VIDEO};
use crate::toolchain::MediaKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
pub enum OutputFormat {
	Mkv,
	Webm,
	Mp4,
	Opus,
	Ogg,
	Mka,
	Avif,
	Jxl,
}

impl OutputFormat {
	pub const ALL: [Self; 8] = [
		Self::Mkv,
		Self::Webm,
		Self::Mp4,
		Self::Opus,
		Self::Ogg,
		Self::Mka,
		Self::Avif,
		Self::Jxl,
	];

	pub fn extension(self) -> &'static str {
		match self {
			Self::Mkv => "mkv",
			Self::Webm => "webm",
			Self::Mp4 => "mp4",
			Self::Opus => "opus",
			Self::Ogg => "ogg",
			Self::Mka => "mka",
			Self::Avif => "avif",
			Self::Jxl => "jxl",
		}
	}

	pub fn kind(self) -> MediaKind {
		match self {
			Self::Mkv | Self::Webm | Self::Mp4 => MediaKind::Video,
			Self::Opus | Self::Ogg | Self::Mka => MediaKind::Audio,
			Self::Avif | Self::Jxl => MediaKind::Image,
		}
	}

	pub fn from_path(path: &Path) -> Option<Self> {
		let extension = path.extension()?.to_str()?.to_ascii_lowercase();
		Self::ALL.into_iter().find(|format| format.extension() == extension)
	}

	pub fn video_container(self) -> Option<Container> {
		match self {
			Self::Mkv => Some(MATROSKA_VIDEO),
			Self::Webm => Some(WEBM_VIDEO),
			Self::Mp4 => Some(MP4_VIDEO),
			Self::Opus | Self::Ogg | Self::Mka | Self::Avif | Self::Jxl => None,
		}
	}

	pub fn audio_container(self) -> Option<Container> {
		match self {
			Self::Opus => Some(OGG_OPUS),
			Self::Ogg => Some(OGG_AUDIO),
			Self::Mka => Some(MATROSKA_AUDIO),
			Self::Mkv | Self::Webm | Self::Mp4 | Self::Avif | Self::Jxl => None,
		}
	}
}

impl fmt::Display for OutputFormat {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, ".{}", self.extension())
	}
}

pub const ANIMATED_JXL: &str = "animated JPEG XL output isn't supported; use -f avif";

pub fn check_kind(format: OutputFormat, kind: MediaKind) -> Result<(), String> {
	let extracts_audio = format.kind() == MediaKind::Audio && kind == MediaKind::Video;
	let animates = format == OutputFormat::Avif && kind == MediaKind::Video;
	if format.kind() == kind || extracts_audio || animates {
		return Ok(());
	}

	if format == OutputFormat::Jxl && kind == MediaKind::Video {
		return Err(ANIMATED_JXL.to_owned());
	}

	let what = match kind {
		MediaKind::Audio => "an audio file",
		MediaKind::Video => "a video",
		MediaKind::Image => "an image",
	};
	Err(format!("{what} can't be written as {format}"))
}

pub fn check_bitrate_cap(format: OutputFormat) -> Result<(), String> {
	if format.kind() == MediaKind::Image {
		return Err(format!(
			"--max-bitrate caps a video's bitrate, so it can't be used for {format} output"
		));
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn formats_come_from_extensions_in_any_case() {
		assert_eq!(
			OutputFormat::from_path(Path::new("out/film.MKV")),
			Some(OutputFormat::Mkv)
		);
		assert_eq!(
			OutputFormat::from_path(Path::new("song.opus")),
			Some(OutputFormat::Opus)
		);
		assert_eq!(OutputFormat::from_path(Path::new("a.jxl")), Some(OutputFormat::Jxl));
		assert_eq!(OutputFormat::from_path(Path::new("notes.txt")), None);
		assert_eq!(OutputFormat::from_path(Path::new("folder")), None);
		assert_eq!(OutputFormat::Webm.to_string(), ".webm");
	}

	#[test]
	fn each_format_holds_one_kind_of_media_and_videos_can_give_their_audio() {
		assert_eq!(check_kind(OutputFormat::Mp4, MediaKind::Video), Ok(()));
		assert_eq!(check_kind(OutputFormat::Mka, MediaKind::Audio), Ok(()));
		assert_eq!(check_kind(OutputFormat::Avif, MediaKind::Image), Ok(()));
		assert_eq!(check_kind(OutputFormat::Opus, MediaKind::Video), Ok(()));
		assert_eq!(
			check_kind(OutputFormat::Mkv, MediaKind::Audio),
			Err("an audio file can't be written as .mkv".into())
		);
		assert_eq!(
			check_kind(OutputFormat::Opus, MediaKind::Image),
			Err("an image can't be written as .opus".into())
		);
		assert_eq!(OutputFormat::Ogg.audio_container().map(|c| c.muxer), Some("ogg"));
		assert_eq!(OutputFormat::Mp4.video_container().map(|c| c.muxer), Some("mp4"));
		assert_eq!(OutputFormat::Avif.video_container(), None);
	}

	#[test]
	fn a_video_can_be_an_animated_avif_but_never_an_animated_jpeg_xl_or_capped() {
		assert_eq!(check_kind(OutputFormat::Avif, MediaKind::Video), Ok(()));
		assert_eq!(
			check_kind(OutputFormat::Jxl, MediaKind::Video),
			Err(ANIMATED_JXL.into())
		);
		assert_eq!(
			check_bitrate_cap(OutputFormat::Avif),
			Err("--max-bitrate caps a video's bitrate, so it can't be used for .avif output".into())
		);
		assert!(check_bitrate_cap(OutputFormat::Jxl).is_err());
		assert_eq!(check_bitrate_cap(OutputFormat::Webm), Ok(()));
		assert_eq!(check_bitrate_cap(OutputFormat::Opus), Ok(()));
	}
}
