mod support;

use std::fs;
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cryarchiver_project::issues::{Problem, Verb};
use cryarchiver_project::record::FileState;
use cryarchiver_project::store::Store;
use cryarchiver_project::view::RunState;
use cryarchiver_project::{Command, Project, ProjectOptions};
use support::{FakeBackend, FakeEncoder, Rule, Snapshot, jpeg, setup, wait_for, write};

const FLAC: &[u8] = b"fLaC\0\0\0\x22\x10\x00\x10\x00\0\0\0\0\0\0\0\0";

fn start(project: &Project) {
	project.send(Command::Start {
		delete_confirmed: false,
	});
}

fn watching(snapshot: &Snapshot) -> bool {
	snapshot
		.status
		.as_ref()
		.is_some_and(|status| status.run_state == RunState::Watching)
}

fn wait_for_file(project: &Project, snapshot: &mut Snapshot, path: &std::path::Path) {
	wait_for(project, snapshot, &format!("{}", path.display()), |_| path.exists());
}

fn state(setup: &support::Setup, path: &str) -> FileState {
	let (_, _, loaded) = Store::open(&setup.project).unwrap();
	loaded
		.files
		.into_iter()
		.find(|(key, _)| key.display() == path)
		.map_or_else(|| panic!("no record for {path}"), |(_, record)| record.state)
}

#[test]
fn files_created_during_a_watched_run_are_archived() {
	let setup = setup(|settings| settings.watch = true);
	write(&setup.source, "Captures/first.txt", b"1");
	let project = support::open(&setup, &FakeEncoder::new());
	let mut snapshot = Snapshot::default();
	start(&project);
	wait_for(&project, &mut snapshot, "watching", watching);

	write(&setup.source, "Captures/second.txt", b"22");
	write(&setup.source, "Captures/New/third.jpg", &jpeg(1, 100));

	wait_for_file(
		&project,
		&mut snapshot,
		&setup.destination.join("Captures").join("second.txt"),
	);
	wait_for_file(
		&project,
		&mut snapshot,
		&setup.destination.join("Captures").join("New").join("third.avif"),
	);
	wait_for(&project, &mut snapshot, "watching again", watching);
}

#[test]
fn a_work_area_linked_into_the_source_while_watching_gets_nothing_made_in_it_or_removed_from_it() {
	let setup = setup(|settings| settings.watch = true);
	write(&setup.source, "first.jpg", &jpeg(1, 100));
	let victim = setup.source.join("Other");
	fs::create_dir_all(victim.join("work")).unwrap();
	let project = support::open(&setup, &FakeEncoder::new());
	let mut snapshot = Snapshot::default();
	start(&project);
	wait_for_file(&project, &mut snapshot, &setup.destination.join("first.avif"));
	wait_for(&project, &mut snapshot, "watching", watching);

	let work_area = setup.destination.join(".cryarchiver");
	if work_area.exists() {
		fs::remove_dir_all(&work_area).unwrap();
	}
	if !support::link_folder(&work_area, &victim) {
		eprintln!("skipped: no folder link can be made here");
		return;
	}
	write(&setup.source, "second.jpg", &jpeg(2, 100));

	wait_for(&project, &mut snapshot, "an issue for second.jpg", |s| {
		s.issues.iter().any(|issue| issue.name == "second.jpg")
	});
	drop(project);

	assert_eq!(support::names_in(&victim), ["work"]);
	assert!(support::names_in(&victim.join("work")).is_empty());
	assert!(!setup.destination.join("second.avif").exists());
	let FileState::Issue { reason, .. } = state(&setup, "second.jpg") else {
		panic!("second.jpg is not an issue: {:?}", state(&setup, "second.jpg"));
	};
	assert!(
		reason.contains(&support::shown(&work_area)) && reason.contains("link"),
		"{reason}"
	);
}

#[test]
fn a_file_written_slowly_is_archived_only_once_it_is_still() {
	let setup = setup(|settings| {
		settings.watch = true;
		settings.stability_window = Duration::from_millis(600);
	});
	let project = support::open(&setup, &FakeEncoder::new());
	let mut snapshot = Snapshot::default();
	start(&project);
	wait_for(&project, &mut snapshot, "watching", watching);

	let path = setup.source.join("recording.txt");
	let mut file = fs::File::create(&path).unwrap();
	let mut written = Vec::new();
	for chunk in 0..8u8 {
		let part = vec![b'a' + chunk; 1000];
		file.write_all(&part).unwrap();
		file.flush().unwrap();
		written.extend_from_slice(&part);
		std::thread::sleep(Duration::from_millis(150));
	}
	drop(file);

	let output = setup.destination.join("recording.txt");
	wait_for_file(&project, &mut snapshot, &output);
	assert_eq!(fs::read(&output).unwrap(), written);
}

#[cfg(windows)]
#[test]
fn a_file_another_program_holds_open_for_writing_waits() {
	use std::fs::OpenOptions;
	use std::os::windows::fs::OpenOptionsExt;

	let setup = setup(|settings| settings.watch = true);
	let path = setup.source.join("capture.txt");
	let mut writer = OpenOptions::new()
		.create(true)
		.truncate(true)
		.write(true)
		.share_mode(0x1 | 0x2)
		.open(&path)
		.unwrap();
	writer.write_all(b"still recording").unwrap();
	write(&setup.source, "free.txt", b"free");
	let project = support::open(&setup, &FakeEncoder::new());
	let mut snapshot = Snapshot::default();
	start(&project);

	wait_for_file(&project, &mut snapshot, &setup.destination.join("free.txt"));
	assert!(!setup.destination.join("capture.txt").exists());

	drop(writer);
	wait_for_file(&project, &mut snapshot, &setup.destination.join("capture.txt"));
}

#[test]
fn a_cue_sheet_in_a_folder_that_keeps_changing_is_not_held_back() {
	let setup = setup(|settings| settings.watch = true);
	write(&setup.source, "Live/show.flac", FLAC);
	write(&setup.source, "Live/show.cue", b"FILE \"show.flac\" WAVE\n");
	let encoder = FakeEncoder::new();
	encoder.rule(".flac", Rule::Encode("opus"));
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();
	start(&project);

	let cue = setup.destination.join("Live").join("show.cue");
	let deadline = Instant::now() + support::WAIT;
	let mut added = 0;
	while !cue.exists() {
		assert!(Instant::now() < deadline, "the cue sheet was held back");
		write(&setup.source, &format!("Live/extra-{added}.txt"), b"x");
		added += 1;
		std::thread::sleep(Duration::from_millis(50));
		support::drain(&project, &mut snapshot);
	}

	assert_eq!(fs::read(&cue).unwrap(), b"FILE \"show.opus\" WAVE\n");
}

#[test]
fn unknown_destination_files_are_reported_never_deleted_and_can_be_acknowledged() {
	let setup = setup(|_| {});
	write(&setup.source, "a.txt", b"a");
	write(&setup.destination, "Old/notes.doc", b"from before");
	let project = support::open(&setup, &FakeEncoder::new());
	let mut snapshot = Snapshot::default();
	start(&project);

	wait_for(&project, &mut snapshot, "the unknown file", |s| {
		s.issues.iter().any(|issue| issue.problem == Problem::UnknownFile)
	});
	let issue = snapshot
		.issues
		.iter()
		.find(|issue| issue.problem == Problem::UnknownFile)
		.unwrap()
		.clone();
	assert_eq!(issue.location, "Old/notes.doc");
	assert_eq!(issue.verbs.accept, Some("Acknowledge"));

	project.send(Command::Resolve(issue.id, Verb::Accept));
	wait_for(&project, &mut snapshot, "the acknowledgement", |s| s.issues.is_empty());
	drop(project);

	assert_eq!(
		fs::read(setup.destination.join("Old").join("notes.doc")).unwrap(),
		b"from before"
	);
	let project = support::open(&setup, &FakeEncoder::new());
	let mut snapshot = Snapshot::default();
	start(&project);
	wait_for(
		&project,
		&mut snapshot,
		"the destination walk and the finish",
		Snapshot::finished,
	);
	support::drain(&project, &mut snapshot);
	assert!(snapshot.issues.is_empty(), "{:?}", snapshot.issues);
}

#[test]
fn a_missing_output_is_reported_and_redo_archives_the_file_again() {
	let setup = setup(|_| {});
	write(&setup.source, "a.txt", b"hello");
	let encoder = FakeEncoder::new();
	{
		let project = support::open(&setup, &encoder);
		let mut snapshot = Snapshot::default();
		start(&project);
		wait_for_file(&project, &mut snapshot, &setup.destination.join("a.txt"));
		wait_for(&project, &mut snapshot, "the finish", |s| s.root().complete);
	}
	fs::remove_file(setup.destination.join("a.txt")).unwrap();

	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();
	wait_for(&project, &mut snapshot, "the missing output", |s| {
		s.issues.iter().any(|issue| issue.problem == Problem::OutputMissing)
	});
	let issue = snapshot.issues[0].clone();
	assert_eq!(issue.reason, "the archived file is missing");
	assert_eq!(issue.verbs.retry, Some("Redo"));

	start(&project);
	project.send(Command::Resolve(issue.id, Verb::Retry));
	wait_for_file(&project, &mut snapshot, &setup.destination.join("a.txt"));
	assert_eq!(fs::read(setup.destination.join("a.txt")).unwrap(), b"hello");
}

#[test]
fn accept_as_is_keeps_a_changed_output_without_a_hash() {
	let setup = setup(|_| {});
	write(&setup.source, "a.txt", b"hello");
	let encoder = FakeEncoder::new();
	{
		let project = support::open(&setup, &encoder);
		let mut snapshot = Snapshot::default();
		start(&project);
		wait_for(&project, &mut snapshot, "the finish", |s| s.root().complete);
	}
	fs::write(setup.destination.join("a.txt"), b"edited by hand").unwrap();

	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();
	wait_for(&project, &mut snapshot, "the changed output", |s| !s.issues.is_empty());
	assert_eq!(snapshot.issues[0].reason, "the archived file changed size");
	project.send(Command::Resolve(snapshot.issues[0].id, Verb::Accept));
	wait_for(&project, &mut snapshot, "the acceptance", |s| s.issues.is_empty());
	drop(project);

	let FileState::Done(output) = state(&setup, "a.txt") else {
		panic!("not done")
	};
	assert_eq!((output.size, output.hash), (14, None));
}

#[test]
fn the_safety_pass_finds_changes_without_the_watcher() {
	let setup = setup(|settings| settings.watch = true);
	write(&setup.source, "Inbox/a.txt", b"a");
	let project = Project::open(
		&setup.project,
		ProjectOptions {
			backend: Arc::new(FakeBackend {
				encoder: FakeEncoder::new(),
			}),
			tuning: cryarchiver_project::Tuning {
				watcher: false,
				safety_pass: Duration::from_millis(200),
				..support::tuning()
			},
			fault: None,
		},
	)
	.unwrap();
	let mut snapshot = Snapshot::default();
	start(&project);
	wait_for(&project, &mut snapshot, "watching", watching);

	write(&setup.source, "Inbox/b.txt", b"b");

	wait_for_file(&project, &mut snapshot, &setup.destination.join("Inbox").join("b.txt"));
}
