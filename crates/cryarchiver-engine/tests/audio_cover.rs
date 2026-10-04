mod support;

use std::fs::File;

use cryarchiver_engine::audio::cover::{add_pictures, source_pictures};
use lofty::config::ParseOptions;
use lofty::file::AudioFile;
use lofty::ogg::{OggPictureStorage, OpusFile, VorbisFile};

#[test]
fn cover_and_title_survive_mp3_to_opus() {
	let dir = tempfile::tempdir().unwrap();
	let mp3 = support::mp3_with_cover(dir.path(), "a.mp3", "曲名テスト");
	let opus = dir.path().join("a.opus");
	support::ffmpeg(&[
		"-i",
		mp3.to_str().unwrap(),
		"-map",
		"0:a",
		"-map_metadata",
		"0",
		"-c:a",
		"libopus",
		opus.to_str().unwrap(),
	]);

	let pictures = source_pictures(&mp3);
	assert_eq!(pictures.len(), 1);
	add_pictures(&opus, pictures).unwrap();

	let file = OpusFile::read_from(&mut File::open(&opus).unwrap(), ParseOptions::new()).unwrap();
	assert_eq!(file.vorbis_comments().pictures().len(), 1);
	assert_eq!(file.vorbis_comments().get("TITLE"), Some("曲名テスト"));
}

#[test]
fn a_cover_reaches_a_vorbis_ogg_too() {
	let dir = tempfile::tempdir().unwrap();
	let mp3 = support::mp3_with_cover(dir.path(), "a.mp3", "Song");
	let ogg = dir.path().join("a.ogg");
	support::ffmpeg(&[
		"-i",
		mp3.to_str().unwrap(),
		"-map",
		"0:a",
		"-map_metadata",
		"0",
		"-c:a",
		"libvorbis",
		ogg.to_str().unwrap(),
	]);

	let cover = source_pictures(&mp3);

	add_pictures(&ogg, cover.clone()).unwrap();

	let file = VorbisFile::read_from(&mut File::open(&ogg).unwrap(), ParseOptions::new()).unwrap();
	let pictures = file.vorbis_comments().pictures();
	assert_eq!(pictures.len(), 1);
	let (written, _) = &pictures[0];
	assert_eq!(written.pic_type(), cover[0].pic_type());
	assert_eq!(written.mime_type(), cover[0].mime_type());
	assert_eq!(written.data(), cover[0].data());
	assert_eq!(file.vorbis_comments().get("TITLE"), Some("Song"));
}

#[test]
fn files_without_covers_are_left_alone() {
	let dir = tempfile::tempdir().unwrap();
	let flac = support::tone(dir.path(), "a.flac", "stereo", 2, 1, &["-c:a", "flac"]);

	assert!(source_pictures(&flac).is_empty());
	assert!(add_pictures(&dir.path().join("missing.opus"), Vec::new()).is_ok());
}
