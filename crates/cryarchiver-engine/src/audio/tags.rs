use std::error::Error;
use std::fs::File;
use std::path::Path;

use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::{AudioFile, FileType};
use lofty::ogg::tag::VorbisComments;
use lofty::ogg::{OpusFile, VorbisFile};
use lofty::probe::Probe;

const RENAMES: &[(&str, &str)] = &[
	("musicbrainz album id", "MUSICBRAINZ_ALBUMID"),
	("musicbrainz artist id", "MUSICBRAINZ_ARTISTID"),
	("musicbrainz album artist id", "MUSICBRAINZ_ALBUMARTISTID"),
	("musicbrainz release group id", "MUSICBRAINZ_RELEASEGROUPID"),
	("musicbrainz release track id", "MUSICBRAINZ_RELEASETRACKID"),
	("musicbrainz track id", "MUSICBRAINZ_TRACKID"),
	("musicbrainz work id", "MUSICBRAINZ_WORKID"),
	("musicbrainz disc id", "MUSICBRAINZ_DISCID"),
	("musicbrainz album type", "RELEASETYPE"),
	("musicbrainz album status", "RELEASESTATUS"),
	("musicbrainz album release country", "RELEASECOUNTRY"),
	("musicbrainz original album id", "MUSICBRAINZ_ORIGINALALBUMID"),
	("musicbrainz original artist id", "MUSICBRAINZ_ORIGINALARTISTID"),
	("acoustid id", "ACOUSTID_ID"),
	("acoustid fingerprint", "ACOUSTID_FINGERPRINT"),
];

pub fn vorbis_name(key: &str) -> Option<&'static str> {
	let key = key.to_ascii_lowercase();
	if key == "lyrics" || key == "unsyncedlyrics" || key.starts_with("lyrics-") {
		return Some("LYRICS");
	}

	RENAMES.iter().find(|(from, _)| *from == key).map(|(_, to)| *to)
}

pub fn rename_keys(ogg: &Path) -> Result<usize, Box<dyn Error + Send + Sync>> {
	edit_comments(ogg, |comments| Ok(rename_in(comments)))
}

/// Applies `edit` to the comments of an Ogg Opus or Vorbis file, and saves the file when `edit` counts a change.
pub(crate) fn edit_comments(
	ogg: &Path,
	edit: impl FnOnce(&mut VorbisComments) -> Result<usize, Box<dyn Error + Send + Sync>>,
) -> Result<usize, Box<dyn Error + Send + Sync>> {
	let options = ParseOptions::new().read_properties(false);
	let file_type = Probe::open(ogg)?.options(options).guess_file_type()?.file_type();

	match file_type {
		Some(FileType::Opus) => {
			let mut file = OpusFile::read_from(&mut File::open(ogg)?, options)?;
			let changed = edit(file.vorbis_comments_mut())?;
			if changed > 0 {
				file.save_to_path(ogg, WriteOptions::default())?;
			}
			Ok(changed)
		}
		Some(FileType::Vorbis) => {
			let mut file = VorbisFile::read_from(&mut File::open(ogg)?, options)?;
			let changed = edit(file.vorbis_comments_mut())?;
			if changed > 0 {
				file.save_to_path(ogg, WriteOptions::default())?;
			}
			Ok(changed)
		}
		other => Err(format!("not an Ogg Opus or Vorbis file ({other:?})").into()),
	}
}

/// A renamed item that the comments already hold under its Vorbis name, with the same value, is kept once.
fn rename_in(comments: &mut VorbisComments) -> usize {
	let items: Vec<(String, String)> = comments.take_items().collect();

	let mut kept: Vec<(String, String, bool)> = Vec::new();
	let mut renamed = 0;
	for (key, value) in items {
		let (key, is_renamed) = match vorbis_name(&key) {
			Some(name) if name != key => {
				renamed += 1;
				(name.to_owned(), true)
			}
			_ => (key, false),
		};

		let held = kept.iter().any(|(seen, seen_value, seen_renamed)| {
			(is_renamed || *seen_renamed) && seen.eq_ignore_ascii_case(&key) && *seen_value == value
		});
		if !held {
			kept.push((key, value, is_renamed));
		}
	}

	for (key, value, _) in kept {
		comments.push(key, value);
	}
	renamed
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn id3_style_keys_get_their_vorbis_names() {
		assert_eq!(vorbis_name("MusicBrainz Album Id"), Some("MUSICBRAINZ_ALBUMID"));
		assert_eq!(
			vorbis_name("MUSICBRAINZ ALBUM ARTIST ID"),
			Some("MUSICBRAINZ_ALBUMARTISTID")
		);
		assert_eq!(vorbis_name("lyrics-eng"), Some("LYRICS"));
		assert_eq!(vorbis_name("Acoustid Id"), Some("ACOUSTID_ID"));
		assert_eq!(
			vorbis_name("MusicBrainz Original Album Id"),
			Some("MUSICBRAINZ_ORIGINALALBUMID")
		);
		assert_eq!(
			vorbis_name("MusicBrainz Original Artist Id"),
			Some("MUSICBRAINZ_ORIGINALARTISTID")
		);
		assert_eq!(vorbis_name("REPLAYGAIN_TRACK_GAIN"), None);
		assert_eq!(vorbis_name("title"), None);
	}

	#[test]
	fn a_renamed_key_whose_value_is_already_under_its_vorbis_name_is_kept_once() {
		let mut comments = VorbisComments::default();
		for (key, value) in [
			("MusicBrainz Album Id", "abc"),
			("MUSICBRAINZ_ALBUMID", "abc"),
			("MUSICBRAINZ_ARTISTID", "xyz"),
			("MusicBrainz Artist Id", "other"),
			("lyrics-eng", "la la"),
			("lyrics-fra", "lo lo"),
			("ARTIST", "Band"),
			("ARTIST", "Band"),
		] {
			comments.push(key.to_owned(), value.to_owned());
		}

		let renamed = rename_in(&mut comments);

		let items: Vec<(&str, &str)> = comments.items().collect();
		assert_eq!(renamed, 4);
		assert_eq!(
			items,
			[
				("MUSICBRAINZ_ALBUMID", "abc"),
				("MUSICBRAINZ_ARTISTID", "xyz"),
				("MUSICBRAINZ_ARTISTID", "other"),
				("LYRICS", "la la"),
				("LYRICS", "lo lo"),
				("ARTIST", "Band"),
				("ARTIST", "Band"),
			]
		);
	}
}
