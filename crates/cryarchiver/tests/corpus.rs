use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;

use lofty::config::ParseOptions;
use lofty::file::AudioFile;
use lofty::ogg::{OggPictureStorage, OpusFile};

fn corpus() -> PathBuf {
	std::env::var_os("CRY_CORPUS").map_or_else(|| PathBuf::from(r"C:\media-corpus"), PathBuf::from)
}

fn copy_into(dir: &Path, relative: &str) {
	let source = corpus().join(relative);
	fs::copy(&source, dir.join(source.file_name().unwrap())).unwrap();
}

#[test]
#[ignore = "needs the local sample corpus (set CRY_CORPUS, default C:\\media-corpus)"]
fn corpus_audio_outcomes() {
	let dir = tempfile::tempdir().unwrap();
	for file in [
		"01.テスト.mp3",
		"02. 試験曲.mp3",
		"battle-music.mp3",
		"generated-valid/audio_opus_mono_32k.opus",
		"generated-valid/audio_10ch_reads-as-5.1.4.wav",
		"generated-valid/audio_hexagonal_flac.mka",
		"generated-valid/audio_24bit_96k.flac",
		"generated-valid/audio_alac_cover.m4a",
		"generated-corrupted/garbage_prefix_4k.mp3",
		"generated-corrupted/text_file_named.mp4",
	] {
		copy_into(dir.path(), file);
	}

	let out = Command::new(env!("CARGO_BIN_EXE_cryarchiver"))
		.current_dir(dir.path())
		.arg(".")
		.output()
		.unwrap();
	let stdout = String::from_utf8_lossy(&out.stdout);

	for encoded in [
		"01.テスト",
		"02. 試験曲",
		"battle-music",
		"audio_24bit_96k",
		"audio_hexagonal_flac",
		"audio_alac_cover",
		"garbage_prefix_4k",
	] {
		assert!(
			dir.path().join(format!("{encoded}.cry.opus")).is_file(),
			"{encoded} missing\n{stdout}"
		);
	}
	assert!(stdout.contains("= audio_opus_mono_32k.opus  kept"), "{stdout}");
	assert!(stdout.contains("= audio_10ch_reads-as-5.1.4.wav  kept"), "{stdout}");
	assert!(stdout.contains("· text_file_named.mp4  skipped"), "{stdout}");

	for with_cover in ["01.テスト.cry.opus", "audio_alac_cover.cry.opus"] {
		let file = OpusFile::read_from(
			&mut File::open(dir.path().join(with_cover)).unwrap(),
			ParseOptions::new(),
		)
		.unwrap();
		assert_eq!(
			file.vorbis_comments().pictures().len(),
			1,
			"{with_cover} lost its cover"
		);
	}
}
