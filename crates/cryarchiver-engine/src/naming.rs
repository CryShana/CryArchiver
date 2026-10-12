use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use unicode_normalization::UnicodeNormalization;

const CLI_MARKER: &str = "cry";
const RESERVED_DEVICE_NAMES: &[&str] = &[
	"con", "prn", "aux", "nul", "conin$", "conout$", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
	"com9", "com¹", "com²", "com³", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9", "lpt¹",
	"lpt²", "lpt³",
];
const NAME_SUFFIX: char = '＿';

pub const MEDIA_EXTENSIONS: &[&str] = &[
	"flac", "wav", "wave", "alac", "m4a", "mp3", "aac", "ac3", "eac3", "dts", "ogg", "oga", "opus", "wma", "aif",
	"aiff", "ape", "wv", "tta", "mka", "mp4", "m4v", "mov", "mkv", "webm", "avi", "wmv", "flv", "mpg", "mpeg", "ts",
	"m2ts", "mts", "vob", "3gp", "ogv", "asf", "jpg", "jpeg", "jpe", "png", "apng", "gif", "webp", "tif", "tiff",
	"bmp", "heic", "heif", "avif", "jxl", "jp2", "tga", "ppm", "pgm", "pbm", "pnm", "qoi", "iso",
];

/// The archival output names of one destination folder (engine spec §17.2). Media files that share a stem keep
/// their source extension (`IMG.jpg.avif`, `IMG.bmp.avif`), so the names depend only on the folder's contents.
#[derive(Debug, Clone, Default)]
pub struct ArchivalFolder {
	media_stems: HashMap<String, u32>,
}

impl ArchivalFolder {
	pub fn add(&mut self, source_name: &OsStr) {
		if let Some(key) = media_stem_key(source_name) {
			*self.media_stems.entry(key).or_default() += 1;
		}
	}

	pub fn encoded_name(&self, source_name: &OsStr, extension: &OsStr) -> OsString {
		let shares_stem =
			media_stem_key(source_name).is_some_and(|key| self.media_stems.get(&key).is_some_and(|count| *count > 1));

		let mut name = if shares_stem {
			source_name.to_os_string()
		} else {
			Path::new(source_name).file_stem().unwrap_or(source_name).to_os_string()
		};
		name.push(".");
		name.push(extension);
		destination_name(&name)
	}
}

pub fn copied_name(source_name: &OsStr) -> OsString {
	destination_name(source_name)
}

pub fn placeholder_name(source_name: &OsStr) -> OsString {
	let mut name = OsString::from("placeholder_");
	name.push(source_name);
	name.push(".txt");
	destination_name(&name)
}

pub fn is_media_name(name: &OsStr) -> bool {
	Path::new(name)
		.extension()
		.and_then(OsStr::to_str)
		.is_some_and(|extension| MEDIA_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()))
}

/// NFC plus full Unicode case folding: `Ä.jpg` and `ä.jpg`, and the NFC and NFD forms of `café`, share a key.
pub fn collision_key(name: &OsStr) -> String {
	let composed: String = name.to_string_lossy().nfc().collect();
	caseless::default_case_fold_str(&composed).nfc().collect()
}

/// Maps the characters and names Windows can't hold to full-width look-alikes (engine spec §17.1). A name that isn't
/// valid Unicode is returned unchanged.
pub fn destination_name(name: &OsStr) -> OsString {
	let Some(text) = name.to_str() else {
		return name.to_os_string();
	};

	let mut mapped: String = text
		.chars()
		.map(|c| match c {
			':' => '：',
			'?' => '？',
			'*' => '＊',
			'"' => '＂',
			'<' => '＜',
			'>' => '＞',
			'|' => '｜',
			'\\' => '＼',
			other => other,
		})
		.collect();

	if is_reserved_device_name(&mapped) {
		mapped.insert(mapped.find('.').unwrap_or(mapped.len()), NAME_SUFFIX);
	}

	if mapped.ends_with(['.', ' ']) {
		mapped.push(NAME_SUFFIX);
	}
	OsString::from(mapped)
}

/// Whether Windows reserves the name for a device, whatever its extension (`CON`, `nul.txt`, `COM1.tar.gz`).
pub fn is_reserved_device_name(name: &str) -> bool {
	let stem = &name[..name.find('.').unwrap_or(name.len())];
	RESERVED_DEVICE_NAMES.contains(&stem.to_ascii_lowercase().as_str())
}

fn media_stem_key(name: &OsStr) -> Option<String> {
	is_media_name(name).then(|| collision_key(Path::new(name).file_stem().unwrap_or(name)))
}

pub fn cli_output_dir(source: &Path, output_dir: Option<&Path>) -> PathBuf {
	match output_dir {
		Some(dir) => dir.to_path_buf(),
		None => source.parent().map(Path::to_path_buf).unwrap_or_default(),
	}
}

/// The CLI output for `source`, named after its stem, or after its whole name when it is a disc folder.
pub fn cli_output_path(source: &Path, output_dir: Option<&Path>, extension: impl AsRef<OsStr>) -> PathBuf {
	let stem = if source.is_dir() {
		source.file_name()
	} else {
		source.file_stem()
	};
	let mut name = OsString::from(stem.unwrap_or_default());
	name.push(format!(".{CLI_MARKER}."));
	name.push(extension);

	cli_output_dir(source, output_dir).join(name)
}

pub fn is_cli_output(path: &Path) -> bool {
	path.file_stem()
		.map(Path::new)
		.and_then(Path::extension)
		.is_some_and(|marker| marker == CLI_MARKER)
}

#[cfg(test)]
mod tests {
	use std::path::Path;

	use super::*;

	#[test]
	fn outputs_go_next_to_the_source_or_into_the_output_dir() {
		assert_eq!(
			cli_output_path(Path::new("music/01.さくら.mp3"), None, "opus"),
			Path::new("music/01.さくら.cry.opus")
		);
		assert_eq!(
			cli_output_path(Path::new("music/a.flac"), Some(Path::new("out")), "opus"),
			Path::new("out/a.cry.opus")
		);
		assert_eq!(cli_output_dir(Path::new("a.flac"), None), Path::new(""));
	}

	#[test]
	fn a_disc_folder_with_dots_keeps_its_whole_name() {
		let dir = tempfile::tempdir().unwrap();
		let first = dir.path().join("Show.Disc1");
		let second = dir.path().join("Show.Disc2");
		std::fs::create_dir(&first).unwrap();
		std::fs::create_dir(&second).unwrap();

		assert_eq!(
			cli_output_path(&first, None, "mkv"),
			dir.path().join("Show.Disc1.cry.mkv")
		);
		assert_eq!(
			cli_output_path(&second, None, "mkv"),
			dir.path().join("Show.Disc2.cry.mkv")
		);
	}

	#[test]
	fn a_media_file_swaps_its_extension() {
		let mut folder = ArchivalFolder::default();
		folder.add(OsStr::new("IMG.jpg"));
		folder.add(OsStr::new("notes.txt"));

		assert_eq!(
			folder.encoded_name(OsStr::new("IMG.jpg"), OsStr::new("avif")),
			"IMG.avif"
		);
		assert_eq!(copied_name(OsStr::new("notes.txt")), "notes.txt");
	}

	#[test]
	fn media_files_sharing_a_stem_keep_their_source_extension() {
		let mut folder = ArchivalFolder::default();
		for name in ["IMG.jpg", "IMG.bmp", "IMG.avif"] {
			folder.add(OsStr::new(name));
		}

		assert_eq!(
			folder.encoded_name(OsStr::new("IMG.jpg"), OsStr::new("avif")),
			"IMG.jpg.avif"
		);
		assert_eq!(
			folder.encoded_name(OsStr::new("IMG.bmp"), OsStr::new("avif")),
			"IMG.bmp.avif"
		);
		assert_eq!(copied_name(OsStr::new("IMG.avif")), "IMG.avif");
	}

	#[test]
	fn stems_collide_across_case_and_normalisation() {
		let mut folder = ArchivalFolder::default();
		folder.add(OsStr::new("Caf\u{e9}.JPG"));
		folder.add(OsStr::new("cafe\u{301}.png"));

		assert_eq!(
			folder.encoded_name(OsStr::new("Caf\u{e9}.JPG"), OsStr::new("avif")),
			"Caf\u{e9}.JPG.avif"
		);
		assert_eq!(
			collision_key(OsStr::new("\u{c4}.jpg")),
			collision_key(OsStr::new("\u{e4}.jpg"))
		);
	}

	#[test]
	fn a_disc_image_and_a_video_sharing_a_stem_get_distinct_names() {
		let mut folder = ArchivalFolder::default();
		for name in ["Film.iso", "Film.mkv"] {
			folder.add(OsStr::new(name));
		}

		assert_eq!(
			folder.encoded_name(OsStr::new("Film.iso"), OsStr::new("mkv")),
			"Film.iso.mkv"
		);
		assert_eq!(
			folder.encoded_name(OsStr::new("Film.mkv"), OsStr::new("mkv")),
			"Film.mkv.mkv"
		);
	}

	#[test]
	fn sidecars_do_not_change_the_media_name() {
		let mut folder = ArchivalFolder::default();
		for name in ["movie.mp4", "movie.srt", "movie.cue"] {
			folder.add(OsStr::new(name));
		}

		assert_eq!(
			folder.encoded_name(OsStr::new("movie.mp4"), OsStr::new("mkv")),
			"movie.mkv"
		);
	}

	#[test]
	fn names_windows_cannot_hold_are_mapped() {
		assert_eq!(destination_name(OsStr::new("a:b?.txt")), "a\u{ff1a}b\u{ff1f}.txt");
		assert_eq!(
			destination_name(OsStr::new("<*\"|>\\")),
			"\u{ff1c}\u{ff0a}\u{ff02}\u{ff5c}\u{ff1e}\u{ff3c}"
		);
		assert_eq!(destination_name(OsStr::new("draft. ")), "draft. \u{ff3f}");
		assert_eq!(destination_name(OsStr::new("CON.txt")), "CON\u{ff3f}.txt");
		assert_eq!(destination_name(OsStr::new("com1")), "com1\u{ff3f}");
		assert_eq!(destination_name(OsStr::new("CONSOLE.txt")), "CONSOLE.txt");
		assert_eq!(destination_name(OsStr::new("CONIN$")), "CONIN$\u{ff3f}");
		assert_eq!(destination_name(OsStr::new("conout$.log")), "conout$\u{ff3f}.log");
		assert_eq!(destination_name(OsStr::new("COM\u{b9}.txt")), "COM\u{b9}\u{ff3f}.txt");
		assert_eq!(destination_name(OsStr::new("com\u{b2}")), "com\u{b2}\u{ff3f}");
		assert_eq!(
			destination_name(OsStr::new("Lpt\u{b3}.tar.gz")),
			"Lpt\u{b3}\u{ff3f}.tar.gz"
		);
		assert_eq!(destination_name(OsStr::new("LPT\u{b9}0.txt")), "LPT\u{b9}0.txt");
		assert_eq!(placeholder_name(OsStr::new("a?.jpg")), "placeholder_a\u{ff1f}.jpg.txt");
	}

	#[test]
	fn recognizes_cli_outputs() {
		assert!(is_cli_output(Path::new("a/video.cry.mkv")));
		assert!(!is_cli_output(Path::new("a/cry.mkv")));
		assert!(!is_cli_output(Path::new("a/video.mkv")));
	}
}
