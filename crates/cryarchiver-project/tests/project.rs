mod support;

use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use cryarchiver_project::keys::RelPath;
use cryarchiver_project::record::{CopyReason, FileRecord, FileState, Hash, How, IssueKind, Output, mtime_ns};
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

/// A copy that carries the hash of `content`, as a real commit's does.
fn copy_of(path: &str, content: &[u8]) -> Output {
	Output {
		hash: Some(hash_of(content)),
		..copied(path, content.len() as u64)
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

fn record_of(setup: &support::Setup, path: &str) -> FileRecord {
	records(setup)
		.into_iter()
		.find(|(key, _)| key == path)
		.map_or_else(|| panic!("no record for {path}"), |(_, record)| record)
}

fn state_of(setup: &support::Setup, path: &str) -> FileState {
	record_of(setup, path).state
}

fn modified(path: &Path) -> i64 {
	mtime_ns(fs::metadata(path).unwrap().modified().unwrap())
}

fn hash_of(content: &[u8]) -> Hash {
	Hash(*blake3::hash(content).as_bytes())
}

/// Stores `record` for the source file `name`, with the size and time the source has now.
fn seed(setup: &support::Setup, name: &str, record: FileRecord) {
	let path = setup.source.join(name);
	let record = FileRecord {
		size: fs::metadata(&path).unwrap().len(),
		mtime_ns: modified(&path),
		..record
	};

	let (mut store, _, _) = Store::open(&setup.project).unwrap();
	store.set_file(&key(name), &record).unwrap();
	store.flush().unwrap();
}

fn with_state(state: FileState) -> FileRecord {
	FileRecord {
		state,
		..FileRecord::default()
	}
}

fn committing(output: Output, replaces: Option<&str>) -> FileRecord {
	FileRecord {
		replaces: replaces.map(key),
		..with_state(FileState::Committing { attempt: 1, output })
	}
}

fn link_folder(link: &Path, target: &Path) -> bool {
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

fn scans_completed(snapshot: &Snapshot) -> usize {
	snapshot
		.logs
		.iter()
		.filter(|entry| entry.message.starts_with("scan complete"))
		.count()
}

fn rescan(project: &Project, snapshot: &mut Snapshot) {
	let before = scans_completed(snapshot);
	project.send(Command::Rescan);
	wait_for(project, snapshot, "the rescan", |s| scans_completed(s) > before);
}

/// Opens the project, waits for its first scan and closes it again.
fn open_and_scan(setup: &support::Setup) {
	let project = support::open(setup, &FakeEncoder::new());
	let mut snapshot = Snapshot::default();
	wait_for(&project, &mut snapshot, "the scan", Snapshot::scan_complete);
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
			fault: None,
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
	{
		let (mut store, _, _) = Store::open(&setup.project).unwrap();
		let record = |name: &str, size, state| FileRecord {
			state,
			..FileRecord::discovered(size, modified(&setup.source.join(name)))
		};
		store
			.write_intent(
				&key("running.txt"),
				&record("running.txt", 1, FileState::InProgress { attempt: 1 }),
			)
			.unwrap();
		for (name, content) in [("placed.txt", b"22".as_slice()), ("lost.txt", b"333")] {
			let state = FileState::Committing {
				attempt: 1,
				output: copy_of(name, content),
			};
			let size = content.len() as u64;
			store.write_intent(&key(name), &record(name, size, state)).unwrap();
		}
	}

	drop(support::open(&setup, &FakeEncoder::new()));

	assert_eq!(state_of(&setup, "running.txt"), FileState::Queued);
	assert_eq!(
		state_of(&setup, "placed.txt"),
		FileState::Done(copy_of("placed.txt", b"22"))
	);
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
	assert!(matches!(
		edited.state,
		FileState::Discovered | FileState::Ready | FileState::Queued
	));
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
	if !link_folder(&setup.source.join("Photos").join("loop"), &setup.source) {
		eprintln!("skipped: can't create a folder link here");
		return;
	}
	let cycle_logs = |snapshot: &Snapshot| {
		snapshot
			.logs
			.iter()
			.filter(|entry| entry.message.contains("leads back"))
			.count()
	};

	let project = support::open(&setup, &FakeEncoder::new());
	let mut snapshot = Snapshot::default();
	wait_for(&project, &mut snapshot, "the scan", Snapshot::scan_complete);
	wait_for(&project, &mut snapshot, "the cycle issue", |s| !s.issues.is_empty());

	assert_eq!(snapshot.issues.len(), 1);
	assert_eq!(snapshot.issues[0].location, "Photos/loop");
	assert_eq!(snapshot.root().files_total, 1);
	assert_eq!(cycle_logs(&snapshot), 1);

	rescan(&project, &mut snapshot);
	assert_eq!(cycle_logs(&snapshot), 1);
	drop(project);

	let project = support::open(&setup, &FakeEncoder::new());
	let mut snapshot = Snapshot::default();
	wait_for(&project, &mut snapshot, "the scan after reopening", |s| {
		s.scan_complete() && !s.issues.is_empty()
	});
	assert_eq!(snapshot.issues.len(), 1);
	assert_eq!(cycle_logs(&snapshot), 0);
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
	wait_for(&project, &mut snapshot, "the root totals", |s| {
		let root = s.root();
		root.files_total == 1 && root.files_done == 1
	});
	drop(project);

	let root = snapshot.root();
	assert!(root.complete);
	assert_eq!((root.bytes_in, root.bytes_out, root.open_issues), (4, 4, 0));
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

#[test]
fn recovery_requeues_a_re_archive_whose_own_old_output_is_still_in_place() {
	let setup = setup(|_| {});
	write(&setup.source, "same.txt", b"new!!");
	write(&setup.source, "unhashed.txt", b"new!!");
	write(&setup.destination, "same.txt", b"old!!");
	write(&setup.destination, "unhashed.txt", b"new!!");
	let output = |name: &str, hash| Output {
		hash,
		..copied(name, 5)
	};
	seed(
		&setup,
		"same.txt",
		committing(output("same.txt", Some(hash_of(b"new!!"))), Some("same.txt")),
	);
	seed(
		&setup,
		"unhashed.txt",
		committing(output("unhashed.txt", None), Some("unhashed.txt")),
	);

	open_and_scan(&setup);

	for name in ["same.txt", "unhashed.txt"] {
		let record = record_of(&setup, name);
		assert_eq!(record.state, FileState::Queued, "{name}");
		assert_eq!(record.replaces, Some(key(name)), "{name}");
	}
	assert_eq!(fs::read(setup.destination.join("same.txt")).unwrap(), b"old!!");
}

#[test]
fn recovery_finishes_a_re_archive_placed_over_its_own_old_output() {
	let setup = setup(|_| {});
	write(&setup.source, "same.txt", b"new!!");
	write(&setup.destination, "same.txt", b"new!!");
	let output = Output {
		hash: Some(hash_of(b"new!!")),
		..copied("same.txt", 5)
	};
	seed(&setup, "same.txt", committing(output.clone(), Some("same.txt")));

	open_and_scan(&setup);

	let record = record_of(&setup, "same.txt");
	assert_eq!(record.state, FileState::Done(output));
	assert_eq!(record.replaces, None);
}

#[test]
fn recovery_removes_the_replaced_output_only_once_the_new_one_is_placed() {
	let setup = setup(|_| {});
	write(&setup.source, "photo.jpg", b"new photo");
	write(&setup.source, "short.jpg", b"new short");
	write(&setup.destination, "photo.avif", b"encoded new photo");
	write(&setup.destination, "photo.webp", b"old output");
	write(&setup.destination, "short.avif", b"cut");
	write(&setup.destination, "short.webp", b"old output");
	write(&setup.destination, "unrelated.txt", b"someone else's");
	let encoded = |name: &str, content: &[u8]| Output {
		how: How::Encoded,
		path: key(name),
		size: content.len() as u64,
		hash: Some(hash_of(content)),
	};
	let placed = encoded("photo.avif", b"encoded new photo");
	seed(&setup, "photo.jpg", committing(placed.clone(), Some("photo.webp")));
	seed(
		&setup,
		"short.jpg",
		committing(encoded("short.avif", b"encoded new short"), Some("short.webp")),
	);

	open_and_scan(&setup);

	let photo = record_of(&setup, "photo.jpg");
	assert_eq!(photo.state, FileState::Done(placed));
	assert_eq!(photo.replaces, None);
	assert!(!setup.destination.join("photo.webp").exists());
	assert!(setup.destination.join("photo.avif").exists());

	let short = record_of(&setup, "short.jpg");
	assert_eq!(short.state, FileState::Queued);
	assert_eq!(short.replaces, Some(key("short.webp")));
	assert!(setup.destination.join("short.webp").exists());
	assert!(setup.destination.join("unrelated.txt").exists());
}

#[test]
fn an_unreadable_source_keeps_an_archived_files_output() {
	let setup = setup(|_| {});
	write(&setup.destination, "clip.mp4", b"clip");
	{
		let (mut store, _, _) = Store::open(&setup.project).unwrap();
		let done = FileRecord {
			state: FileState::Done(copied("clip.mp4", 4)),
			..FileRecord::discovered(4, 1)
		};
		store.set_file(&key("clip.mp4"), &done).unwrap();
		store
			.set_file(&key("fresh.mp4"), &FileRecord::discovered(5, 1))
			.unwrap();
		store.flush().unwrap();
	}
	let target = setup.dir.path().join("target");
	fs::create_dir(&target).unwrap();
	for name in ["clip.mp4", "fresh.mp4"] {
		if !link_folder(&setup.source.join(name), &target) {
			eprintln!("skipped: can't create a folder link here");
			return;
		}
	}
	fs::remove_dir(&target).unwrap();

	let project = support::open(&setup, &FakeEncoder::new());
	let mut snapshot = Snapshot::default();
	wait_for(&project, &mut snapshot, "the scan", Snapshot::scan_complete);
	wait_for(&project, &mut snapshot, "the warning", |s| {
		s.notices
			.iter()
			.any(|notice| notice.level == NoticeLevel::Warning && notice.text.starts_with("clip.mp4 can't be read: "))
	});
	drop(project);

	assert_eq!(state_of(&setup, "clip.mp4"), FileState::Done(copied("clip.mp4", 4)));
	assert!(matches!(
		state_of(&setup, "fresh.mp4"),
		FileState::Issue {
			kind: IssueKind::SourceUnreadable,
			..
		}
	));
}

#[test]
fn a_folder_moved_out_and_back_is_archived_as_it_was() {
	let setup = setup(|_| {});
	write(&setup.source, "A/a.txt", b"a");
	write(&setup.source, "A/B/b.txt", b"bb");
	seed(&setup, "A/a.txt", with_state(FileState::Done(copied("A/a.txt", 1))));
	seed(&setup, "A/B/b.txt", with_state(FileState::Done(copied("A/B/b.txt", 2))));
	let moved = setup.dir.path().join("moved");

	{
		let project = support::open(&setup, &FakeEncoder::new());
		let mut snapshot = Snapshot::default();
		wait_for(&project, &mut snapshot, "the scan", Snapshot::scan_complete);
		fs::rename(setup.source.join("A"), &moved).unwrap();
		rescan(&project, &mut snapshot);
	}
	assert_eq!(state_of(&setup, "A/a.txt"), FileState::SourceGone(copied("A/a.txt", 1)));
	assert_eq!(
		state_of(&setup, "A/B/b.txt"),
		FileState::SourceGone(copied("A/B/b.txt", 2))
	);

	{
		let project = support::open(&setup, &FakeEncoder::new());
		let mut snapshot = Snapshot::default();
		wait_for(&project, &mut snapshot, "the scan", Snapshot::scan_complete);
		fs::rename(&moved, setup.source.join("A")).unwrap();
		rescan(&project, &mut snapshot);
	}
	assert_eq!(state_of(&setup, "A/a.txt"), FileState::Done(copied("A/a.txt", 1)));
	assert_eq!(state_of(&setup, "A/B/b.txt"), FileState::Done(copied("A/B/b.txt", 2)));
}

#[test]
fn archived_files_whose_source_comes_back_are_picked_up() {
	let setup = setup(|_| {});
	write(&setup.source, "back.txt", b"back");
	write(&setup.source, "restored.txt", b"restored");
	write(&setup.source, "edited.txt", b"edited");
	seed(
		&setup,
		"back.txt",
		with_state(FileState::SourceGone(copied("back.txt", 4))),
	);
	seed(
		&setup,
		"restored.txt",
		with_state(FileState::OriginalDeleted(copied("restored.txt", 8))),
	);
	seed(
		&setup,
		"edited.txt",
		with_state(FileState::OriginalDeleted(copied("edited.txt", 6))),
	);
	write(&setup.source, "back.txt", b"back, changed");
	write(&setup.source, "edited.txt", b"edited again");

	open_and_scan(&setup);

	for name in ["back.txt", "edited.txt"] {
		let record = record_of(&setup, name);
		assert!(
			matches!(
				record.state,
				FileState::Discovered | FileState::Ready | FileState::Queued
			),
			"{name}"
		);
		assert_eq!(record.replaces, None, "{name}");
	}
	assert_eq!(
		state_of(&setup, "restored.txt"),
		FileState::Done(copied("restored.txt", 8))
	);
}

#[test]
fn a_pending_delete_finishes_only_when_the_source_is_gone() {
	let setup = setup(|_| {});
	write(&setup.source, "present.txt", b"present");
	write(&setup.source, "gone.txt", b"gone");
	seed(
		&setup,
		"present.txt",
		with_state(FileState::DeletePending(copied("present.txt", 7))),
	);
	seed(
		&setup,
		"gone.txt",
		with_state(FileState::DeletePending(copied("gone.txt", 4))),
	);
	fs::remove_file(setup.source.join("gone.txt")).unwrap();

	open_and_scan(&setup);

	assert_eq!(
		state_of(&setup, "present.txt"),
		FileState::DeletePending(copied("present.txt", 7))
	);
	assert_eq!(
		state_of(&setup, "gone.txt"),
		FileState::OriginalDeleted(copied("gone.txt", 4))
	);
}
