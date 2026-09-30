mod support;

use std::fs::File;

use cryarchiver_engine::audio::tags::rename_keys;
use lofty::config::ParseOptions;
use lofty::file::AudioFile;
use lofty::ogg::OpusFile;

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
