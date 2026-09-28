mod support;

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cryarchiver_engine::preset::{Overrides, VmafOverride};
use cryarchiver_project::issues::Verb;
use cryarchiver_project::keys::RelPath;
use cryarchiver_project::record::{CopyReason, FileRecord, FileState, Hash, How, IssueKind, Output, mtime_ns};
use cryarchiver_project::settings::{SettingsChange, Speed};
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
	assert!(
		!source.join("archive").exists(),
		"a refused destination is never created in the source"
	);

	let deeper = cryarchiver_project::create(&source, &source.join("a").join("b"), OsStr::new("a"), |_| {});

	assert!(matches!(deeper, Err(ProjectError::Settings(_))));
	assert_eq!(fs::read_dir(&source).unwrap().count(), 0);
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
fn a_project_that_deletes_originals_archives_but_deletes_nothing_until_the_session_confirms_it() {
	let setup = setup(|settings| settings.delete_originals = true);
	let original = write(&setup.source, "notes.txt", b"notes");
	let project = support::open(&setup, &FakeEncoder::new());
	let mut snapshot = Snapshot::default();

	project.send(Command::Start {
		delete_confirmed: false,
	});
	wait_for(&project, &mut snapshot, "the archive", Snapshot::finished);
	assert!(original.exists());
	assert!(setup.destination.join("notes.txt").exists());
	assert!(snapshot.status.as_ref().unwrap().deleting_paused);

	project.send(Command::Start { delete_confirmed: true });
	wait_for(&project, &mut snapshot, "the delete", |s| {
		s.overall.as_ref().is_some_and(|overall| overall.originals_deleted == 1)
	});
	assert!(!original.exists());
	assert!(!snapshot.status.as_ref().unwrap().deleting_paused);
}

#[test]
fn a_project_file_copied_away_from_its_archive_is_refused() {
	let setup = setup(|_| {});
	let elsewhere = setup.dir.path().join("elsewhere");
	fs::create_dir_all(&elsewhere).unwrap();
	let copy = elsewhere.join("archive.cryarchive");
	fs::copy(&setup.project, &copy).unwrap();

	let refused = Project::open(
		&copy,
		ProjectOptions {
			backend: Arc::new(FakeBackend {
				encoder: FakeEncoder::new(),
			}),
			tuning: support::tuning(),
			fault: None,
		},
	);

	let Err(err @ ProjectError::Moved { .. }) = refused else {
		panic!("the copy opened");
	};
	let (_, meta, _) = Store::open(&setup.project).unwrap();
	assert!(
		err.to_string()
			.contains(&cryarchiver_engine::text::display_path(&meta.settings.destination)),
		"{err}"
	);

	assert_eq!(
		fs::read_dir(&elsewhere).unwrap().count(),
		1,
		"nothing written beside the copy"
	);
	drop(support::open(&setup, &FakeEncoder::new()));
}

#[test]
fn a_missing_source_root_leaves_the_run_unclean() {
	let setup = setup(|_| {});
	write(&setup.source, "a.txt", b"a");
	{
		let project = support::open(&setup, &FakeEncoder::new());
		let mut snapshot = Snapshot::default();
		project.send(Command::Start {
			delete_confirmed: false,
		});
		wait_for(&project, &mut snapshot, "the archive", Snapshot::finished);
	}
	fs::rename(&setup.source, setup.dir.path().join("unplugged")).unwrap();

	let project = support::open(&setup, &FakeEncoder::new());
	let mut snapshot = Snapshot::default();
	project.send(Command::Start {
		delete_confirmed: false,
	});
	wait_for(&project, &mut snapshot, "the completion check", Snapshot::finished);
	drop(project);

	assert_eq!(snapshot.issues.len(), 1, "{:?}", snapshot.issues);
	assert_eq!(snapshot.issues[0].location, "the source folder");
	assert!(snapshot.overall.as_ref().unwrap().progress < 1.0);
	assert!(
		snapshot
			.notices
			.iter()
			.any(|notice| notice.text == "1 source folder can't be read, so the archive isn't complete")
	);
	assert!(matches!(records(&setup)[0].1.state, FileState::Done(_)));
}

#[test]
fn creating_a_project_over_a_missing_source_names_it() {
	let dir = tempfile::tempdir().unwrap();
	let missing = dir.path().join("missing");

	let err = cryarchiver_project::create(
		&missing,
		&dir.path().join("archive").join("deep"),
		OsStr::new("a"),
		|_| {},
	)
	.unwrap_err();

	assert!(
		err.to_string()
			.contains(&cryarchiver_engine::text::display_path(&missing)),
		"{err}"
	);
	assert!(!dir.path().join("archive").exists());
}

#[test]
fn creating_over_a_source_that_is_a_file_creates_nothing() {
	let dir = tempfile::tempdir().unwrap();
	let file = dir.path().join("photos.zip");
	fs::write(&file, b"zip").unwrap();

	let err = cryarchiver_project::create(&file, &dir.path().join("archive"), OsStr::new("a"), |_| {}).unwrap_err();

	assert!(err.to_string().contains("not a folder"), "{err}");
	assert!(!dir.path().join("archive").exists());
}

#[cfg(windows)]
#[test]
fn a_case_variant_destination_of_a_missing_source_does_not_create_the_source() {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("new").join("a");
	let destination = dir.path().join("NEW").join("a").join("b");

	let created = cryarchiver_project::create(&source, &destination, OsStr::new("a"), |_| {});

	assert!(created.is_err());
	assert!(!dir.path().join("new").exists());
}

#[test]
fn dot_dot_in_a_destination_cannot_reach_into_the_source() {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("source");
	fs::create_dir(&source).unwrap();

	for destination in [
		source.join("new").join("..").join("x"),
		source.join("new").join("deep").join(".."),
		dir.path().join("missing").join("..").join("source").join("sub"),
	] {
		let created = cryarchiver_project::create(&source, &destination, OsStr::new("a"), |_| {});

		assert!(
			matches!(created, Err(ProjectError::Settings(_))),
			"{}: {created:?}",
			destination.display()
		);
	}
	assert_eq!(fs::read_dir(&source).unwrap().count(), 0);
	assert!(!dir.path().join("missing").exists());
}

fn assert_only_the_planned_folder_is_created(destination: &Path, source: &Path, archive: &Path) {
	let created = cryarchiver_project::create(source, destination, OsStr::new("a"), |_| {});

	assert!(created.is_ok(), "{}: {created:?}", destination.display());
	assert_eq!(fs::read_dir(source).unwrap().count(), 0, "{}", destination.display());
	assert!(archive.join("a.cryarchive").is_file());
}

#[test]
fn a_destination_that_climbs_out_of_the_source_creates_nothing_in_it() {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("source");
	fs::create_dir(&source).unwrap();
	let destination = source.join("new").join("..").join("..").join("archive");

	assert_only_the_planned_folder_is_created(&destination, &source, &dir.path().join("archive"));
}

#[cfg(windows)]
#[test]
fn a_verbatim_destination_that_climbs_out_of_the_source_creates_nothing_in_it() {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("source");
	fs::create_dir(&source).unwrap();
	let verbatim = fs::canonicalize(&source).unwrap();
	assert!(verbatim.to_string_lossy().starts_with(r"\\?\"));
	let destination = PathBuf::from(format!(r"{}\new\..\..\archive", verbatim.display()));

	assert_only_the_planned_folder_is_created(&destination, &source, &dir.path().join("archive"));
}

#[cfg(unix)]
#[test]
fn a_typed_unix_destination_that_climbs_out_of_the_source_creates_nothing_in_it() {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("source");
	fs::create_dir(&source).unwrap();
	let destination = PathBuf::from(format!("{}/source/new/deeper/../../../archive", dir.path().display()));

	assert_only_the_planned_folder_is_created(&destination, &source, &dir.path().join("archive"));
}

#[cfg(unix)]
#[test]
fn a_link_reached_after_dot_dot_cannot_lead_into_the_source() {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("source");
	fs::create_dir_all(source.join("sub")).unwrap();
	fs::create_dir(dir.path().join("d")).unwrap();
	std::os::unix::fs::symlink(source.join("sub"), dir.path().join("d").join("lnk")).unwrap();
	let destination = PathBuf::from(format!("{}/d/missing/../lnk/archive", dir.path().display()));

	let created = cryarchiver_project::create(&source, &destination, OsStr::new("a"), |_| {});

	assert!(matches!(created, Err(ProjectError::Settings(_))), "{created:?}");
	assert_eq!(fs::read_dir(source.join("sub")).unwrap().count(), 0);
	assert!(!dir.path().join("d").join("missing").exists());
}

#[cfg(windows)]
#[test]
fn a_destination_through_a_junction_into_the_source_is_refused() {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("source");
	fs::create_dir(&source).unwrap();
	let alias = dir.path().join("alias");
	let made = std::process::Command::new("cmd")
		.args(["/C", "mklink", "/J"])
		.arg(&alias)
		.arg(&source)
		.stdout(std::process::Stdio::null())
		.status();
	if !made.is_ok_and(|status| status.success()) {
		eprintln!("skipped: a junction can't be created here");
		return;
	}

	for destination in [alias.join("archive"), alias.clone()] {
		let created = cryarchiver_project::create(&source, &destination, OsStr::new("a"), |_| {});

		assert!(matches!(created, Err(ProjectError::Settings(_))), "{created:?}");
	}
	assert_eq!(fs::read_dir(&source).unwrap().count(), 0);
	fs::remove_dir(&alias).unwrap();
}

#[cfg(unix)]
#[test]
fn a_destination_through_a_link_into_the_source_is_refused() {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("source");
	fs::create_dir(&source).unwrap();
	let alias = dir.path().join("alias");
	std::os::unix::fs::symlink(&source, &alias).unwrap();

	for destination in [alias.join("archive"), alias.clone()] {
		let created = cryarchiver_project::create(&source, &destination, OsStr::new("a"), |_| {});

		assert!(matches!(created, Err(ProjectError::Settings(_))), "{created:?}");
	}
	assert_eq!(fs::read_dir(&source).unwrap().count(), 0);
}

struct NameSetup {
	dir: tempfile::TempDir,
	source: PathBuf,
	destination: PathBuf,
}

impl NameSetup {
	fn new() -> Self {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("src");
		fs::create_dir_all(source.join("sub")).unwrap();
		fs::create_dir(dir.path().join("d")).unwrap();
		Self {
			destination: dir.path().join("d").join("archive"),
			source,
			dir,
		}
	}

	/// The name is refused before anything is created, and the source keeps only its `sub` folder, empty.
	fn assert_refused(&self, name: &str) {
		let created = cryarchiver_project::create(&self.source, &self.destination, OsStr::new(name), |_| {});

		assert!(matches!(created, Err(ProjectError::Name { .. })), "{name}: {created:?}");
		assert!(!self.destination.exists(), "{name}");
		let entries: Vec<_> = fs::read_dir(&self.source)
			.unwrap()
			.map(|entry| entry.unwrap().file_name())
			.collect();
		assert_eq!(entries, ["sub"], "{name}");
		assert_eq!(fs::read_dir(self.source.join("sub")).unwrap().count(), 0, "{name}");
	}
}

#[test]
fn a_name_that_climbs_or_is_absolute_is_refused() {
	let setup = NameSetup::new();

	setup.assert_refused("../../src/fwd");
	setup.assert_refused(&setup.source.join("abs").display().to_string());
	setup.assert_refused("..");
	setup.assert_refused(".");
	setup.assert_refused("");
}

#[cfg(windows)]
#[test]
fn windows_names_that_leave_the_destination_are_refused() {
	let setup = NameSetup::new();
	let _ = std::process::Command::new("cmd")
		.args(["/C", "mklink", "/J"])
		.arg(setup.dir.path().join("d").join("j"))
		.arg(setup.source.join("sub"))
		.stdout(std::process::Stdio::null())
		.status();

	setup.assert_refused(r"..\..\src\evil");
	setup.assert_refused(r"..\j\evil");
	setup.assert_refused("C:rel");
	setup.assert_refused(&format!(r"\\?\{}\abs", setup.source.display()));
}

#[cfg(unix)]
#[test]
fn a_unix_name_through_a_link_is_refused() {
	let setup = NameSetup::new();
	std::os::unix::fs::symlink(setup.source.join("sub"), setup.dir.path().join("d").join("lnk")).unwrap();

	setup.assert_refused("../lnk/evil");
	setup.assert_refused("../../src/evil");
}

#[test]
fn a_dangling_link_where_the_project_file_goes_is_refused_and_nothing_appears_where_it_leads() {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("src");
	let destination = dir.path().join("d").join("archive");
	fs::create_dir(&source).unwrap();
	fs::create_dir_all(&destination).unwrap();
	let target = source.join("linked.cryarchive");
	let link = destination.join("evil.cryarchive");
	if !support::link_file(&link, &target) {
		eprintln!("skipped: no file link can be made here");
		return;
	}

	let created = cryarchiver_project::create(&source, &destination, OsStr::new("evil"), |_| {});

	assert!(
		matches!(created, Err(ProjectError::Store(StoreError::Exists(_)))),
		"{created:?}"
	);
	assert!(fs::symlink_metadata(&target).is_err());
	assert!(support::names_in(&source).is_empty());
	assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
}

fn try_open(project: &Path) -> Result<Project, ProjectError> {
	Project::open(
		project,
		ProjectOptions {
			backend: Arc::new(FakeBackend {
				encoder: FakeEncoder::new(),
			}),
			tuning: support::tuning(),
			fault: None,
		},
	)
}

/// What a sweep of the source would find to remove: folders without a `job.lock`, at the top and in `work`.
fn fill_as_a_work_area(source: &Path) -> Vec<String> {
	write(source, "victim/keep.txt", b"keep");
	write(source, "victim2/data.txt", b"data");
	write(source, "work/victim3/more.txt", b"more");
	write(source, "top.txt", b"top");
	support::names_in(source)
}

fn remove_link(link: &Path) {
	fs::remove_dir(link).or_else(|_| fs::remove_file(link)).unwrap();
}

#[test]
fn a_project_whose_work_area_is_a_link_is_refused_and_nothing_in_the_source_is_removed() {
	for linked in [
		&[".cryarchiver"][..],
		&[".cryarchiver", "work"],
		&[".cryarchiver", "work", "staged"],
	] {
		let setup = setup(|_| {});
		let before = fill_as_a_work_area(&setup.source);
		let link = linked
			.iter()
			.fold(setup.destination.clone(), |path, name| path.join(name));
		fs::create_dir_all(link.parent().unwrap()).unwrap();
		if !link_folder(&link, &setup.source) {
			eprintln!("skipped: no folder link can be made here");
			return;
		}

		let refused = try_open(&setup.project);

		let Err(err @ ProjectError::Link { .. }) = refused else {
			panic!("{linked:?}: the project opened");
		};
		assert!(err.to_string().contains(&support::shown(&link)), "{err}");
		assert_eq!(support::names_in(&setup.source), before, "{linked:?}");
		assert_eq!(fs::read(setup.source.join("victim").join("keep.txt")).unwrap(), b"keep");
		assert_eq!(
			fs::read(setup.source.join("work").join("victim3").join("more.txt")).unwrap(),
			b"more"
		);

		remove_link(&link);
		assert!(try_open(&setup.project).is_ok(), "{linked:?}");
		assert_eq!(support::names_in(&setup.source), before, "{linked:?}");
	}
}

#[test]
fn a_project_file_that_is_a_link_is_refused() {
	let setup = setup(|_| {});
	let elsewhere = setup.dir.path().join("elsewhere");
	fs::create_dir(&elsewhere).unwrap();
	let real = elsewhere.join("archive.cryarchive");
	fs::rename(&setup.project, &real).unwrap();
	if !support::link_file(&setup.project, &real) {
		eprintln!("skipped: no file link can be made here");
		return;
	}

	let refused = try_open(&setup.project);

	let Err(err @ ProjectError::Link { .. }) = refused else {
		panic!("the linked project file opened");
	};
	assert!(
		err.to_string()
			.contains(&cryarchiver_engine::text::display_path(&setup.project)),
		"{err}"
	);
}

#[test]
fn the_project_file_is_named_before_the_destination_exists() {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("source");
	fs::create_dir(&source).unwrap();
	let destination = dir.path().join("new").join(".").join("archive");

	let planned = cryarchiver_project::project_file(&destination, OsStr::new("a")).unwrap();
	let created = cryarchiver_project::create(&source, &destination, OsStr::new("a"), |_| {}).unwrap();

	assert_eq!(planned, created);
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

fn start(project: &Project) {
	project.send(Command::Start {
		delete_confirmed: false,
	});
}

#[test]
fn an_acknowledged_unreadable_folder_stays_quiet_across_reopens_until_it_can_be_read() {
	let setup = setup(|_| {});
	write(&setup.source, "a.txt", b"a");
	write(&setup.source, "Private/b.txt", b"b");
	let Some(denied) = support::Unlistable::deny(&setup.source.join("Private")) else {
		eprintln!("skipped: this user may list any folder");
		return;
	};

	{
		let project = support::open(&setup, &FakeEncoder::new());
		let mut snapshot = Snapshot::default();
		start(&project);
		wait_for(&project, &mut snapshot, "the issue", |s| {
			s.finished() && s.issues.iter().any(|issue| issue.location == "Private")
		});
		let issue = snapshot
			.issues
			.iter()
			.find(|issue| issue.location == "Private")
			.unwrap();
		assert_eq!(issue.verbs.accept, Some("Acknowledge"));

		project.send(Command::Resolve(issue.id, Verb::Accept));
		wait_for(&project, &mut snapshot, "a clean finish", |s| {
			s.finished()
				&& s.issues.is_empty()
				&& s.overall.as_ref().is_some_and(|overall| {
					overall.open_issues == 0
						&& overall.progress >= 1.0
						&& overall
							.parity
							.is_some_and(|parity| parity.source_files == 1 && parity.archive_files == 1)
				})
		});
	}

	{
		let project = support::open(&setup, &FakeEncoder::new());
		let mut snapshot = Snapshot::default();
		start(&project);
		wait_for(&project, &mut snapshot, "the finish", |s| {
			s.finished()
				&& s.logs
					.iter()
					.any(|entry| entry.message.starts_with("Private can't be read and is left out"))
		});
		support::drain(&project, &mut snapshot);
		assert!(snapshot.issues.is_empty(), "{:?}", snapshot.issues);
		assert_eq!(snapshot.overall.as_ref().unwrap().open_issues, 0);
	}
	assert!(!setup.destination.join("Private").join("b.txt").exists());

	drop(denied);
	let project = support::open(&setup, &FakeEncoder::new());
	let mut snapshot = Snapshot::default();
	start(&project);
	wait_for(&project, &mut snapshot, "the folder archived", |s| {
		s.finished() && s.overall.as_ref().is_some_and(|overall| overall.files_done == 2)
	});
	drop(project);

	assert_eq!(fs::read(setup.destination.join("Private").join("b.txt")).unwrap(), b"b");
	let (_, _, loaded) = Store::open(&setup.project).unwrap();
	let (_, private) = loaded
		.folders
		.iter()
		.find(|(key, _)| key.display() == "Private")
		.unwrap();
	assert!(!private.unreadable_acknowledged);
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
	write(&setup.destination, "A/a.txt", b"a");
	write(&setup.destination, "A/B/b.txt", b"bb");
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
	write(&setup.destination, "restored.txt", b"restored");
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
	write(&setup.destination, "present.txt", b"present");
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
		FileState::Done(copied("present.txt", 7))
	);
	assert_eq!(
		state_of(&setup, "gone.txt"),
		FileState::OriginalDeleted(copied("gone.txt", 4))
	);
}

#[test]
fn quality_learned_in_one_run_is_there_the_next_time() {
	use cryarchiver_engine::metadata::ImageFormat;
	use cryarchiver_engine::quality::keys::{CacheKey, Group, ImageGroup};
	use cryarchiver_engine::quality::memory::Observation;

	let setup = setup(|_| {});
	let encoder = FakeEncoder::new();
	let key = CacheKey {
		group: Group::Image(ImageGroup::new(ImageFormat::Jpeg, false, false, false, 4000, 3000)),
		encoder: "ravif-0.13".into(),
		preset: 6,
		target: 85.0,
		floor: None,
	};
	{
		let project = support::open(&setup, &encoder);
		let quality = encoder
			.quality
			.lock()
			.unwrap()
			.clone()
			.expect("the encoder gets the project's memory");
		quality.record(
			&key,
			Observation {
				setting: 90.5,
				slope: None,
				bpp: None,
			},
		);
		let mut snapshot = Snapshot::default();
		wait_for(&project, &mut snapshot, "the scan", Snapshot::scan_complete);
	}

	let _project = support::open(&setup, &encoder);
	let quality = encoder.quality.lock().unwrap().clone().unwrap();

	assert_eq!(quality.lookup(&key).map(|stats| stats.setting), Some(90.5));
}

#[test]
fn a_forgotten_group_is_gone_the_next_time() {
	use cryarchiver_engine::metadata::ImageFormat;
	use cryarchiver_engine::quality::keys::{CacheKey, Group, ImageGroup};
	use cryarchiver_engine::quality::memory::Observation;

	let setup = setup(|_| {});
	let encoder = FakeEncoder::new();
	let key = CacheKey {
		group: Group::Image(ImageGroup::new(ImageFormat::Jpeg, false, false, false, 4000, 3000)),
		encoder: "ravif-0.13.0".into(),
		preset: 6,
		target: 85.0,
		floor: None,
	};
	let learned = Observation {
		setting: 90.5,
		slope: None,
		bpp: None,
	};
	for forget in [false, true] {
		let project = support::open(&setup, &encoder);
		let quality = encoder.quality.lock().unwrap().clone().unwrap();
		if forget {
			assert!(quality.lookup(&key).is_some(), "the first run saved the group");
			quality.forget(&key);
		} else {
			quality.record(&key, learned);
		}
		let mut snapshot = Snapshot::default();
		wait_for(&project, &mut snapshot, "the scan", Snapshot::scan_complete);
	}

	let _project = support::open(&setup, &encoder);
	let quality = encoder.quality.lock().unwrap().clone().unwrap();

	assert_eq!(quality.lookup(&key), None);
}

#[test]
fn a_project_file_from_before_quality_overrides_opens_without_them() {
	#[derive(serde::Serialize)]
	struct Previous {
		#[serde(with = "cryarchiver_project::keys::path_bytes")]
		source: PathBuf,
		#[serde(with = "cryarchiver_project::keys::path_bytes")]
		destination: PathBuf,
		speed: Speed,
		min_savings: u8,
		watch: bool,
		performance_limit: Option<u8>,
		stability_window: std::time::Duration,
		keep_auxiliary_images: bool,
		delete_originals: bool,
	}

	let setup = setup(|settings| settings.speed = Speed::Slow);
	let (_, meta, _) = Store::open(&setup.project).unwrap();
	let current = meta.settings;
	let previous = Previous {
		source: current.source.clone(),
		destination: current.destination.clone(),
		speed: current.speed,
		min_savings: current.min_savings,
		watch: current.watch,
		performance_limit: current.performance_limit,
		stability_window: current.stability_window,
		keep_auxiliary_images: current.keep_auxiliary_images,
		delete_originals: current.delete_originals,
	};
	{
		let db = redb::Database::open(&setup.project).unwrap();
		let txn = db.begin_write().unwrap();
		{
			let mut table = txn
				.open_table(redb::TableDefinition::<&str, &[u8]>::new("meta"))
				.unwrap();
			table
				.insert("settings", rmp_serde::to_vec_named(&previous).unwrap().as_slice())
				.unwrap();
		}
		txn.commit().unwrap();
	}

	let project = support::open(&setup, &FakeEncoder::new());

	assert!(project.settings().overrides.is_empty());
	assert_eq!(project.settings(), &current);
	assert_eq!(project.settings().speed, Speed::Slow);
}

#[test]
fn an_overrides_change_is_stored_and_read_back_on_reopen() {
	let setup = setup(|_| {});
	let encoder = FakeEncoder::new();
	let custom = Overrides {
		video: Some(VmafOverride {
			mean: 90.0,
			floor: 85.0,
		}),
		image: Some(80.0),
		audio_stereo_kbps: None,
	};
	{
		let project = support::open(&setup, &encoder);
		let mut snapshot = Snapshot::default();
		project.send(Command::ChangeSettings(SettingsChange::Overrides(Overrides {
			video: Some(VmafOverride {
				mean: 90.04,
				floor: 85.0,
			}),
			..custom
		})));
		wait_for(&project, &mut snapshot, "the new overrides", |s| {
			s.settings.as_ref().is_some_and(|settings| settings.overrides == custom)
		});
	}

	let project = support::open(&setup, &encoder);

	assert_eq!(project.settings().overrides, custom);
}

#[test]
fn a_new_project_stores_its_overrides_normalized() {
	let setup = setup(|settings| {
		settings.overrides = Overrides {
			video: Some(VmafOverride {
				mean: 90.04,
				floor: 85.06,
			}),
			image: Some(80.04),
			audio_stereo_kbps: Some(160),
		};
	});

	let (_, meta, _) = Store::open(&setup.project).unwrap();

	assert_eq!(
		meta.settings.overrides,
		Overrides {
			video: Some(VmafOverride {
				mean: 90.0,
				floor: 85.1,
			}),
			image: Some(80.0),
			audio_stereo_kbps: Some(160),
		}
	);
}

#[test]
fn a_new_project_with_an_invalid_override_is_refused_with_its_reason_and_nothing_is_created() {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("source");
	fs::create_dir(&source).unwrap();
	let destination = dir.path().join("new").join("archive");

	for (overrides, reason) in [
		(
			Overrides {
				image: Some(200.0),
				..Overrides::default()
			},
			"between 50 and 95",
		),
		(
			Overrides {
				audio_stereo_kbps: Some(99),
				..Overrides::default()
			},
			"in steps of 8",
		),
	] {
		let refused = cryarchiver_project::create(&source, &destination, OsStr::new("archive"), |settings| {
			settings.overrides = overrides;
		});

		let Err(err) = refused else {
			panic!("{overrides:?}: the project was made");
		};
		assert!(err.to_string().contains(reason), "{err}");
		assert!(!dir.path().join("new").exists(), "{overrides:?}");
		assert_eq!(support::names_in(dir.path()), ["source"]);
	}
}

#[test]
fn a_project_file_with_another_name_is_refused_and_never_written() {
	let setup = setup(|_| {});
	let other_name = setup.dir.path().join("other name.cryarchive");
	fs::hard_link(&setup.project, &other_name).unwrap();
	let before = fs::read(&setup.project).unwrap();

	let refused = try_open(&setup.project);

	let Err(err @ ProjectError::Store(StoreError::HardLinked(2))) = refused else {
		panic!("the hard-linked project file opened");
	};
	assert!(err.to_string().contains("hard link"), "{err}");
	assert_eq!(fs::read(&setup.project).unwrap(), before);
	assert_eq!(fs::read(&other_name).unwrap(), before);

	fs::remove_file(&other_name).unwrap();
	assert!(try_open(&setup.project).is_ok());
}

#[test]
fn a_refused_open_leaves_the_project_file_as_it_was() {
	let setup = setup(|_| {});
	drop(support::open(&setup, &FakeEncoder::new()));
	let elsewhere = setup.dir.path().join("elsewhere");
	fs::create_dir(&elsewhere).unwrap();
	let copy = elsewhere.join("archive.cryarchive");
	fs::copy(&setup.project, &copy).unwrap();
	let before = fs::read(&copy).unwrap();

	let refused = try_open(&copy);

	assert!(matches!(refused, Err(ProjectError::Moved { .. })), "the copy opened");
	assert!(fs::read(&copy).unwrap() == before, "the refused copy was written");
	assert_eq!(support::names_in(&elsewhere), ["archive.cryarchive"]);
}
