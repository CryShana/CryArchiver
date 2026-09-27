#[path = "../../cryarchiver-project/tests/support/mod.rs"]
mod project;
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
fn tui_without_a_terminal_explains_and_exits_with_2() {
	let dir = tempfile::tempdir().unwrap();

	for args in [
		&[][..],
		&["tui"][..],
		&["--demo"][..],
		&["tui", "--demo"][..],
		&["family.cryarchive"][..],
		&["tui", "family.cryarchive"][..],
	] {
		let out = cryarchiver(dir.path(), args);

		assert_eq!(out.status.code(), Some(2), "{args:?}");
		assert!(
			stderr(&out).contains("needs an interactive terminal"),
			"{args:?}: {}",
			stderr(&out)
		);
		assert!(stdout(&out).is_empty(), "{args:?}");
	}
}

#[test]
fn project_create_writes_a_project_that_dump_prints() {
	let dir = tempfile::tempdir().unwrap();
	fs::create_dir_all(dir.path().join("Photos")).unwrap();

	let out = cryarchiver(
		dir.path(),
		&["project", "create", "Photos", "Archive", "--watch", "--limit", "30"],
	);

	assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
	assert!(stdout(&out).contains("Photos.cryarchive"), "{}", stdout(&out));
	let project = dir.path().join("Archive").join("Photos.cryarchive");
	assert!(project.is_file());

	let dump = cryarchiver(dir.path(), &["project", "dump", project.to_str().unwrap()]);
	assert_eq!(dump.status.code(), Some(0), "{}", stderr(&dump));
	assert!(stdout(&dump).contains("format = 1"), "{}", stdout(&dump));
	assert!(stdout(&dump).contains("watch: true"), "{}", stdout(&dump));
	assert!(
		stdout(&dump).contains("performance_limit: Some(30)"),
		"{}",
		stdout(&dump)
	);
}

#[test]
fn project_create_refuses_a_destination_inside_the_source() {
	let dir = tempfile::tempdir().unwrap();
	fs::create_dir_all(dir.path().join("Photos")).unwrap();

	let out = cryarchiver(dir.path(), &["project", "create", "Photos", "Photos/Archive"]);

	assert_eq!(out.status.code(), Some(2));
	assert!(stderr(&out).contains("inside the source"), "{}", stderr(&out));
}

fn project_with(dir: &Path, delete_originals: bool) -> PathBuf {
	fs::create_dir_all(dir.join("source")).unwrap();
	cryarchiver_project::create(
		&dir.join("source"),
		&dir.join("archive"),
		std::ffi::OsStr::new("archive"),
		|settings| settings.delete_originals = delete_originals,
	)
	.unwrap()
}

#[test]
fn run_needs_the_delete_flag_and_the_setting_to_agree() {
	let dir = tempfile::tempdir().unwrap();
	let deleting = project_with(&dir.path().join("a"), true);
	let keeping = project_with(&dir.path().join("b"), false);

	let refused = cryarchiver(dir.path(), &["run", deleting.to_str().unwrap()]);
	let unwanted = cryarchiver(dir.path(), &["run", keeping.to_str().unwrap(), "--delete-originals"]);

	assert_eq!(refused.status.code(), Some(2));
	assert!(
		stderr(&refused).contains("repeat --delete-originals"),
		"{}",
		stderr(&refused)
	);
	assert_eq!(unwanted.status.code(), Some(2));
	assert!(
		stderr(&unwanted).contains("needs the project's setting on"),
		"{}",
		stderr(&unwanted)
	);
}

#[test]
fn run_refuses_a_project_that_is_already_open() {
	let dir = tempfile::tempdir().unwrap();
	let project = project_with(dir.path(), false);
	let _held = cryarchiver_project::store::Store::open(&project).unwrap();

	let out = cryarchiver(dir.path(), &["run", project.to_str().unwrap()]);

	assert_eq!(out.status.code(), Some(2));
	assert!(stderr(&out).contains("already open"), "{}", stderr(&out));
}

#[test]
fn run_of_a_clean_project_exits_with_0() {
	let dir = tempfile::tempdir().unwrap();
	let project = project_with(dir.path(), false);

	let out = cryarchiver(dir.path(), &["run", project.to_str().unwrap()]);

	assert_eq!(out.status.code(), Some(0), "{}{}", stdout(&out), stderr(&out));
	assert!(stdout(&out).contains("0 source files → 0 archived"), "{}", stdout(&out));
}

#[test]
fn run_reports_unknown_destination_files_and_exits_with_1() {
	let dir = tempfile::tempdir().unwrap();
	let project = project_with(dir.path(), false);
	fs::write(dir.path().join("archive").join("old backup.txt"), b"old").unwrap();

	let out = cryarchiver(dir.path(), &["run", project.to_str().unwrap()]);

	assert_eq!(out.status.code(), Some(1), "{}{}", stdout(&out), stderr(&out));
	assert!(stdout(&out).contains("1 open issue:"), "{}", stdout(&out));
	assert!(stdout(&out).contains("old backup.txt"), "{}", stdout(&out));
	assert!(dir.path().join("archive").join("old backup.txt").is_file());
}

#[test]
fn run_with_the_source_drive_gone_exits_with_1() {
	let dir = tempfile::tempdir().unwrap();
	let project = project_with(dir.path(), false);
	fs::write(dir.path().join("source").join("a.txt"), b"a").unwrap();
	let first = cryarchiver(dir.path(), &["run", project.to_str().unwrap()]);
	assert_eq!(first.status.code(), Some(0), "{}{}", stdout(&first), stderr(&first));
	fs::rename(dir.path().join("source"), dir.path().join("unplugged")).unwrap();

	let out = cryarchiver(dir.path(), &["run", project.to_str().unwrap()]);

	assert_eq!(out.status.code(), Some(1), "{}{}", stdout(&out), stderr(&out));
	assert!(
		stdout(&out).contains("the source folder: the folder can't be read"),
		"{}",
		stdout(&out)
	);
	assert!(dir.path().join("archive").join("a.txt").is_file());
}

#[test]
fn run_after_an_unreadable_folder_was_acknowledged_exits_with_0() {
	let setup = project::setup(|_| {});
	project::write(&setup.source, "a.txt", b"a");
	project::write(&setup.source, "Private/b.txt", b"b");
	let Some(_denied) = project::Unlistable::deny(&setup.source.join("Private")) else {
		eprintln!("skipped: this user may list any folder");
		return;
	};
	let run = || cryarchiver(setup.dir.path(), &["run", setup.project.to_str().unwrap()]);

	let unclean = run();
	assert_eq!(
		unclean.status.code(),
		Some(1),
		"{}{}",
		stdout(&unclean),
		stderr(&unclean)
	);
	assert!(
		stdout(&unclean).contains("Private: the folder can't be read"),
		"{}",
		stdout(&unclean)
	);

	{
		let session = project::open(&setup, &project::FakeEncoder::new());
		let mut snapshot = project::Snapshot::default();
		session.send(cryarchiver_project::Command::Start {
			delete_confirmed: false,
		});
		project::wait_for(&session, &mut snapshot, "the issue", |s| {
			s.finished() && s.issues.iter().any(|issue| issue.location == "Private")
		});
		let issue = snapshot
			.issues
			.iter()
			.find(|issue| issue.location == "Private")
			.unwrap();
		session.send(cryarchiver_project::Command::Resolve(
			issue.id,
			cryarchiver_project::issues::Verb::Accept,
		));
		project::wait_for(&session, &mut snapshot, "a clean finish", |s| {
			s.finished() && s.issues.is_empty()
		});
	}

	let out = run();

	assert_eq!(out.status.code(), Some(0), "{}{}", stdout(&out), stderr(&out));
	assert!(stdout(&out).contains("1 source files → 1 archived"), "{}", stdout(&out));
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

#[test]
fn compresses_a_video_next_to_the_source() {
	let dir = tempfile::tempdir().unwrap();
	let source = support::mpeg4(dir.path(), "clip.mp4", 3);
	let before = fs::read(&source).unwrap();

	let out = cryarchiver(dir.path(), &["-s", "fast", "clip.mp4"]);

	assert_eq!(out.status.code(), Some(0), "{}{}", stdout(&out), stderr(&out));
	assert!(stdout(&out).contains("✓ clip.mp4 → clip.cry.mkv"), "{}", stdout(&out));
	assert_eq!(fs::read(&source).unwrap(), before);
	assert!(!dir.path().join(".cry-work").exists());
}

#[test]
fn rerun_skips_existing_video_outputs() {
	let dir = tempfile::tempdir().unwrap();
	support::mpeg4(dir.path(), "clip.mp4", 3);
	assert!(cryarchiver(dir.path(), &["-s", "fast", "clip.mp4"]).status.success());

	let out = cryarchiver(dir.path(), &["-s", "fast", "clip.mp4"]);

	assert!(out.status.success());
	assert!(stdout(&out).contains("already exists"), "{}", stdout(&out));
}

#[test]
fn mixed_audio_and_video_batches_run_together() {
	let dir = tempfile::tempdir().unwrap();
	support::mpeg4(dir.path(), "a.mp4", 3);
	support::mpeg4(dir.path(), "b.mp4", 3);
	flac(dir.path(), "c.flac", 5);

	let out = cryarchiver(dir.path(), &["-s", "fast", "."]);

	assert_eq!(out.status.code(), Some(0), "{}{}", stdout(&out), stderr(&out));
	assert!(dir.path().join("a.cry.mkv").is_file());
	assert!(dir.path().join("b.cry.mkv").is_file());
	assert!(dir.path().join("c.cry.opus").is_file());
}

#[test]
fn compresses_an_image_next_to_the_source() {
	let dir = tempfile::tempdir().unwrap();
	let source = support::photo_png(dir.path(), "photo.png", 320, 240);
	let before = fs::read(&source).unwrap();

	let out = cryarchiver(dir.path(), &["-s", "fast", "photo.png"]);

	assert_eq!(out.status.code(), Some(0), "{}{}", stdout(&out), stderr(&out));
	assert!(
		stdout(&out).contains("✓ photo.png → photo.cry.avif"),
		"{}",
		stdout(&out)
	);
	assert_eq!(fs::read(&source).unwrap(), before);
	assert!(!dir.path().join(".cry-work").exists());
}

#[test]
fn rerun_skips_existing_image_outputs() {
	let dir = tempfile::tempdir().unwrap();
	support::photo_png(dir.path(), "photo.png", 320, 240);
	assert!(cryarchiver(dir.path(), &["-s", "fast", "photo.png"]).status.success());

	let out = cryarchiver(dir.path(), &["-s", "fast", "photo.png"]);

	assert!(out.status.success());
	assert!(
		stdout(&out).contains("· photo.png  skipped: output already exists"),
		"{}",
		stdout(&out)
	);
}

#[test]
fn raw_camera_files_are_kept_as_they_are() {
	let dir = tempfile::tempdir().unwrap();
	fs::write(dir.path().join("DSC_0001.NEF"), b"II*\0\x08\0\0\0\0\0\0\0\0\0").unwrap();

	let out = cryarchiver(dir.path(), &["DSC_0001.NEF"]);

	assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
	assert!(
		stdout(&out).contains("= DSC_0001.NEF  kept: RAW camera files are never converted"),
		"{}",
		stdout(&out)
	);
}

#[test]
fn help_and_doctor_mention_images() {
	let dir = tempfile::tempdir().unwrap();

	let help = cryarchiver(dir.path(), &["--help"]);
	let doctor = cryarchiver(dir.path(), &["doctor"]);

	assert!(stdout(&help).contains("--drop-aux"), "{}", stdout(&help));
	assert!(stdout(&doctor).contains("image filters"), "{}", stdout(&doctor));
	assert!(stdout(&doctor).contains("libjxl"), "{}", stdout(&doctor));
}
