mod support;

use std::fs::File;

use cryarchiver_engine::audio::cover::{add_pictures, source_pictures};
use cryarchiver_engine::audio::tags::rename_keys;
use lofty::config::ParseOptions;
use lofty::file::AudioFile;
use lofty::ogg::{OggPictureStorage, OpusFile, VorbisFile};

#[test]
fn picard_style_keys_are_renamed_in_place() {
	let dir = tempfile::tempdir().unwrap();
	let opus = dir.path().join("tagged.opus");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"sine=duration=1",
		"-metadata",
		"MusicBrainz Album Id=abc",
		"-metadata",
		"lyrics-eng=la la",
		"-metadata",
		"title=Song",
		"-c:a",
		"libopus",
		opus.to_str().unwrap(),
	]);

	let renamed = rename_keys(&opus).unwrap();

	let file = OpusFile::read_from(
		&mut File::open(&opus).unwrap(),
		ParseOptions::new().read_properties(false),
	)
	.unwrap();
	let items: Vec<(String, String)> = file
		.vorbis_comments()
		.items()
		.map(|(key, value)| (key.to_owned(), value.to_owned()))
		.collect();
	assert_eq!(renamed, 2, "{items:?}");
	assert!(
		items.contains(&("MUSICBRAINZ_ALBUMID".into(), "abc".into())),
		"{items:?}"
	);
	assert!(items.contains(&("LYRICS".into(), "la la".into())), "{items:?}");
	assert!(
		!items
			.iter()
			.any(|(key, _)| key.eq_ignore_ascii_case("MusicBrainz Album Id")),
		"{items:?}"
	);
}

#[test]
fn vorbis_ogg_keys_are_renamed_too() {
	let dir = tempfile::tempdir().unwrap();
	let ogg = dir.path().join("tagged.ogg");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"sine=duration=1",
		"-metadata",
		"MusicBrainz Artist Id=xyz",
		"-metadata",
		"MusicBrainz Original Album Id=orig",
		"-metadata",
		"title=Song",
		"-c:a",
		"libvorbis",
		ogg.to_str().unwrap(),
	]);

	let renamed = rename_keys(&ogg).unwrap();

	let file = VorbisFile::read_from(
		&mut File::open(&ogg).unwrap(),
		ParseOptions::new().read_properties(false),
	)
	.unwrap();
	let items: Vec<(String, String)> = file
		.vorbis_comments()
		.items()
		.map(|(key, value)| (key.to_owned(), value.to_owned()))
		.collect();
	assert_eq!(renamed, 2, "{items:?}");
	assert!(
		items.contains(&("MUSICBRAINZ_ARTISTID".into(), "xyz".into())),
		"{items:?}"
	);
	assert!(
		items.contains(&("MUSICBRAINZ_ORIGINALALBUMID".into(), "orig".into())),
		"{items:?}"
	);
	assert_eq!(file.vorbis_comments().get("TITLE"), Some("Song"), "{items:?}");
}

#[test]
fn a_file_without_renamable_keys_is_left_untouched() {
	let dir = tempfile::tempdir().unwrap();
	let opus = dir.path().join("plain.opus");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"sine=duration=1",
		"-metadata",
		"title=Song",
		"-c:a",
		"libopus",
		opus.to_str().unwrap(),
	]);
	let before = std::fs::read(&opus).unwrap();

	let renamed = rename_keys(&opus).unwrap();

	assert_eq!(renamed, 0);
	assert_eq!(std::fs::read(&opus).unwrap(), before);
}

#[test]
fn a_file_that_is_not_ogg_opus_or_vorbis_is_an_error() {
	let dir = tempfile::tempdir().unwrap();
	let flac = support::tone(dir.path(), "a.flac", "stereo", 2, 1, &["-c:a", "flac"]);

	assert!(rename_keys(&flac).is_err());
}

#[test]
fn covers_and_other_tags_survive_the_rename() {
	let dir = tempfile::tempdir().unwrap();
	let mp3 = support::mp3_with_cover(dir.path(), "a.mp3", "Song");
	let opus = dir.path().join("a.opus");
	support::ffmpeg(&[
		"-i",
		mp3.to_str().unwrap(),
		"-map",
		"0:a",
		"-map_metadata",
		"0",
		"-metadata",
		"MusicBrainz Album Id=abc",
		"-c:a",
		"libopus",
		opus.to_str().unwrap(),
	]);
	add_pictures(&opus, source_pictures(&mp3)).unwrap();

	let renamed = rename_keys(&opus).unwrap();

	let file = OpusFile::read_from(&mut File::open(&opus).unwrap(), ParseOptions::new()).unwrap();
	assert_eq!(renamed, 1);
	assert_eq!(file.vorbis_comments().pictures().len(), 1);
	assert_eq!(file.vorbis_comments().get("TITLE"), Some("Song"));
	assert_eq!(file.vorbis_comments().get("MUSICBRAINZ_ALBUMID"), Some("abc"));
}
