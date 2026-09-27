mod support;

use std::fs;

use cryarchiver_engine::outcome::ErrorKind;
use cryarchiver_project::issues::{Problem, Verb};
use cryarchiver_project::record::FileState;
use cryarchiver_project::settings::{DELETE_CONFIRMATION, SettingsChange};
use cryarchiver_project::store::Store;
use cryarchiver_project::view::NoticeLevel;
use cryarchiver_project::{Command, Fault, Project};
use support::{FakeEncoder, Rule, Snapshot, jpeg, setup, wait_for, write};

const BIG: usize = 100_000;

fn state(setup: &support::Setup, path: &str) -> FileState {
	let (_, _, loaded) = Store::open(&setup.project).unwrap();
	loaded
		.files
		.into_iter()
		.find(|(key, _)| key.display() == path)
		.map_or_else(|| panic!("no record for {path}"), |(_, record)| record.state)
}

fn deleted(snapshot: &Snapshot) -> u64 {
	snapshot.overall.as_ref().map_or(0, |overall| overall.originals_deleted)
}

fn start_deleting(project: &Project) {
	project.send(Command::Start { delete_confirmed: true });
}

fn archive_without_deleting(setup: &support::Setup, encoder: &std::sync::Arc<FakeEncoder>) {
	let project = support::open(setup, encoder);
	let mut snapshot = Snapshot::default();
	project.send(Command::Start {
		delete_confirmed: false,
	});
	wait_for(&project, &mut snapshot, "the archive", |s| {
		s.overall.as_ref().is_some_and(|overall| overall.parity.is_some())
	});
}

fn enable_deleting(project: &Project) {
	project.send(Command::ChangeSettings(SettingsChange::DeleteOriginals {
		enabled: true,
		confirmation: DELETE_CONFIRMATION.into(),
	}));
}

#[test]
fn originals_are_deleted_only_after_a_verified_archive() {
	let setup = setup(|settings| settings.delete_originals = true);
	let photo = write(&setup.source, "Photos/a.jpg", &jpeg(1, 500));
	let notes = write(&setup.source, "notes.txt", b"notes");
	let project = support::open(&setup, &FakeEncoder::new());
	let mut snapshot = Snapshot::default();

	start_deleting(&project);
	wait_for(&project, &mut snapshot, "both originals deleted", |s| deleted(s) == 2);
	drop(project);

	assert!(!photo.exists() && !notes.exists());
	assert!(setup.source.join("Photos").is_dir(), "folders are never deleted");
	assert_eq!(fs::read(setup.destination.join("notes.txt")).unwrap(), b"notes");
	assert!(setup.destination.join("Photos").join("a.avif").exists());
	assert!(matches!(state(&setup, "notes.txt"), FileState::OriginalDeleted(_)));
	assert!(snapshot.logs.iter().any(|log| log.message.starts_with("deleted ")));
}

#[test]
fn deletion_needs_the_typed_confirmation() {
	let setup = setup(|_| {});
	let project = support::open(&setup, &FakeEncoder::new());
	let mut snapshot = Snapshot::default();

	project.send(Command::ChangeSettings(SettingsChange::DeleteOriginals {
		enabled: true,
		confirmation: "yes".into(),
	}));

	wait_for(&project, &mut snapshot, "the refusal", |s| {
		s.notices.iter().any(|notice| notice.level == NoticeLevel::Warning)
	});
	assert!(
		snapshot
			.settings
			.as_ref()
			.is_none_or(|settings| !settings.delete_originals)
	);
}

#[test]
fn turning_deletion_on_mid_project_covers_files_archived_earlier() {
	let setup = setup(|_| {});
	let notes = write(&setup.source, "notes.txt", b"notes");
	let encoder = FakeEncoder::new();
	archive_without_deleting(&setup, &encoder);
	assert!(notes.exists());

	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();
	project.send(Command::Start {
		delete_confirmed: false,
	});
	enable_deleting(&project);
	wait_for(&project, &mut snapshot, "the delete", |s| deleted(s) == 1);

	assert!(!notes.exists());
}

#[test]
fn an_archived_file_that_changed_keeps_its_original_and_becomes_an_issue() {
	let setup = setup(|_| {});
	let notes = write(&setup.source, "notes.txt", b"notes");
	let encoder = FakeEncoder::new();
	archive_without_deleting(&setup, &encoder);
	fs::write(setup.destination.join("notes.txt"), b"NOTES").unwrap();

	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();
	wait_for(&project, &mut snapshot, "the scan", Snapshot::scan_complete);
	project.send(Command::Start {
		delete_confirmed: false,
	});
	enable_deleting(&project);
	wait_for(&project, &mut snapshot, "the issue", |s| {
		s.issues.iter().any(|issue| issue.problem == Problem::OutputMissing)
	});

	assert!(notes.exists());
	assert_eq!(deleted(&snapshot), 0);
}

#[test]
fn files_with_issues_or_excluded_are_never_deleted() {
	let setup = setup(|settings| settings.delete_originals = true);
	let broken = write(&setup.source, "broken.jpg", &jpeg(1, 100));
	write(&setup.source, "fine.txt", b"fine");
	let encoder = FakeEncoder::new();
	encoder.rule("broken.jpg", Rule::Fail(ErrorKind::SourceDamaged("truncated".into())));
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();

	start_deleting(&project);
	wait_for(&project, &mut snapshot, "the issue and one delete", |s| {
		!s.issues.is_empty() && deleted(s) == 1
	});
	assert!(broken.exists());

	project.send(Command::Resolve(snapshot.issues[0].id, Verb::Exclude));
	wait_for(&project, &mut snapshot, "the exclusion", |s| s.issues.is_empty());
	std::thread::sleep(std::time::Duration::from_millis(200));
	assert!(broken.exists());
}

#[test]
fn a_duplicate_is_deleted_once_its_placeholder_and_the_canonical_copy_check_out() {
	let setup = setup(|_| {});
	let canonical = write(&setup.source, "a.jpg", &jpeg(4, BIG));
	let duplicate = write(&setup.source, "Copies/a.jpg", &jpeg(4, BIG));
	let encoder = FakeEncoder::new();
	archive_without_deleting(&setup, &encoder);
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();

	project.send(Command::Start {
		delete_confirmed: false,
	});
	enable_deleting(&project);
	wait_for(&project, &mut snapshot, "both deleted", |s| deleted(s) == 2);

	assert!(!canonical.exists() && !duplicate.exists());
	assert!(setup.destination.join("Copies").join("placeholder_a.jpg.txt").exists());
}

#[test]
fn a_duplicate_whose_canonical_copy_no_longer_checks_out_keeps_its_original() {
	let setup = setup(|_| {});
	let canonical = write(&setup.source, "a.jpg", &jpeg(4, BIG));
	let duplicate = write(&setup.source, "Copies/a.jpg", &jpeg(4, BIG));
	let encoder = FakeEncoder::new();
	archive_without_deleting(&setup, &encoder);
	let archived = setup.destination.join("a.avif");
	let damaged: Vec<u8> = fs::read(&archived).unwrap().iter().map(|byte| byte ^ 1).collect();
	fs::write(&archived, damaged).unwrap();

	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();
	wait_for(&project, &mut snapshot, "the scan", Snapshot::scan_complete);
	project.send(Command::Start {
		delete_confirmed: false,
	});
	enable_deleting(&project);
	wait_for(&project, &mut snapshot, "the canonical copy's issue", |s| {
		s.issues.iter().any(|issue| issue.problem == Problem::OutputMissing)
	});
	std::thread::sleep(std::time::Duration::from_millis(200));
	support::drain(&project, &mut snapshot);

	assert!(canonical.exists() && duplicate.exists());
	assert_eq!(deleted(&snapshot), 0);
}

#[test]
fn an_original_replaced_during_its_encode_is_archived_again_before_it_is_deleted() {
	let setup = setup(|settings| settings.delete_originals = true);
	let photo = write(&setup.source, "a.jpg", &jpeg(1, 500));
	let encoder = FakeEncoder::new();
	encoder.rule(
		"a.jpg",
		Rule::Replace {
			path: photo.clone(),
			content: jpeg(2, 500),
		},
	);
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();

	start_deleting(&project);
	wait_for(&project, &mut snapshot, "the delete", |s| deleted(s) == 1);
	drop(project);

	assert_eq!(encoder.calls(), ["a.jpg", "a.jpg"], "the replacement was archived too");
	assert!(!photo.exists());
}

#[test]
fn a_duplicate_replaced_between_its_hash_and_its_placeholder_is_archived_again_before_it_is_deleted() {
	let setup = setup(|settings| settings.delete_originals = true);
	write(&setup.source, "a.jpg", &jpeg(4, BIG));
	let duplicate = write(&setup.source, "Copies/b.jpg", &jpeg(4, BIG));
	let encoder = FakeEncoder::new();
	encoder.rule(
		"a.jpg",
		Rule::Replace {
			path: duplicate.clone(),
			content: jpeg(5, BIG),
		},
	);
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();

	start_deleting(&project);
	wait_for(&project, &mut snapshot, "both deleted", |s| deleted(s) == 2);
	drop(project);

	assert_eq!(
		encoder.calls(),
		["a.jpg", "b.jpg"],
		"the replacement was archived on its own"
	);
	assert!(!duplicate.exists());
	assert!(setup.destination.join("Copies").join("b.avif").exists());
	assert!(!setup.destination.join("Copies").join("placeholder_b.jpg.txt").exists());
}

#[test]
fn a_file_reached_through_a_link_out_of_the_source_is_kept() {
	let setup = setup(|settings| settings.delete_originals = true);
	let outside = setup.dir.path().join("outside");
	fs::create_dir_all(&outside).unwrap();
	fs::write(outside.join("kept.txt"), b"not ours").unwrap();
	let link = setup.source.join("linked");
	#[cfg(unix)]
	let linked = std::os::unix::fs::symlink(&outside, &link).is_ok();
	#[cfg(windows)]
	let linked = std::process::Command::new("cmd")
		.arg("/C")
		.arg("mklink")
		.arg("/J")
		.arg(&link)
		.arg(&outside)
		.output()
		.is_ok_and(|out| out.status.success());
	if !linked {
		eprintln!("skipped: can't create a folder link here");
		return;
	}
	let project = support::open(&setup, &FakeEncoder::new());
	let mut snapshot = Snapshot::default();

	start_deleting(&project);
	wait_for(&project, &mut snapshot, "the refusal", |s| {
		s.logs.iter().any(|log| log.message.contains("original kept"))
	});

	assert_eq!(fs::read(outside.join("kept.txt")).unwrap(), b"not ours");
	assert_eq!(
		fs::read(setup.destination.join("linked").join("kept.txt")).unwrap(),
		b"not ours"
	);
}

#[test]
fn a_crash_after_the_delete_intent_keeps_the_original_and_checks_it_again() {
	let setup = setup(|settings| settings.delete_originals = true);
	let notes = write(&setup.source, "notes.txt", b"notes");
	let encoder = FakeEncoder::new();
	let project = support::open_with(&setup, &encoder, Some(Fault::AfterDeleteIntent));
	let mut snapshot = Snapshot::default();

	start_deleting(&project);
	wait_for(&project, &mut snapshot, "the crash", |s| s.stopped);
	drop(project);
	assert!(notes.exists());
	assert!(matches!(state(&setup, "notes.txt"), FileState::DeletePending(_)));

	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();
	wait_for(&project, &mut snapshot, "the scan", Snapshot::scan_complete);
	assert!(notes.exists());
	start_deleting(&project);
	wait_for(&project, &mut snapshot, "the delete", |s| deleted(s) == 1);
	drop(project);

	assert!(!notes.exists());
	assert!(matches!(state(&setup, "notes.txt"), FileState::OriginalDeleted(_)));
}

#[test]
fn an_archived_original_replaced_before_it_joins_a_size_group_is_archived_again_before_it_is_deleted() {
	let setup = setup(|_| {});
	let photo = write(&setup.source, "a.jpg", &jpeg(1, BIG));
	let encoder = FakeEncoder::new();
	archive_without_deleting(&setup, &encoder);

	let modified = fs::metadata(&photo).unwrap().modified().unwrap();
	let replacement = setup.source.parent().unwrap().join("replacement.jpg");
	fs::write(&replacement, jpeg(2, BIG)).unwrap();
	fs::rename(&replacement, &photo).unwrap();
	fs::File::options()
		.write(true)
		.open(&photo)
		.unwrap()
		.set_modified(modified)
		.unwrap();
	write(&setup.source, "Copies/c.jpg", &jpeg(3, BIG));

	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();
	enable_deleting(&project);
	start_deleting(&project);
	wait_for(&project, &mut snapshot, "both deleted", |s| deleted(s) == 2);
	drop(project);

	assert_eq!(
		encoder.calls().iter().filter(|call| call.as_str() == "a.jpg").count(),
		2,
		"the replacement was archived before its original was deleted"
	);
}
