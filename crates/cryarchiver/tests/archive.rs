//! End-to-end project runs with the real engine and ffmpeg, on small generated media.

#[path = "../../cryarchiver-engine/tests/support/mod.rs"]
mod support;

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::path::{MAIN_SEPARATOR_STR, Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

use cryarchiver_project::settings::{Settings, Speed};

fn stdout(out: &Output) -> String {
	String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
	String::from_utf8_lossy(&out.stderr).into_owned()
}

fn run(args: &[&OsStr]) -> Output {
	Command::new(env!("CARGO_BIN_EXE_cryarchiver"))
		.args(args)
		.output()
		.unwrap()
}

fn project(source: &Path, destination: &Path, configure: impl FnOnce(&mut Settings)) -> PathBuf {
	cryarchiver_project::create(source, destination, OsStr::new("archive"), |settings| {
		settings.speed = Speed::Fast;
		settings.stability_window = Duration::ZERO;
		configure(settings);
	})
	.unwrap()
}

/// Every file under `root` with its size and modification time.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, (u64, SystemTime)> {
	let mut files = BTreeMap::new();
	let mut folders = vec![root.to_path_buf()];
	while let Some(folder) = folders.pop() {
		for entry in fs::read_dir(&folder).unwrap().flatten() {
			let meta = entry.metadata().unwrap();
			if meta.is_dir() {
				folders.push(entry.path());
			} else {
				files.insert(entry.path(), (meta.len(), meta.modified().unwrap()));
			}
		}
	}
	files
}

fn native(path: &str) -> String {
	path.replace('/', MAIN_SEPARATOR_STR)
}

#[test]
fn a_project_run_archives_media_copies_the_rest_and_replaces_duplicates() {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("source");
	let destination = dir.path().join("archive");
	for folder in ["Music", "Backup/Music", "Photos", "Videos", "Empty"] {
		fs::create_dir_all(source.join(folder)).unwrap();
	}

	let song = support::tone(&source.join("Music"), "song.flac", "stereo", 2, 3, &["-c:a", "flac"]);
	fs::copy(&song, source.join("Backup").join("Music").join("song.flac")).unwrap();
	fs::write(
		source.join("Music").join("song.cue"),
		b"FILE \"song.flac\" WAVE\n  TRACK 01 AUDIO\n",
	)
	.unwrap();
	support::photo_png(&source.join("Photos"), "photo.png", 320, 240);
	support::mpeg4(&source.join("Videos"), "clip.mp4", 1);
	fs::write(source.join("notes.txt"), b"plain notes").unwrap();

	let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_400_000_001);
	File::options()
		.write(true)
		.open(&song)
		.unwrap()
		.set_modified(old)
		.unwrap();
	let before = snapshot(&source);
	let project = project(&source, &destination, |_| {});

	let out = run(&[OsStr::new("run"), project.as_os_str()]);

	assert_eq!(out.status.code(), Some(0), "{}{}", stdout(&out), stderr(&out));
	assert!(stdout(&out).contains("6 source files → 6 archived"), "{}", stdout(&out));
	assert_eq!(snapshot(&source), before, "the source changed");

	let archived = |path: &str| destination.join(native(path));
	assert!(archived("Music/song.opus").is_file());
	assert_eq!(
		fs::metadata(archived("Music/song.opus")).unwrap().modified().unwrap(),
		old
	);
	assert_eq!(
		fs::read(archived("Music/song.cue")).unwrap(),
		b"FILE \"song.opus\" WAVE\n  TRACK 01 AUDIO\n"
	);
	let placeholder = fs::read_to_string(archived("Backup/Music/placeholder_song.flac.txt")).unwrap();
	assert!(placeholder.contains(&native("../../Music/song.opus")), "{placeholder}");
	assert!(archived("Photos/photo.avif").is_file());
	assert!(archived("Videos/clip.mkv").is_file());
	assert_eq!(fs::read(archived("notes.txt")).unwrap(), b"plain notes");
	assert!(archived("Empty").is_dir());
	assert!(!destination.join(".cryarchiver").join("work").exists());

	let archive = snapshot(&destination);
	let again = run(&[OsStr::new("run"), project.as_os_str()]);

	assert_eq!(again.status.code(), Some(0), "{}{}", stdout(&again), stderr(&again));
	assert_eq!(snapshot(&destination).len(), archive.len());
	assert_eq!(
		snapshot(&destination).get(&archived("Music/song.opus")),
		archive.get(&archived("Music/song.opus")),
		"a finished project is not archived again"
	);
}

#[test]
fn a_deleting_run_removes_only_verified_originals() {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("source");
	let destination = dir.path().join("archive");
	fs::create_dir_all(&source).unwrap();
	let song = support::tone(&source, "song.flac", "stereo", 2, 3, &["-c:a", "flac"]);
	let notes = source.join("notes.txt");
	fs::write(&notes, b"plain notes").unwrap();
	let project = project(&source, &destination, |settings| settings.delete_originals = true);

	let out = run(&[OsStr::new("run"), project.as_os_str(), OsStr::new("--delete-originals")]);

	assert_eq!(out.status.code(), Some(0), "{}{}", stdout(&out), stderr(&out));
	assert!(stderr(&out).contains("DELETING ORIGINALS"), "{}", stderr(&out));
	assert!(stdout(&out).contains("2 originals deleted"), "{}", stdout(&out));
	assert!(!song.exists() && !notes.exists());
	assert!(source.is_dir());
	assert!(destination.join("song.opus").is_file());
	assert_eq!(fs::read(destination.join("notes.txt")).unwrap(), b"plain notes");
}

#[test]
fn a_fast_project_run_writes_no_quality_entries() {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("source");
	let destination = dir.path().join("archive");
	fs::create_dir_all(&source).unwrap();
	support::mpeg4(&source, "clip.mp4", 1);
	let project = project(&source, &destination, |_| {});

	let out = run(&[OsStr::new("run"), project.as_os_str()]);

	assert_eq!(out.status.code(), Some(0), "{}{}", stdout(&out), stderr(&out));
	let mut dumped = Vec::new();
	cryarchiver_project::store::dump(&project, &mut dumped).unwrap();
	let dumped = String::from_utf8(dumped).unwrap();
	let quality = dumped.split("[quality]").nth(1).unwrap();
	assert!(quality.trim().is_empty(), "{dumped}");

	let decisions = dumped.split("[decisions]").nth(1).unwrap();
	let decisions = decisions.split("[unknown]").next().unwrap();
	assert!(decisions.contains("\"clip.mp4\" attempt 1 = Decision"), "{dumped}");
	assert!(
		decisions.contains("chosen: Some(Table)"),
		"the real engine's choice is kept: {dumped}"
	);
}
