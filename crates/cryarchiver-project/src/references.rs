//! Reference files (project layer spec §8.1): cue sheets and playlists name other files, and sidecar subtitles belong
//! to the video that shares their stem. They wait for what they name, then are copied with the names rewritten where
//! the archive changed them.

use std::ffi::{OsStr, OsString};
use std::path::Path;

use cryarchiver_engine::naming::{destination_name, is_media_name};

use crate::keys::RelPath;

pub const MAX_REFERENCE_BYTES: u64 = 1 << 20;

const CUE_EXTENSIONS: &[&str] = &["cue"];
const PLAYLIST_EXTENSIONS: &[&str] = &["m3u", "m3u8"];
const SIDECAR_EXTENSIONS: &[&str] = &["srt", "ass", "ssa", "vtt", "sub", "idx", "sup", "lrc"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
	CueSheet,
	Playlist,
	Sidecar,
}

pub fn kind_of(name: &OsStr) -> Option<RefKind> {
	let extension = Path::new(name).extension()?.to_str()?.to_ascii_lowercase();
	let extension = extension.as_str();

	if CUE_EXTENSIONS.contains(&extension) {
		Some(RefKind::CueSheet)
	} else if PLAYLIST_EXTENSIONS.contains(&extension) {
		Some(RefKind::Playlist)
	} else if SIDECAR_EXTENSIONS.contains(&extension) {
		Some(RefKind::Sidecar)
	} else {
		None
	}
}

/// A name as the reference file writes it, and the source file it resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
	pub text: Vec<u8>,
	pub target: RelPath,
}

/// The files a cue sheet or playlist names, resolved against its own folder. Absolute paths, URLs and paths that
/// climb out of the source are left alone.
pub fn references(kind: RefKind, folder: &RelPath, content: &[u8]) -> Vec<Reference> {
	let texts: Vec<&[u8]> = match kind {
		RefKind::CueSheet => content.split(|&byte| byte == b'\n').filter_map(cue_file_name).collect(),
		RefKind::Playlist => content
			.split(|&byte| byte == b'\n')
			.filter_map(playlist_entry)
			.collect(),
		RefKind::Sidecar => Vec::new(),
	};

	texts
		.into_iter()
		.filter_map(|text| {
			Some(Reference {
				target: resolve(folder, text)?,
				text: text.to_vec(),
			})
		})
		.collect()
}

/// Replaces each named file's text with its new text, only inside the lines that name files.
pub fn rewrite(kind: RefKind, content: &[u8], renames: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
	let mut out = Vec::with_capacity(content.len());
	for (index, line) in content.split(|&byte| byte == b'\n').enumerate() {
		if index > 0 {
			out.push(b'\n');
		}

		let named = match kind {
			RefKind::CueSheet => cue_file_name(line),
			RefKind::Playlist => playlist_entry(line),
			RefKind::Sidecar => None,
		};
		let replaced = named.and_then(|text| {
			let (_, new) = renames.iter().find(|(old, _)| old.as_slice() == text)?;
			let start = subslice_offset(line, text);
			Some([&line[..start], new.as_slice(), &line[start + text.len()..]].concat())
		});
		out.extend_from_slice(replaced.as_deref().unwrap_or(line));
	}
	out
}

/// The new text for a reference, written with the separator style the original used.
pub fn reference_text(original: &[u8], new_relative: &str) -> Vec<u8> {
	if original.contains(&b'\\') && !original.contains(&b'/') {
		new_relative.replace('/', "\\").into_bytes()
	} else {
		new_relative.as_bytes().to_vec()
	}
}

/// The media file a sidecar belongs to: the sibling whose stem the sidecar's name starts with (`movie.en.srt` belongs
/// to `movie.mp4`). The longest matching stem wins.
pub fn sidecar_target<'a>(sidecar: &OsStr, siblings: impl IntoIterator<Item = &'a OsStr>) -> Option<OsString> {
	let sidecar_stem = Path::new(sidecar).file_stem()?.to_str()?;

	siblings
		.into_iter()
		.filter(|sibling| *sibling != sidecar && is_media_name(sibling))
		.filter_map(|sibling| {
			let stem = Path::new(sibling).file_stem()?.to_str()?;
			let belongs = sidecar_stem == stem || sidecar_stem.starts_with(&format!("{stem}."));
			belongs.then(|| (stem.len(), sibling.to_os_string()))
		})
		.max_by_key(|(length, _)| *length)
		.map(|(_, sibling)| sibling)
}

/// The sidecar's destination name once its media file is archived as `output_name`: the media file's new stem
/// replaces the old one.
pub fn sidecar_name(sidecar: &OsStr, media_source: &OsStr, media_output: &OsStr) -> OsString {
	let (Some(sidecar), Some(old_stem), Some(new_stem)) = (
		sidecar.to_str(),
		Path::new(media_source).file_stem().and_then(OsStr::to_str),
		Path::new(media_output).file_stem().and_then(OsStr::to_str),
	) else {
		return destination_name(sidecar);
	};

	match sidecar.strip_prefix(old_stem) {
		Some(rest) => destination_name(OsStr::new(&format!("{new_stem}{rest}"))),
		None => destination_name(OsStr::new(sidecar)),
	}
}

fn cue_file_name(line: &[u8]) -> Option<&[u8]> {
	let line = trim(line);
	if line.len() < 5 || !line[..5].eq_ignore_ascii_case(b"FILE ") {
		return None;
	}

	let rest = trim(&line[5..]);
	if let Some(quoted) = rest.strip_prefix(b"\"") {
		let end = quoted.iter().position(|&byte| byte == b'"')?;
		return Some(&quoted[..end]);
	}

	let end = rest.iter().rposition(|&byte| byte == b' ')?;
	Some(trim(&rest[..end]))
}

fn playlist_entry(line: &[u8]) -> Option<&[u8]> {
	let line = trim(line);
	let absolute = line.starts_with(b"/") || line.starts_with(b"\\") || line.get(1) == Some(&b':');
	let url = line.windows(3).any(|window| window == b"://");

	(!line.is_empty() && !line.starts_with(b"#") && !absolute && !url).then_some(line)
}

fn resolve(folder: &RelPath, text: &[u8]) -> Option<RelPath> {
	let mut components: Vec<OsString> = folder.components().collect();
	for part in text.split(|&byte| byte == b'/' || byte == b'\\') {
		match part {
			b"" | b"." => {}
			b".." => {
				components.pop()?;
			}
			name => components.push(OsString::from(std::str::from_utf8(name).ok()?)),
		}
	}

	let mut path = RelPath::root();
	for component in &components {
		path = path.join(component);
	}
	Some(path)
}

fn trim(bytes: &[u8]) -> &[u8] {
	let start = bytes
		.iter()
		.position(|byte| !byte.is_ascii_whitespace())
		.unwrap_or(bytes.len());
	let end = bytes
		.iter()
		.rposition(|byte| !byte.is_ascii_whitespace())
		.map_or(start, |end| end + 1);
	&bytes[start..end]
}

fn subslice_offset(outer: &[u8], inner: &[u8]) -> usize {
	inner.as_ptr().addr() - outer.as_ptr().addr()
}

#[cfg(test)]
mod tests {
	use super::*;

	fn folder(path: &str) -> RelPath {
		RelPath::from_bytes(path.as_bytes().to_vec())
	}

	const CUE: &[u8] = b"REM GENRE Jazz\r\nFILE \"Disc 1.flac\" WAVE\r\n  TRACK 01 AUDIO\r\nFILE other.wav WAVE\r\n";

	#[test]
	fn recognises_reference_kinds_by_extension() {
		assert_eq!(kind_of(OsStr::new("Album.CUE")), Some(RefKind::CueSheet));
		assert_eq!(kind_of(OsStr::new("mix.m3u8")), Some(RefKind::Playlist));
		assert_eq!(kind_of(OsStr::new("movie.en.srt")), Some(RefKind::Sidecar));
		assert_eq!(kind_of(OsStr::new("notes.txt")), None);
	}

	#[test]
	fn cue_sheets_name_their_files() {
		let found = references(RefKind::CueSheet, &folder("Music/Album"), CUE);

		assert_eq!(
			found,
			[
				Reference {
					text: b"Disc 1.flac".to_vec(),
					target: folder("Music/Album/Disc 1.flac"),
				},
				Reference {
					text: b"other.wav".to_vec(),
					target: folder("Music/Album/other.wav"),
				},
			]
		);
	}

	#[test]
	fn playlists_skip_comments_urls_absolute_paths_and_escapes() {
		let playlist = b"#EXTM3U\n#EXTINF:1,Song\nsub\\01.flac\n../other/02.mp3\nhttp://radio/stream\nC:\\music\\x.mp3\n/abs/y.mp3\n../../../out.mp3\n";

		let found = references(RefKind::Playlist, &folder("Music/Mix"), playlist);

		let targets: Vec<String> = found.iter().map(|reference| reference.target.display()).collect();
		assert_eq!(targets, ["Music/Mix/sub/01.flac", "Music/other/02.mp3"]);
	}

	#[test]
	fn rewriting_changes_only_the_named_files() {
		let renames = vec![(b"Disc 1.flac".to_vec(), b"Disc 1.opus".to_vec())];

		let rewritten = rewrite(RefKind::CueSheet, CUE, &renames);

		assert_eq!(
			rewritten,
			b"REM GENRE Jazz\r\nFILE \"Disc 1.opus\" WAVE\r\n  TRACK 01 AUDIO\r\nFILE other.wav WAVE\r\n"
		);
	}

	#[test]
	fn rewritten_playlist_entries_keep_their_separator_style() {
		assert_eq!(reference_text(b"sub\\01.flac", "sub/01.opus"), b"sub\\01.opus");
		assert_eq!(reference_text(b"sub/01.flac", "sub/01.opus"), b"sub/01.opus");

		let rewritten = rewrite(
			RefKind::Playlist,
			b"#EXTM3U\nsub\\01.flac\n",
			&[(b"sub\\01.flac".to_vec(), b"sub\\01.opus".to_vec())],
		);
		assert_eq!(rewritten, b"#EXTM3U\nsub\\01.opus\n");
	}

	#[test]
	fn sidecars_belong_to_the_media_file_sharing_their_stem() {
		let siblings = [
			OsStr::new("movie.mp4"),
			OsStr::new("movie.en.srt"),
			OsStr::new("movie.en.mkv"),
			OsStr::new("notes.txt"),
		];

		assert_eq!(
			sidecar_target(OsStr::new("movie.en.srt"), siblings),
			Some(OsString::from("movie.en.mkv"))
		);
		assert_eq!(
			sidecar_target(OsStr::new("movie.srt"), siblings),
			Some(OsString::from("movie.mp4"))
		);
		assert_eq!(sidecar_target(OsStr::new("other.srt"), siblings), None);
	}

	#[test]
	fn sidecars_follow_a_changed_stem() {
		assert_eq!(
			sidecar_name(
				OsStr::new("movie.en.srt"),
				OsStr::new("movie.mp4"),
				OsStr::new("movie.mkv")
			),
			"movie.en.srt"
		);
		assert_eq!(
			sidecar_name(
				OsStr::new("movie.en.srt"),
				OsStr::new("movie.mp4"),
				OsStr::new("movie.mp4.mkv")
			),
			"movie.mp4.en.srt"
		);
	}
}
