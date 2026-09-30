use std::error::Error;
use std::fs::File;
use std::path::Path;

use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::AudioFile;
use lofty::ogg::OpusFile;

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

pub fn rename_keys(opus: &Path) -> Result<usize, Box<dyn Error + Send + Sync>> {
	let mut file = OpusFile::read_from(&mut File::open(opus)?, ParseOptions::new().read_properties(false))?;
	let comments = file.vorbis_comments_mut();
	let items: Vec<(String, String)> = comments.take_items().collect();

	let mut renamed = 0;
	for (key, value) in items {
		match vorbis_name(&key) {
			Some(name) if name != key => {
				renamed += 1;
				comments.push(name.to_owned(), value);
			}
			_ => comments.push(key, value),
		}
	}

	if renamed > 0 {
		file.save_to_path(opus, WriteOptions::default())?;
	}
	Ok(renamed)
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
		assert_eq!(vorbis_name("REPLAYGAIN_TRACK_GAIN"), None);
		assert_eq!(vorbis_name("title"), None);
	}
}
