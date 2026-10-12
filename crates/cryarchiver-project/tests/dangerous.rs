mod support;

use std::fs;
use std::time::{Duration, SystemTime};

use cryarchiver_engine::outcome::ErrorKind;
use cryarchiver_project::issues::{Problem, Verb};
use cryarchiver_project::record::{CopyReason, FileRecord, FileState, How, mtime_ns};
use cryarchiver_project::settings::{DELETE_CONFIRMATION, SettingsChange};
use cryarchiver_project::store::Store;
use cryarchiver_project::tree::FolderId;
use cryarchiver_project::view::{NoticeLevel, RowState};
use cryarchiver_project::{Command, Fault, Project, Target};
use support::{FakeEncoder, Rule, Snapshot, file_id, jpeg, setup, wait_for, wait_for_state, write};

const BIG: usize = 100_000;

fn record(setup: &support::Setup, path: &str) -> FileRecord {
	let (_, _, loaded) = Store::open(&setup.project).unwrap();
	loaded
		.files
		.into_iter()
		.find(|(key, _)| key.display() == path)
		.map_or_else(|| panic!("no record for {path}"), |(_, record)| record)
}

fn state(setup: &support::Setup, path: &str) -> FileState {
	record(setup, path).state
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
fn a_kept_file_retried_before_deleting_is_confirmed_is_deleted_like_any_archived_file() {
	let setup = setup(|settings| settings.delete_originals = true);
	let retried = write(&setup.source, "a.jpg", &jpeg(1, 500));
	let left = write(&setup.source, "b.jpg", &jpeg(2, 500));
	let encoder = FakeEncoder::new();
	encoder.rule(".jpg", Rule::Keep);
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();
	project.send(Command::Start {
		delete_confirmed: false,
	});
	wait_for(&project, &mut snapshot, "the archive", |s| {
		s.overall.as_ref().is_some_and(|overall| overall.parity.is_some())
	});
	let file = file_id(&project, &mut snapshot, FolderId::ROOT, "a.jpg");

	encoder.rule("a.jpg", Rule::Encode("avif"));
	project.send(Command::RetryKept(Target::File(file)));
	wait_for_state(&project, &mut snapshot, file, RowState::Done);
	assert!(
		retried.exists() && left.exists(),
		"nothing is deleted before it is confirmed"
	);

	start_deleting(&project);
	wait_for(&project, &mut snapshot, "both originals deleted", |s| deleted(s) == 2);
	project.send(Command::RetryKept(Target::Folder(FolderId::ROOT)));
	wait_for(&project, &mut snapshot, "the notice", |s| {
		s.notices
			.iter()
			.any(|notice| notice.text == "nothing kept to retry here")
	});
	drop(project);

	assert!(!retried.exists() && !left.exists());
	assert!(!setup.destination.join("a.jpg").exists());
	assert_eq!(fs::read(setup.destination.join("a.avif")).unwrap(), b"encoded a.jpg");
	assert_eq!(fs::read(setup.destination.join("b.jpg")).unwrap(), jpeg(2, 500));
	assert!(matches!(state(&setup, "a.jpg"), FileState::OriginalDeleted(output) if output.how == How::Encoded));
	assert!(matches!(
		state(&setup, "b.jpg"),
		FileState::OriginalDeleted(output) if output.how == How::Copied(CopyReason::KeptOriginal)
	));
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
	wait_for(&project, &mut snapshot, "the exclusion and the finish", |s| {
		s.issues.is_empty() && s.finished()
	});
	support::drain(&project, &mut snapshot);
	assert!(broken.exists());
	assert_eq!(deleted(&snapshot), 1);
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
	wait_for(
		&project,
		&mut snapshot,
		"the canonical copy's issue and the finish",
		|s| s.issues.iter().any(|issue| issue.problem == Problem::OutputMissing) && s.finished(),
	);
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

/// In both modes: without deleting, the duplicate would silently point at content that isn't its own; deleting, its
/// original would go with its content archived nowhere.
#[test]
fn a_canonical_copy_edited_in_place_before_its_encode_never_lets_its_duplicate_go_unarchived() {
	for deleting in [false, true] {
		let setup = setup(|settings| settings.delete_originals = deleting);
		let canonical = write(&setup.source, "a.jpg", &jpeg(4, BIG));
		let duplicate = write(&setup.source, "Copies/b.jpg", &jpeg(4, BIG));
		let retagged = jpeg(5, BIG + 10);
		let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_234_567_891);
		let encoder = FakeEncoder::new();
		encoder.rule(
			"a.jpg",
			Rule::Rewrite {
				path: canonical.clone(),
				content: retagged.clone(),
				modified,
			},
		);
		let project = support::open_unwatched(&setup, &encoder);
		let mut snapshot = Snapshot::default();

		project.send(Command::Start {
			delete_confirmed: deleting,
		});
		if deleting {
			wait_for(&project, &mut snapshot, "both deleted", |s| deleted(s) == 2);
		} else {
			wait_for(&project, &mut snapshot, "the archive", |s| {
				s.overall.as_ref().is_some_and(|overall| overall.parity.is_some())
			});
		}
		drop(project);

		let calls = encoder.calls();
		assert!(
			calls.iter().any(|call| call == "b.jpg"),
			"archived from its own source: {calls:?}"
		);
		assert_eq!(duplicate.exists(), !deleting);
		assert_eq!(
			fs::read(setup.destination.join("Copies").join("b.avif")).unwrap(),
			b"encoded b.jpg"
		);
		assert!(!setup.destination.join("Copies").join("placeholder_b.jpg.txt").exists());

		let record = record(&setup, "a.jpg");
		assert_eq!(
			(record.size, record.mtime_ns),
			(retagged.len() as u64, mtime_ns(modified)),
			"the retagged content is what the record describes"
		);
		assert_eq!(
			matches!(record.state, FileState::OriginalDeleted(_)),
			deleting,
			"{:?}",
			record.state
		);
	}
}

fn run_until_done(project: &Project, snapshot: &mut Snapshot, deleting: bool, originals: u64) {
	project.send(Command::Start {
		delete_confirmed: deleting,
	});
	wait_for(project, snapshot, "the run", |s| {
		s.overall.as_ref().is_some_and(|overall| overall.parity.is_some()) && (!deleting || deleted(s) == originals)
	});
}

/// The source may hold another project's work area, left behind by a crash or still in use: it is archived like any
/// folder, and never mistaken for this project's own.
#[test]
fn a_source_folder_named_like_the_work_area_is_archived_beside_it_and_survives_reopening() {
	for deleting in [false, true] {
		let setup = setup(|settings| settings.delete_originals = deleting);
		let staged = write(&setup.source, ".cryarchiver/work/staged/a.txt", b"staged-looking");
		let workspace = write(&setup.source, ".cryarchiver/work/x/b.txt", b"workspace-looking");
		let encoder = FakeEncoder::new();
		let project = support::open(&setup, &encoder);
		let mut snapshot = Snapshot::default();
		run_until_done(&project, &mut snapshot, deleting, 2);
		drop(project);

		assert_eq!(staged.exists(), !deleting);
		assert_eq!(workspace.exists(), !deleting);
		let outputs = [
			(".cryarchiver/work/staged/a.txt", &b"staged-looking"[..]),
			(".cryarchiver/work/x/b.txt", &b"workspace-looking"[..]),
		]
		.map(|(source, content)| {
			let state = state(&setup, source);
			let output = state.output().unwrap_or_else(|| panic!("{source}: {state:?}"));
			(output.path.to_path(&setup.destination), content)
		});
		for (output, content) in &outputs {
			assert_eq!(fs::read(output).unwrap(), *content, "deleting: {deleting}");
		}
		assert!(
			setup
				.destination
				.join(".cryarchiver (source)")
				.join("work")
				.join("x")
				.join("b.txt")
				.exists()
		);

		let project = support::open(&setup, &encoder);
		let mut snapshot = Snapshot::default();
		run_until_done(&project, &mut snapshot, deleting, 2);
		support::drain(&project, &mut snapshot);
		drop(project);

		for (output, content) in &outputs {
			assert_eq!(
				fs::read(output).ok().as_deref(),
				Some(*content),
				"reopened, deleting: {deleting}"
			);
		}
		assert!(snapshot.issues.is_empty(), "{:?}", snapshot.issues);
		let state = state(&setup, ".cryarchiver/work/x/b.txt");
		assert!(
			matches!(
				(&state, deleting),
				(FileState::OriginalDeleted(_), true) | (FileState::Done(_), false)
			),
			"{state:?}"
		);
	}
}
