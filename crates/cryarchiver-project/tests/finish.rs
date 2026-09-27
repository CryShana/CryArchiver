mod support;

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use cryarchiver_engine::outcome::ErrorKind;
use cryarchiver_project::Command;
use cryarchiver_project::issues::Verb;
use cryarchiver_project::record::{CopyReason, FileState};
use cryarchiver_project::store::Store;
use cryarchiver_project::view::{Parity, RunState};
use support::{FakeEncoder, Rule, Snapshot, jpeg, setup, wait_for, write};

const FLAC: &[u8] = b"fLaC\0\0\0\x22\x10\x00\x10\x00\0\0\0\0\0\0\0\0";

fn state(setup: &support::Setup, path: &str) -> FileState {
	let (_, _, loaded) = Store::open(&setup.project).unwrap();
	loaded
		.files
		.into_iter()
		.find(|(key, _)| key.display() == path)
		.map_or_else(|| panic!("no record for {path}"), |(_, record)| record.state)
}

fn finished(snapshot: &Snapshot) -> bool {
	snapshot
		.overall
		.as_ref()
		.is_some_and(|overall| overall.parity.is_some())
}

fn start(project: &cryarchiver_project::Project) {
	project.send(Command::Start {
		delete_confirmed: false,
	});
}

fn set_folder_time(path: &Path, time: SystemTime) {
	#[cfg(windows)]
	let folder = {
		use std::os::windows::fs::OpenOptionsExt;
		fs::OpenOptions::new()
			.access_mode(0x0100)
			.custom_flags(0x0200_0000)
			.open(path)
			.unwrap()
	};
	#[cfg(not(windows))]
	let folder = fs::File::open(path).unwrap();

	folder
		.set_times(fs::FileTimes::new().set_modified(time).set_accessed(time))
		.unwrap();
}

#[test]
fn a_cue_sheet_is_rewritten_once_its_audio_is_archived() {
	let setup = setup(|_| {});
	write(&setup.source, "Album/disc.flac", FLAC);
	write(
		&setup.source,
		"Album/disc.cue",
		b"TITLE \"Live\"\r\nFILE \"disc.flac\" WAVE\r\n  TRACK 01 AUDIO\r\n",
	);
	let encoder = FakeEncoder::new();
	encoder.rule(".flac", Rule::Encode("opus"));
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();

	start(&project);
	wait_for(&project, &mut snapshot, "the run to finish", finished);
	drop(project);

	assert_eq!(
		fs::read(setup.destination.join("Album").join("disc.cue")).unwrap(),
		b"TITLE \"Live\"\r\nFILE \"disc.opus\" WAVE\r\n  TRACK 01 AUDIO\r\n"
	);
	assert!(matches!(
		state(&setup, "Album/disc.cue"),
		FileState::Done(output) if output.how == cryarchiver_project::record::How::Copied(CopyReason::Rewritten)
	));
}

#[test]
fn a_sidecar_waits_for_its_video_and_keeps_matching_it() {
	let setup = setup(|_| {});
	write(&setup.source, "movie.mp4", &jpeg(1, 100));
	write(
		&setup.source,
		"movie.en.srt",
		b"1\n00:00:01,000 --> 00:00:02,000\nHello\n",
	);
	let encoder = FakeEncoder::new();
	encoder.rule(".mp4", Rule::Encode("mkv"));
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();

	start(&project);
	wait_for(&project, &mut snapshot, "the run to finish", finished);
	drop(project);

	assert!(setup.destination.join("movie.mkv").exists());
	assert!(setup.destination.join("movie.en.srt").exists());
}

#[test]
fn finished_folders_get_their_source_timestamps_and_empty_ones_are_mirrored() {
	let setup = setup(|_| {});
	write(&setup.source, "Photos/a.txt", b"a");
	fs::create_dir_all(setup.source.join("Empty")).unwrap();
	let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_300_000_001);
	set_folder_time(&setup.source.join("Photos"), old);
	set_folder_time(&setup.source.join("Empty"), old);
	let project = support::open(&setup, &FakeEncoder::new());
	let mut snapshot = Snapshot::default();

	start(&project);
	wait_for(&project, &mut snapshot, "the run to finish", finished);
	drop(project);

	for folder in ["Photos", "Empty"] {
		let modified = fs::metadata(setup.destination.join(folder))
			.unwrap()
			.modified()
			.unwrap();
		assert_eq!(modified, old, "{folder}");
	}
}

#[test]
fn the_completion_check_reports_parity_and_the_run_finishes() {
	let setup = setup(|_| {});
	write(&setup.source, "a.jpg", &jpeg(1, 100));
	write(&setup.source, "b.txt", b"b");
	write(&setup.source, "c.jpg", &jpeg(2, 100));
	let encoder = FakeEncoder::new();
	encoder.rule("c.jpg", Rule::Fail(ErrorKind::SourceDamaged("truncated".into())));
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();

	start(&project);
	wait_for(&project, &mut snapshot, "the issue", |s| !s.issues.is_empty());
	wait_for(&project, &mut snapshot, "the first check", finished);
	assert_eq!(
		snapshot.overall.as_ref().unwrap().parity,
		Some(Parity {
			source_files: 3,
			archive_files: 2,
			excluded: 0
		})
	);

	project.send(Command::Resolve(snapshot.issues[0].id, Verb::Exclude));
	wait_for(&project, &mut snapshot, "the clean finish", |s| {
		s.status
			.as_ref()
			.is_some_and(|status| status.run_state == RunState::Finished)
			&& s.overall
				.as_ref()
				.and_then(|overall| overall.parity)
				.is_some_and(|parity| parity.excluded == 1)
	});
	assert_eq!(
		snapshot.overall.as_ref().unwrap().parity,
		Some(Parity {
			source_files: 3,
			archive_files: 2,
			excluded: 1
		})
	);
	assert!((snapshot.overall.as_ref().unwrap().progress - 1.0).abs() < f64::EPSILON);
}

#[test]
fn watch_mode_waits_idle_instead_of_finishing() {
	let setup = setup(|settings| settings.watch = true);
	write(&setup.source, "a.txt", b"a");
	let project = support::open(&setup, &FakeEncoder::new());
	let mut snapshot = Snapshot::default();

	start(&project);
	wait_for(&project, &mut snapshot, "watching", |s| {
		s.status
			.as_ref()
			.is_some_and(|status| status.run_state == RunState::Watching)
	});
}
