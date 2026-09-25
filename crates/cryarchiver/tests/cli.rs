#[path = "../../cryarchiver-engine/tests/support/mod.rs"]
mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn cryarchiver(dir: &Path, args: &[&str]) -> Output {
	Command::new(env!("CARGO_BIN_EXE_cryarchiver"))
		.current_dir(dir)
		.args(args)
		.output()
		.unwrap()
}

fn stdout(out: &Output) -> String {
	String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
	String::from_utf8_lossy(&out.stderr).into_owned()
}

fn flac(dir: &Path, name: &str, seconds: u32) -> PathBuf {
	support::tone(dir, name, "stereo", 2, seconds, &["-c:a", "flac"])
}

#[test]
fn prints_version() {
	let dir = tempfile::tempdir().unwrap();

	let out = cryarchiver(dir.path(), &["--version"]);

	assert!(out.status.success());
	assert_eq!(stdout(&out).trim(), "cryarchiver 0.1.0");
}

#[test]
fn no_arguments_prints_the_tui_notice() {
	let dir = tempfile::tempdir().unwrap();

	let out = cryarchiver(dir.path(), &[]);

	assert!(out.status.success());
	assert!(stdout(&out).contains("TUI"));
}

#[test]
fn doctor_lists_audio_capabilities() {
	let dir = tempfile::tempdir().unwrap();

	let out = cryarchiver(dir.path(), &["doctor"]);

	assert!(out.status.success(), "{}", stdout(&out));
	assert!(stdout(&out).contains("libopus"));
}

#[test]
fn compresses_flac_next_to_source_and_leaves_the_source_untouched() {
	let dir = tempfile::tempdir().unwrap();
	let source = flac(dir.path(), "a.flac", 10);
	let before = (
		fs::read(&source).unwrap(),
		fs::metadata(&source).unwrap().modified().unwrap(),
	);

	let out = cryarchiver(dir.path(), &["a.flac"]);

	assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
	assert!(dir.path().join("a.cry.opus").is_file());
	assert!(stdout(&out).contains("✓ a.flac → a.cry.opus"));
	assert_eq!(
		(
			fs::read(&source).unwrap(),
			fs::metadata(&source).unwrap().modified().unwrap()
		),
		before
	);
	assert!(!dir.path().join(".cry-work").exists());
}

#[test]
fn rerun_skips_existing_outputs() {
	let dir = tempfile::tempdir().unwrap();
	flac(dir.path(), "a.flac", 5);
	assert!(cryarchiver(dir.path(), &["a.flac"]).status.success());
	let first = fs::metadata(dir.path().join("a.cry.opus")).unwrap().modified().unwrap();

	let out = cryarchiver(dir.path(), &["a.flac"]);

	assert!(out.status.success());
	assert!(stdout(&out).contains("already exists"));
	assert_eq!(
		fs::metadata(dir.path().join("a.cry.opus")).unwrap().modified().unwrap(),
		first
	);
}

#[test]
fn folders_wildcards_and_output_dir() {
	let dir = tempfile::tempdir().unwrap();
	let music = dir.path().join("music");
	fs::create_dir_all(music.join("sub")).unwrap();
	flac(&music, "a.flac", 5);
	flac(&music.join("sub"), "c.flac", 5);
	fs::write(music.join("notes.txt"), b"not audio").unwrap();

	let folder = cryarchiver(dir.path(), &["music"]);
	let wildcard = cryarchiver(dir.path(), &["-o", "out", "music/*.flac"]);

	assert!(folder.status.success());
	assert!(music.join("a.cry.opus").is_file());
	assert!(!music.join("sub").join("c.cry.opus").exists());
	assert!(stdout(&folder).contains("· notes.txt  skipped"));
	assert!(wildcard.status.success());
	assert!(dir.path().join("out").join("a.cry.opus").is_file());
}

#[test]
fn unicode_names_round_trip() {
	let dir = tempfile::tempdir().unwrap();
	flac(dir.path(), "02. 試験曲.flac", 5);

	let out = cryarchiver(dir.path(), &["02. 試験曲.flac"]);

	assert!(out.status.success(), "{}", stdout(&out));
	assert!(dir.path().join("02. 試験曲.cry.opus").is_file());
	assert!(stdout(&out).contains("02. 試験曲.flac"));
}

#[test]
fn damaged_sources_fail_with_exit_code_one() {
	let dir = tempfile::tempdir().unwrap();
	let source = flac(dir.path(), "cut.flac", 10);
	let bytes = fs::read(&source).unwrap();
	fs::write(&source, &bytes[..bytes.len() * 2 / 5]).unwrap();

	let out = cryarchiver(dir.path(), &["cut.flac"]);

	assert_eq!(out.status.code(), Some(1));
	assert!(stdout(&out).contains("✗ cut.flac  failed: source damaged"));
	assert!(!dir.path().join("cut.cry.opus").exists());
}

#[test]
fn missing_inputs_fail_the_run() {
	let dir = tempfile::tempdir().unwrap();
	flac(dir.path(), "a.flac", 5);

	let out = cryarchiver(dir.path(), &["a.flac", "typo.flac"]);

	assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
	assert!(dir.path().join("a.cry.opus").is_file());
	assert!(stderr(&out).contains("typo.flac: not found"), "{}", stderr(&out));
}

#[test]
fn no_inputs_is_a_usage_error() {
	let dir = tempfile::tempdir().unwrap();

	let out = cryarchiver(dir.path(), &["missing.flac"]);

	assert_eq!(out.status.code(), Some(2));
}

#[test]
fn leading_dash_names_work_through_wildcards() {
	let dir = tempfile::tempdir().unwrap();
	flac(dir.path(), "-intro.flac", 5);

	let out = cryarchiver(dir.path(), &["*.flac"]);

	assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
	assert!(dir.path().join("-intro.cry.opus").is_file());
	assert!(stdout(&out).contains("✓ -intro.flac"));
}

#[test]
fn same_stem_inputs_do_not_share_an_output() {
	let dir = tempfile::tempdir().unwrap();
	flac(dir.path(), "song.flac", 5);
	support::tone(dir.path(), "song.wav", "stereo", 2, 5, &["-c:a", "pcm_s16le"]);

	let out = cryarchiver(dir.path(), &["--overwrite", "song.flac", "song.wav"]);

	assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
	assert!(stdout(&out).contains("· song.wav  skipped: same output name as"));
	assert!(dir.path().join("song.cry.opus").is_file());
}
