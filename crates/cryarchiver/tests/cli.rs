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
fn project_create_stores_quality_overrides_that_a_reopen_reads_back() {
	let dir = tempfile::tempdir().unwrap();
	fs::create_dir_all(dir.path().join("Photos")).unwrap();

	let out = cryarchiver(
		dir.path(),
		&["project", "create", "Photos", "Archive", "--image-target", "80"],
	);

	assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
	let project = dir.path().join("Archive").join("Photos.cryarchive");
	let (_, meta, _) = cryarchiver_project::store::Store::open(&project).unwrap();
	assert_eq!(
		meta.settings.overrides,
		cryarchiver_engine::preset::Overrides {
			image: Some(80.0),
			..Default::default()
		}
	);

	let dump = cryarchiver(dir.path(), &["project", "dump", project.to_str().unwrap()]);
	assert_eq!(dump.status.code(), Some(0), "{}", stderr(&dump));
	assert!(
		stdout(&dump).contains("overrides: Overrides { video: None, image: Some(80.0), audio_stereo_kbps: None }"),
		"{}",
		stdout(&dump)
	);
}

#[test]
fn quality_flags_out_of_range_exit_with_2_and_the_range() {
	let dir = tempfile::tempdir().unwrap();
	fs::create_dir_all(dir.path().join("Photos")).unwrap();

	let compress = cryarchiver(dir.path(), &["a.flac", "--image-target", "40"]);
	let create = cryarchiver(
		dir.path(),
		&["project", "create", "Photos", "Archive", "--video-vmaf", "95/60"],
	);
	let fast = cryarchiver(dir.path(), &["a.mp4", "-s", "fast", "--video-vmaf", "93/89"]);

	assert_eq!(compress.status.code(), Some(2));
	assert!(stderr(&compress).contains("between 50 and 95"), "{}", stderr(&compress));
	assert_eq!(create.status.code(), Some(2));
	assert!(
		stderr(&create).contains("between 70 and the mean (95)"),
		"{}",
		stderr(&create)
	);
	assert!(!dir.path().join("Archive").exists());
	assert_eq!(fast.status.code(), Some(2));
	assert!(
		stderr(&fast).contains("FAST never measures video quality; use moderate or slow"),
		"{}",
		stderr(&fast)
	);
}

#[test]
fn run_of_a_project_with_overrides_names_the_custom_profile() {
	let dir = tempfile::tempdir().unwrap();
	fs::create_dir_all(dir.path().join("source")).unwrap();
	let project = cryarchiver_project::create(
		&dir.path().join("source"),
		&dir.path().join("archive"),
		std::ffi::OsStr::new("archive"),
		|settings| settings.overrides.image = Some(80.0),
	)
	.unwrap();

	let out = cryarchiver(dir.path(), &["run", project.to_str().unwrap()]);

	assert_eq!(out.status.code(), Some(0), "{}{}", stdout(&out), stderr(&out));
	assert!(
		stdout(&out).contains("MODERATE · custom: image 80\n"),
		"{}",
		stdout(&out)
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

/// Runs `project create src d/archive --name <name>` from `cwd` and checks it exits with 2, naming the reason, with
/// nothing created in the source or the destination.
fn assert_name_refused(dir: &Path, cwd: &Path, name: &str) {
	let (source, destination) = (dir.join("src"), dir.join("d").join("archive"));

	let out = cryarchiver(
		cwd,
		&[
			"project",
			"create",
			source.to_str().unwrap(),
			destination.to_str().unwrap(),
			"--name",
			name,
		],
	);

	assert_eq!(out.status.code(), Some(2), "{name}: {}", stdout(&out));
	assert!(stderr(&out).contains("can't be used"), "{name}: {}", stderr(&out));
	assert!(!destination.exists(), "{name}");
	let entries: Vec<_> = fs::read_dir(&source)
		.unwrap()
		.map(|entry| entry.unwrap().file_name())
		.collect();
	assert_eq!(entries, ["sub"], "{name}");
	assert_eq!(fs::read_dir(source.join("sub")).unwrap().count(), 0, "{name}");
}

fn name_setup() -> tempfile::TempDir {
	let dir = tempfile::tempdir().unwrap();
	fs::create_dir_all(dir.path().join("src").join("sub")).unwrap();
	fs::create_dir(dir.path().join("d")).unwrap();
	dir
}

#[test]
fn project_create_refuses_a_name_that_leaves_the_destination() {
	let dir = name_setup();
	let absolute = dir.path().join("src").join("abs");

	assert_name_refused(dir.path(), dir.path(), "../../src/fwd");
	assert_name_refused(dir.path(), dir.path(), absolute.to_str().unwrap());
}

#[cfg(windows)]
#[test]
fn project_create_refuses_windows_names_that_leave_the_destination() {
	let dir = name_setup();
	let _ = Command::new("cmd")
		.args(["/C", "mklink", "/J"])
		.arg(dir.path().join("d").join("j"))
		.arg(dir.path().join("src").join("sub"))
		.stdout(std::process::Stdio::null())
		.status();
	let inside_source = dir.path().join("src");

	assert_name_refused(dir.path(), dir.path(), r"..\..\src\evil");
	assert_name_refused(dir.path(), dir.path(), r"..\j\evil");
	assert_name_refused(dir.path(), &inside_source, "C:rel");
}

#[cfg(unix)]
#[test]
fn project_create_refuses_a_unix_name_through_a_link() {
	let dir = name_setup();
	std::os::unix::fs::symlink(dir.path().join("src").join("sub"), dir.path().join("d").join("lnk")).unwrap();

	assert_name_refused(dir.path(), dir.path(), "../lnk/evil");
}

#[test]
fn project_create_refuses_a_dangling_link_where_the_project_file_goes() {
	let dir = name_setup();
	let (source, destination) = (dir.path().join("src"), dir.path().join("d").join("archive"));
	fs::create_dir(&destination).unwrap();
	let target = source.join("linked.cryarchive");
	if !project::link_file(&destination.join("sevil.cryarchive"), &target) {
		eprintln!("skipped: no file link can be made here");
		return;
	}

	let out = cryarchiver(
		dir.path(),
		&[
			"project",
			"create",
			source.to_str().unwrap(),
			destination.to_str().unwrap(),
			"--name",
			"sevil",
		],
	);

	assert_eq!(out.status.code(), Some(2), "{}", stdout(&out));
	assert!(fs::symlink_metadata(&target).is_err());
	assert_eq!(project::names_in(&source), ["sub"]);
}

#[test]
fn run_refuses_a_project_whose_work_folder_is_a_link_into_the_source() {
	let dir = tempfile::tempdir().unwrap();
	let project = project_with(dir.path(), false);
	let source = dir.path().join("source");
	for (path, content) in [
		("victim/keep.txt", "keep"),
		("victim2/data.txt", "data"),
		("top.txt", "top"),
	] {
		project::write(&source, path, content.as_bytes());
	}
	let work = dir.path().join("archive").join(".cryarchiver").join("work");
	fs::create_dir_all(work.parent().unwrap()).unwrap();
	if !project::link_folder(&work, &source) {
		eprintln!("skipped: no folder link can be made here");
		return;
	}

	let out = cryarchiver(dir.path(), &["run", project.to_str().unwrap()]);

	assert_eq!(out.status.code(), Some(2), "{}", stdout(&out));
	assert!(stderr(&out).contains("link"), "{}", stderr(&out));
	assert_eq!(project::names_in(&source), ["top.txt", "victim", "victim2"]);
	assert_eq!(fs::read(source.join("victim").join("keep.txt")).unwrap(), b"keep");
	assert_eq!(fs::read(source.join("victim2").join("data.txt")).unwrap(), b"data");
	assert!(fs::symlink_metadata(&work).is_ok());
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
	flac(dir.path(), "02. 夜明け.flac", 5);

	let out = cryarchiver(dir.path(), &["02. 夜明け.flac"]);

	assert!(out.status.success(), "{}", stdout(&out));
	assert!(dir.path().join("02. 夜明け.cry.opus").is_file());
	assert!(stdout(&out).contains("02. 夜明け.flac"));
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
fn plain_runs_write_nothing_outside_the_outputs() {
	let dir = tempfile::tempdir().unwrap();
	let home = tempfile::tempdir().unwrap();
	support::mpeg4(dir.path(), "first.mp4", 3);
	support::mpeg4(dir.path(), "second.mp4", 3);

	let out = Command::new(env!("CARGO_BIN_EXE_cryarchiver"))
		.current_dir(dir.path())
		.env("HOME", home.path())
		.env("LOCALAPPDATA", home.path())
		.env("XDG_CACHE_HOME", home.path())
		.args(["first.mp4", "second.mp4", "-o", "out"])
		.output()
		.unwrap();

	assert_eq!(out.status.code(), Some(0), "{}{}", stdout(&out), stderr(&out));
	assert!(
		fs::read_dir(home.path()).unwrap().next().is_none(),
		"the cache dir gained files"
	);
	let produced: Vec<_> = fs::read_dir(dir.path().join("out"))
		.unwrap()
		.map(|entry| entry.unwrap().file_name())
		.collect();
	assert_eq!(produced.len(), 2, "{produced:?}");
	assert!(
		produced.iter().all(|name| name.to_str().unwrap().ends_with(".cry.mkv")),
		"{produced:?}"
	);
}

#[test]
fn an_audio_override_sets_the_opus_bitrate() {
	let dir = tempfile::tempdir().unwrap();
	flac(dir.path(), "a.flac", 5);

	let custom = cryarchiver(dir.path(), &["a.flac", "-o", "custom", "--audio-kbps", "64"]);
	let preset = cryarchiver(dir.path(), &["a.flac", "-o", "preset"]);

	assert_eq!(custom.status.code(), Some(0), "{}{}", stdout(&custom), stderr(&custom));
	assert!(
		stdout(&custom).contains("MODERATE · custom: audio 64 kbps\n"),
		"{}",
		stdout(&custom)
	);
	assert!(!stdout(&preset).contains("custom"), "{}", stdout(&preset));
	let size = |folder: &str| fs::metadata(dir.path().join(folder).join("a.cry.opus")).unwrap().len();
	assert!(
		size("custom") * 4 < size("preset") * 3,
		"{} vs {}",
		size("custom"),
		size("preset")
	);
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
fn an_output_file_needs_exactly_one_input() {
	let dir = tempfile::tempdir().unwrap();
	flac(dir.path(), "a.flac", 2);
	flac(dir.path(), "b.flac", 2);

	let out = cryarchiver(dir.path(), &["a.flac", "b.flac", "-o", "both.opus"]);

	assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
	assert!(stderr(&out).contains("exactly one input"), "{}", stderr(&out));
	assert!(!dir.path().join("both.opus").exists());
	assert!(!dir.path().join("a.cry.opus").exists());
}

#[test]
fn an_output_file_picks_the_format_and_must_agree_with_f() {
	let dir = tempfile::tempdir().unwrap();
	flac(dir.path(), "song.flac", 2);

	let conflicting = cryarchiver(dir.path(), &["song.flac", "-o", "song.mka", "-f", "opus"]);
	let out = cryarchiver(dir.path(), &["song.flac", "-o", "song.mka"]);

	assert_eq!(conflicting.status.code(), Some(2), "{}", stderr(&conflicting));
	assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
	let bytes = fs::read(dir.path().join("song.mka")).unwrap();
	assert_eq!(&bytes[..4], b"\x1a\x45\xdf\xa3", "a Matroska file");
}

#[test]
fn a_format_that_does_not_fit_a_file_fails_only_that_file() {
	let dir = tempfile::tempdir().unwrap();
	support::mpeg4(dir.path(), "clip.mp4", 3);
	flac(dir.path(), "a.flac", 2);
	support::photo_png(dir.path(), "photo.png", 64, 48);

	let out = cryarchiver(
		dir.path(),
		&["-s", "fast", "-f", "webm", "clip.mp4", "a.flac", "photo.png"],
	);

	assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
	assert!(dir.path().join("clip.cry.webm").is_file(), "{}", stdout(&out));
	assert!(
		stdout(&out).contains("✗ a.flac  failed: encode failed: an audio file can't be written as .webm"),
		"{}",
		stdout(&out)
	);
	assert!(
		stdout(&out).contains("✗ photo.png  failed: encode failed: an image can't be written as .webm"),
		"{}",
		stdout(&out)
	);
}

#[test]
fn a_video_named_as_an_avif_output_becomes_an_animated_avif() {
	let dir = tempfile::tempdir().unwrap();
	support::mpeg4(dir.path(), "clip.mp4", 1);

	let out = cryarchiver(dir.path(), &["clip.mp4", "-s", "fast", "-o", "clip.avif"]);

	assert_eq!(out.status.code(), Some(0), "{}{}", stdout(&out), stderr(&out));
	assert!(stdout(&out).contains("an audio track was dropped"), "{}", stdout(&out));
	let bytes = fs::read(dir.path().join("clip.avif")).unwrap();
	assert_eq!(&bytes[4..12], b"ftypavis", "an image sequence");
}

#[test]
fn a_bitrate_cap_is_refused_for_avif_output() {
	let dir = tempfile::tempdir().unwrap();
	support::mpeg4(dir.path(), "clip.mp4", 1);

	let by_format = cryarchiver(dir.path(), &["clip.mp4", "-f", "avif", "--max-bitrate", "2"]);
	let by_name = cryarchiver(dir.path(), &["clip.mp4", "-o", "clip.avif", "--max-bitrate", "2"]);

	for out in [by_format, by_name] {
		assert_eq!(out.status.code(), Some(2), "{}{}", stdout(&out), stderr(&out));
		assert!(stderr(&out).contains("--max-bitrate"), "{}", stderr(&out));
	}
	assert!(!dir.path().join("clip.cry.avif").exists());
	assert!(!dir.path().join("clip.avif").exists());
}

#[test]
fn an_image_encoder_that_disagrees_with_the_format_is_refused() {
	let dir = tempfile::tempdir().unwrap();
	support::photo_png(dir.path(), "photo.png", 64, 48);

	let out = cryarchiver(dir.path(), &["-f", "avif", "--image-encoder", "jxl", "photo.png"]);

	assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
	assert!(stderr(&out).contains("--image-encoder and -f"), "{}", stderr(&out));
	assert!(!dir.path().join("photo.cry.avif").exists());
}

#[test]
fn strip_is_a_flag() {
	let dir = tempfile::tempdir().unwrap();
	flac(dir.path(), "song.flac", 2);

	let out = cryarchiver(dir.path(), &["--strip", "song.flac"]);

	assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
	assert!(dir.path().join("song.cry.opus").is_file());
}

#[test]
fn burn_subs_takes_its_value_after_an_equals_sign() {
	let dir = tempfile::tempdir().unwrap();
	flac(dir.path(), "song.flac", 2);

	let out = cryarchiver(dir.path(), &["--burn-subs", "song.flac", "--sub-size", "8"]);

	assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
	assert!(dir.path().join("song.cry.opus").is_file(), "song.flac stays an input");
}

#[test]
fn merge_audio_is_a_flag() {
	let dir = tempfile::tempdir().unwrap();
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"sine=duration=2",
		"-f",
		"lavfi",
		"-i",
		"sine=frequency=880:duration=2",
		"-map",
		"0",
		"-map",
		"1",
		"-c:a",
		"flac",
		dir.path().join("two.mka").to_str().unwrap(),
	]);

	let out = cryarchiver(dir.path(), &["--merge-audio", "two.mka"]);

	assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
	let output = dir.path().join("two.cry.opus");
	assert!(output.is_file());
	let probed = Command::new("ffprobe")
		.args(["-v", "error", "-show_entries", "stream=codec_type", "-of", "csv=p=0"])
		.arg(&output)
		.output()
		.unwrap();
	let streams = String::from_utf8_lossy(&probed.stdout);
	assert_eq!(streams.lines().filter(|line| *line == "audio").count(), 1, "{streams}");
}

#[test]
fn an_output_file_naming_the_source_itself_is_refused() {
	let dir = tempfile::tempdir().unwrap();
	let source = support::mpeg4(dir.path(), "clip.mp4", 3);
	let before = fs::read(&source).unwrap();
	fs::create_dir(dir.path().join("sub")).unwrap();

	let exact = cryarchiver(dir.path(), &["clip.mp4", "-o", "clip.mp4"]);
	let respelled = cryarchiver(dir.path(), &["clip.mp4", "-o", "sub/../clip.mp4"]);

	assert_eq!(exact.status.code(), Some(0), "{}{}", stdout(&exact), stderr(&exact));
	assert!(stdout(&exact).contains("skipped"), "{}", stdout(&exact));
	assert_eq!(
		respelled.status.code(),
		Some(0),
		"{}{}",
		stdout(&respelled),
		stderr(&respelled)
	);
	assert!(stdout(&respelled).contains("skipped"), "{}", stdout(&respelled));
	assert_eq!(fs::read(&source).unwrap(), before);

	let mut entries: Vec<_> = fs::read_dir(dir.path())
		.unwrap()
		.map(|entry| entry.unwrap().file_name())
		.collect();
	entries.sort();
	assert_eq!(entries, ["clip.mp4", "sub"]);
	assert_eq!(fs::read_dir(dir.path().join("sub")).unwrap().count(), 0);
}

#[cfg(windows)]
#[test]
fn an_output_file_naming_the_source_by_a_different_case_is_refused() {
	let dir = tempfile::tempdir().unwrap();
	let source = support::mpeg4(dir.path(), "clip.mp4", 3);
	let before = fs::read(&source).unwrap();

	let out = cryarchiver(dir.path(), &["clip.mp4", "-o", "CLIP.MP4"]);

	assert_eq!(out.status.code(), Some(0), "{}{}", stdout(&out), stderr(&out));
	assert!(stdout(&out).contains("skipped"), "{}", stdout(&out));
	assert_eq!(fs::read(&source).unwrap(), before);
	assert_eq!(
		fs::read_dir(dir.path())
			.unwrap()
			.map(|entry| entry.unwrap().file_name())
			.collect::<Vec<_>>(),
		["clip.mp4"]
	);
}

#[cfg(windows)]
#[test]
fn a_locked_source_is_retried_automatically() {
	use std::io::{BufRead, BufReader, Read};
	use std::os::windows::fs::OpenOptionsExt;
	use std::process::Stdio;

	let dir = tempfile::tempdir().unwrap();
	let song = flac(dir.path(), "song.flac", 2);
	let mut lock = Some(fs::OpenOptions::new().read(true).share_mode(0).open(&song).unwrap());

	let mut child = Command::new(env!("CARGO_BIN_EXE_cryarchiver"))
		.current_dir(dir.path())
		.arg("song.flac")
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.unwrap();
	let mut child_stderr = child.stderr.take().unwrap();
	let errors = std::thread::spawn(move || {
		let mut text = String::new();
		let _ = child_stderr.read_to_string(&mut text);
		text
	});
	let mut printed = String::new();
	for line in BufReader::new(child.stdout.take().unwrap()).lines() {
		let line = line.unwrap();
		if line.starts_with('↻') {
			lock = None;
		}
		printed.push_str(&line);
		printed.push('\n');
	}
	let status = child.wait().unwrap();
	drop(lock);
	let errors = errors.join().unwrap();

	assert_eq!(status.code(), Some(0), "{printed}{errors}");
	assert!(printed.contains("↻ song.flac  retrying in 5 s"), "{printed}{errors}");
	assert!(dir.path().join("song.cry.opus").is_file(), "{printed}{errors}");
}

#[test]
fn trims_take_the_three_time_spellings() {
	let dir = tempfile::tempdir().unwrap();
	flac(dir.path(), "song.flac", 10);

	let bad = cryarchiver(dir.path(), &["--ss", "1:75", "song.flac"]);
	let out = cryarchiver(dir.path(), &["--ss", "0:02", "--to", "00:00:05.000", "song.flac"]);

	assert_eq!(bad.status.code(), Some(2), "{}", stderr(&bad));
	assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
	assert!(dir.path().join("song.cry.opus").is_file());
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

#[test]
fn resource_flags_run_the_batch() {
	let dir = tempfile::tempdir().unwrap();
	flac(dir.path(), "song.flac", 2);

	let out = cryarchiver(
		dir.path(),
		&["--threads", "2", "--priority", "idle", "--cpu-limit", "50", "song.flac"],
	);

	assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
	assert!(dir.path().join("song.cry.opus").is_file());
}

#[test]
fn low_priority_runs_the_batch() {
	let dir = tempfile::tempdir().unwrap();
	flac(dir.path(), "song.flac", 2);

	let out = cryarchiver(dir.path(), &["--priority", "low", "song.flac"]);

	assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
	assert!(dir.path().join("song.cry.opus").is_file());
}

#[test]
fn compat_is_not_available_yet() {
	let dir = tempfile::tempdir().unwrap();
	flac(dir.path(), "song.flac", 2);

	let out = cryarchiver(dir.path(), &["-c", "compat", "song.flac"]);

	assert_eq!(out.status.code(), Some(2));
	assert!(stderr(&out).contains("COMPATIBILITY"), "{}", stderr(&out));
	assert!(!dir.path().join("song.cry.opus").exists());
}

#[test]
fn a_verbose_run_with_a_failure_writes_nothing_but_the_outputs() {
	let dir = tempfile::tempdir().unwrap();
	let home = tempfile::tempdir().unwrap();
	flac(dir.path(), "good.flac", 2);
	let cut = flac(dir.path(), "cut.flac", 10);
	let bytes = fs::read(&cut).unwrap();
	fs::write(&cut, &bytes[..bytes.len() * 2 / 5]).unwrap();

	let mut command = Command::new(env!("CARGO_BIN_EXE_cryarchiver"));
	for variable in [
		"HOME",
		"APPDATA",
		"LOCALAPPDATA",
		"XDG_CACHE_HOME",
		"XDG_CONFIG_HOME",
		"XDG_STATE_HOME",
	] {
		command.env(variable, home.path());
	}
	let out = command
		.current_dir(dir.path())
		.args(["-v", "good.flac", "cut.flac", "-o", "out"])
		.output()
		.unwrap();

	assert_eq!(out.status.code(), Some(1), "{}{}", stdout(&out), stderr(&out));
	assert!(
		stdout(&out).contains("✗ cut.flac  failed: source damaged"),
		"{}",
		stdout(&out)
	);
	assert!(
		fs::read_dir(home.path()).unwrap().next().is_none(),
		"the home folder gained files"
	);
	let mut names: Vec<String> = fs::read_dir(dir.path())
		.unwrap()
		.map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
		.collect();
	names.sort();
	assert_eq!(names, ["cut.flac", "good.flac", "out"]);
	let produced: Vec<String> = fs::read_dir(dir.path().join("out"))
		.unwrap()
		.map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
		.collect();
	assert_eq!(produced, ["good.cry.opus"]);
}

fn snapshot(dir: &Path) -> Vec<(std::ffi::OsString, Vec<u8>)> {
	let mut files: Vec<_> = fs::read_dir(dir)
		.unwrap()
		.map(|entry| {
			let entry = entry.unwrap();
			(entry.file_name(), fs::read(entry.path()).unwrap())
		})
		.collect();
	files.sort();
	files
}

#[test]
fn a_folder_with_damaged_and_misnamed_files_fails_only_those() {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("in");
	fs::create_dir(&source).unwrap();
	flac(&source, "good.flac", 2);
	let cut = flac(&source, "cut.flac", 10);
	let bytes = fs::read(&cut).unwrap();
	fs::write(&cut, &bytes[..bytes.len() * 2 / 5]).unwrap();
	fs::write(source.join("notes.mp4"), b"not a video\n").unwrap();
	fs::write(source.join("empty.jpg"), b"").unwrap();
	fs::write(
		source.join("error.gif"),
		b"<!DOCTYPE html><html><body>503</body></html>",
	)
	.unwrap();
	let before = snapshot(&source);

	let out = cryarchiver(
		dir.path(),
		&["in", "-o", "out", "--speed", "fast", "--min-savings", "0"],
	);

	let text = stdout(&out);
	assert_eq!(out.status.code(), Some(1), "{text}");
	assert!(text.contains("✓ good.flac"), "{text}");
	assert!(text.contains("✗ cut.flac  failed: source damaged"), "{text}");
	for skipped in ["notes.mp4", "empty.jpg", "error.gif"] {
		assert!(text.contains(&format!("· {skipped}  skipped")), "{text}");
	}

	assert_eq!(snapshot(&source), before);
	let produced: Vec<String> = fs::read_dir(dir.path().join("out"))
		.unwrap()
		.map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
		.collect();
	assert_eq!(produced, ["good.cry.opus"]);
}
