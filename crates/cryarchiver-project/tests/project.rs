mod support;

use std::ffi::OsStr;
use std::fs;
use std::sync::Arc;

use cryarchiver_project::keys::RelPath;
use cryarchiver_project::record::{CopyReason, FileRecord, FileState, Hash, How, Output};
use cryarchiver_project::store::{Store, StoreError};
use cryarchiver_project::tree::FolderId;
use cryarchiver_project::view::{FilterEntry, NoticeLevel, RowState, RunState};
use cryarchiver_project::{Command, Project, ProjectError, ProjectOptions};
use support::{FakeBackend, FakeEncoder, Snapshot, setup, wait_for, write};

fn key(path: &str) -> RelPath {
	RelPath::from_bytes(path.as_bytes().to_vec())
}

fn copied(path: &str, size: u64) -> Output {
	Output {
		how: How::Copied(CopyReason::NotMedia),
		path: key(path),
		size,
		hash: Some(Hash([1; 32])),
	}
}

fn records(setup: &support::Setup) -> Vec<(String, FileRecord)> {
	let (_, _, loaded) = Store::open(&setup.project).unwrap();
	loaded
		.files
		.into_iter()
		.map(|(key, record)| (key.display(), record))
		.collect()
}

fn state_of(setup: &support::Setup, path: &str) -> FileState {
	records(setup)
		.into_iter()
		.find(|(key, _)| key == path)
		.map_or_else(|| panic!("no record for {path}"), |(_, record)| record.state)
}

#[test]
fn nested_roots_are_refused_at_creation() {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("source");
	fs::create_dir_all(&source).unwrap();

	let inside = cryarchiver_project::create(&source, &source.join("archive"), OsStr::new("a"), |_| {});

	assert!(matches!(inside, Err(ProjectError::Settings(_))));
}

#[test]
fn opening_scans_the_source_into_folders_and_rows() {
	let setup = setup(|_| {});
	write(&setup.source, "notes.txt", b"hello");
	write(&setup.source, "Photos/2021/a.txt", b"a");
	write(&setup.source, "Photos/b.txt", b"b");
	fs::create_dir_all(setup.source.join("Empty")).unwrap();
	let encoder = FakeEncoder::new();

	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();
	wait_for(&project, &mut snapshot, "the scan", Snapshot::scan_complete);
	wait_for(&project, &mut snapshot, "three files", |s| s.root().files_total == 3);

	let names: Vec<&str> = snapshot.folders.iter().map(|(_, _, name)| name.as_str()).collect();
	assert_eq!(names, ["Empty", "Photos", "2021"]);
	assert_eq!(snapshot.status.as_ref().unwrap().run_state, RunState::Paused);

	project.send(Command::Watch(FolderId::ROOT, true));
	wait_for(&project, &mut snapshot, "the root rows", |s| {
		s.rows.contains_key(&FolderId::ROOT)
	});
	let row = snapshot.row(FolderId::ROOT, "notes.txt");
	assert_eq!(row.status.state, RowState::Queued);
	assert_eq!(row.status.bytes_in, 5);
	assert!(encoder.calls().is_empty());
}

#[test]
fn a_second_open_is_refused() {
	let setup = setup(|_| {});
	let encoder = FakeEncoder::new();
	let _first = support::open(&setup, &encoder);

	let second = Project::open(
		&setup.project,
		ProjectOptions {
			backend: Arc::new(FakeBackend { encoder }),
			tuning: support::tuning(),
		},
	);

	assert!(matches!(second, Err(ProjectError::Store(StoreError::AlreadyOpen))));
}

#[test]
fn reopening_keeps_what_the_scan_found() {
	let setup = setup(|_| {});
	write(&setup.source, "Music/a.txt", b"a");
	let encoder = FakeEncoder::new();
	{
		let project = support::open(&setup, &encoder);
		let mut snapshot = Snapshot::default();
		wait_for(&project, &mut snapshot, "the scan", |s| {
			s.scan_complete() && s.root().files_total == 1
		});
	}
	write(&setup.source, "Music/b.txt", b"b");

	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();
	let first = project.wait_update(support::WAIT).unwrap();
	wait_for(&project, &mut snapshot, "the rescan", |s| {
		s.scan_complete() && s.root().files_total == 2
	});

	assert!(matches!(first, cryarchiver_project::view::Update::Settings(_)));
	assert_eq!(
		snapshot.folders.iter().filter(|(_, _, name)| name == "Music").count(),
		1
	);
	drop(project);
	assert_eq!(records(&setup).len(), 2);
}

#[test]
fn the_filter_answers_with_matches_and_their_ancestors() {
	let setup = setup(|_| {});
	write(&setup.source, "Photos/Beach/sunset.txt", b"1");
	write(&setup.source, "Photos/Beach/other.txt", b"2");
	write(&setup.source, "Docs/Sunset notes.txt", b"3");
	let encoder = FakeEncoder::new();
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();
	wait_for(&project, &mut snapshot, "the scan", |s| {
		s.scan_complete() && s.root().files_total == 3
	});

	project.send(Command::Filter("SUNSET".into()));
	wait_for(&project, &mut snapshot, "the filter", |s| s.filter.is_some());

	let filter = snapshot.filter.clone().unwrap();
	assert_eq!(filter.matching_files, 2);
	let shape: Vec<(u16, String)> = filter
		.rows
		.iter()
		.map(|(depth, entry)| match entry {
			FilterEntry::Folder(id) if *id == FolderId::ROOT => (*depth, "/".to_owned()),
			FilterEntry::Folder(id) => (*depth, snapshot.folders.iter().find(|f| f.0 == *id).unwrap().2.clone()),
			FilterEntry::File { row, .. } => (*depth, row.name.clone()),
		})
		.collect();
	assert_eq!(
		shape,
		[
			(0, "/".to_owned()),
			(1, "Docs".to_owned()),
			(2, "Sunset notes.txt".to_owned()),
			(1, "Photos".to_owned()),
			(2, "Beach".to_owned()),
			(3, "sunset.txt".to_owned()),
		]
	);

	project.send(Command::Filter(String::new()));
	wait_for(&project, &mut snapshot, "the filter to clear", |s| s.filter.is_none());
}

#[test]
fn recovery_requeues_running_work_and_finishes_placed_commits() {
	let setup = setup(|_| {});
	write(&setup.source, "running.txt", b"1");
	write(&setup.source, "placed.txt", b"22");
	write(&setup.source, "lost.txt", b"333");
	write(&setup.destination, "placed.txt", b"22");
	let modified = |name: &str| {
		cryarchiver_project::record::mtime_ns(fs::metadata(setup.source.join(name)).unwrap().modified().unwrap())
	};
	{
		let (mut store, _, _) = Store::open(&setup.project).unwrap();
		let record = |name: &str, size, state| FileRecord {
			state,
			..FileRecord::discovered(size, modified(name))
		};
		store
			.write_intent(
				&key("running.txt"),
				&record("running.txt", 1, FileState::InProgress { attempt: 1 }),
			)
			.unwrap();
		for (name, size) in [("placed.txt", 2), ("lost.txt", 3)] {
			let state = FileState::Committing {
				attempt: 1,
				output: copied(name, size),
			};
			store.write_intent(&key(name), &record(name, size, state)).unwrap();
		}
	}

	drop(support::open(&setup, &FakeEncoder::new()));

	assert_eq!(state_of(&setup, "running.txt"), FileState::Queued);
	assert_eq!(state_of(&setup, "placed.txt"), FileState::Done(copied("placed.txt", 2)));
	assert_eq!(state_of(&setup, "lost.txt"), FileState::Queued);
}

#[test]
fn changed_and_deleted_sources_are_reconciled() {
	let setup = setup(|_| {});
	write(&setup.source, "edited.txt", b"new content");
	write(&setup.destination, "edited.txt", b"old");
	write(&setup.destination, "deleted.txt", b"gone");
	{
		let (mut store, _, _) = Store::open(&setup.project).unwrap();
		let done = |size, output| FileRecord {
			state: FileState::Done(output),
			..FileRecord::discovered(size, 1)
		};
		store
			.set_file(&key("edited.txt"), &done(3, copied("edited.txt", 3)))
			.unwrap();
		store
			.set_file(&key("deleted.txt"), &done(4, copied("deleted.txt", 4)))
			.unwrap();
		store
			.set_file(&key("never.txt"), &FileRecord::discovered(5, 1))
			.unwrap();
		store.flush().unwrap();
	}

	let project = support::open(&setup, &FakeEncoder::new());
	let mut snapshot = Snapshot::default();
	wait_for(&project, &mut snapshot, "the scan", Snapshot::scan_complete);
	drop(project);

	let all = records(&setup);
	let edited = &all.iter().find(|(key, _)| key == "edited.txt").unwrap().1;
	assert_eq!(edited.state, FileState::Discovered);
	assert_eq!(edited.size, 11);
	assert_eq!(edited.replaces, Some(key("edited.txt")));
	assert_eq!(
		state_of(&setup, "deleted.txt"),
		FileState::SourceGone(copied("deleted.txt", 4))
	);
	assert!(all.iter().all(|(key, _)| key != "never.txt"));
}

#[test]
fn a_folder_link_back_to_an_ancestor_is_reported_once() {
	let setup = setup(|_| {});
	write(&setup.source, "Photos/a.txt", b"a");
	let link = setup.source.join("Photos").join("loop");
	#[cfg(unix)]
	let linked = std::os::unix::fs::symlink(&setup.source, &link).is_ok();
	#[cfg(windows)]
	let linked = std::process::Command::new("cmd")
		.arg("/C")
		.arg("mklink")
		.arg("/J")
		.arg(&link)
		.arg(&setup.source)
		.output()
		.is_ok_and(|out| out.status.success());
	if !linked {
		eprintln!("skipped: can't create a folder link here");
		return;
	}

	let project = support::open(&setup, &FakeEncoder::new());
	let mut snapshot = Snapshot::default();
	wait_for(&project, &mut snapshot, "the scan", Snapshot::scan_complete);
	wait_for(&project, &mut snapshot, "the cycle issue", |s| !s.issues.is_empty());

	assert_eq!(snapshot.issues.len(), 1);
	assert_eq!(snapshot.issues[0].location, "Photos/loop");
	assert_eq!(snapshot.root().files_total, 1);
}

#[test]
fn a_vanished_folder_drops_its_whole_subtree() {
	let setup = setup(|_| {});
	write(&setup.destination, "A/B/deep.txt", b"deep");
	{
		let (mut store, _, _) = Store::open(&setup.project).unwrap();
		let deep = FileRecord {
			state: FileState::Done(copied("A/B/deep.txt", 4)),
			..FileRecord::discovered(4, 1)
		};
		store.set_file(&key("A/B/deep.txt"), &deep).unwrap();
		store
			.set_file(&key("A/B/C/queued.txt"), &FileRecord::discovered(5, 1))
			.unwrap();
		store
			.set_file(&key("A/top.txt"), &FileRecord::discovered(6, 1))
			.unwrap();
		store.flush().unwrap();
	}

	let project = support::open(&setup, &FakeEncoder::new());
	let mut snapshot = Snapshot::default();
	wait_for(&project, &mut snapshot, "the scan", Snapshot::scan_complete);
	drop(project);

	let all = records(&setup);
	assert_eq!(
		state_of(&setup, "A/B/deep.txt"),
		FileState::SourceGone(copied("A/B/deep.txt", 4))
	);
	assert!(
		all.iter()
			.all(|(key, _)| key != "A/B/C/queued.txt" && key != "A/top.txt")
	);
}

fn link_folder(link: &std::path::Path, target: &std::path::Path) -> bool {
	#[cfg(unix)]
	return std::os::unix::fs::symlink(target, link).is_ok();

	#[cfg(windows)]
	return std::process::Command::new("cmd")
		.arg("/C")
		.arg("mklink")
		.arg("/J")
		.arg(link)
		.arg(target)
		.output()
		.is_ok_and(|out| out.status.success());
}

#[test]
fn a_folder_link_to_a_folder_holding_the_source_is_a_cycle() {
	let setup = setup(|_| {});
	write(&setup.source, "a.txt", b"a");
	write(&setup.dir.path().join("beside"), "b.txt", b"b");
	if !link_folder(&setup.source.join("up"), setup.dir.path()) {
		eprintln!("skipped: can't create a folder link here");
		return;
	}

	let project = support::open(&setup, &FakeEncoder::new());
	let mut snapshot = Snapshot::default();
	wait_for(&project, &mut snapshot, "the scan", Snapshot::scan_complete);
	wait_for(&project, &mut snapshot, "the cycle issue", |s| !s.issues.is_empty());

	assert_eq!(snapshot.issues.len(), 1);
	assert_eq!(snapshot.issues[0].location, "up");
	assert_eq!(snapshot.root().files_total, 1);
}

#[test]
fn a_cycle_whose_link_is_removed_is_no_longer_an_issue() {
	let setup = setup(|_| {});
	write(&setup.source, "Photos/a.txt", b"a");
	let link = setup.source.join("Photos").join("loop");
	if !link_folder(&link, &setup.source) {
		eprintln!("skipped: can't create a folder link here");
		return;
	}
	let project = support::open(&setup, &FakeEncoder::new());
	let mut snapshot = Snapshot::default();
	wait_for(&project, &mut snapshot, "the cycle issue", |s| !s.issues.is_empty());

	#[cfg(windows)]
	fs::remove_dir(&link).unwrap();
	#[cfg(unix)]
	fs::remove_file(&link).unwrap();
	project.send(Command::Rescan);

	wait_for(&project, &mut snapshot, "the issue to close", |s| s.issues.is_empty());
	assert!(setup.source.join("Photos").join("a.txt").exists());
}

#[test]
fn a_project_that_deletes_originals_needs_the_confirmation_to_start() {
	let setup = setup(|settings| settings.delete_originals = true);
	let project = support::open(&setup, &FakeEncoder::new());
	let mut snapshot = Snapshot::default();

	project.send(Command::Start {
		delete_confirmed: false,
	});
	wait_for(&project, &mut snapshot, "the refusal", |s| {
		s.notices.iter().any(|notice| notice.level == NoticeLevel::Error)
	});
	assert_eq!(snapshot.status.as_ref().unwrap().run_state, RunState::Paused);

	project.send(Command::Start { delete_confirmed: true });
	wait_for(&project, &mut snapshot, "the start", |s| {
		s.status
			.as_ref()
			.is_some_and(|status| status.run_state == RunState::Running && status.deleting_originals)
	});
}
