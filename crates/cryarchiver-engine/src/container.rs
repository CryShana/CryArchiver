pub mod av1;
pub mod avif;
pub mod isobmff;
pub mod jxl;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cover {
	Stream,
	// ffmpeg's Ogg muxer rejects cover streams, so the picture is added after the encode.
	VorbisPicture,
	Attachment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Container {
	pub extension: &'static str,
	pub muxer: &'static str,
	pub cover: Cover,
}

pub const OGG_OPUS: Container = Container {
	extension: "opus",
	muxer: "opus",
	cover: Cover::VorbisPicture,
};
pub const MATROSKA_AUDIO: Container = Container {
	extension: "mka",
	muxer: "matroska",
	cover: Cover::Stream,
};
pub const MATROSKA_VIDEO: Container = Container {
	extension: "mkv",
	muxer: "matroska",
	cover: Cover::Attachment,
};

pub const AVIF_EXTENSION: &str = "avif";
pub const JXL_EXTENSION: &str = "jxl";

pub const OUTPUT_EXTENSIONS: [&str; 5] = [
	OGG_OPUS.extension,
	MATROSKA_AUDIO.extension,
	MATROSKA_VIDEO.extension,
	AVIF_EXTENSION,
	JXL_EXTENSION,
];
