//! Crash recovery (project layer spec §12): the project task stops dead at each durable intent, and after a lost
//! batch; reopening must lose no archived data and leave nothing half-written.

mod support;

use std::fs;
use std::path::MAIN_SEPARATOR_STR;
use std::sync::Arc;

use cryarchiver_project::issues::{Problem, Verb};
use cryarchiver_project::keys::RelPath;
use cryarchiver_project::record::{FileRecord, FileState, How, Output};
use cryarchiver_project::settings::{DELETE_CONFIRMATION, SettingsChange};
use cryarchiver_project::store::Store;
use cryarchiver_project::view::NoticeLevel;
use cryarchiver_project::{Command, Fault};
use support::{FakeEncoder, Snapshot, jpeg, setup, wait_for, write};

/// Above the 64 KiB dedup minimum, so copies of it are duplicates.
const BIG: usize = 100_000;

/// The fake encoder's output for `a.jpg` has this size; a file the project didn't write, of the same size.
const UNKNOWN: &[u8] = b"not our file!";

fn records(setup: &support::Setup) -> Vec<(String, FileRecord)> {
	let (_, _, loaded) = Store::open(&setup.project).unwrap();
	loaded
		.files
		.into_iter()
		.map(|(key, record)| (key.display(), record))
		.collect()
}

fn state(setup: &support::Setup, path: &str) -> FileState {
	records(setup)
		.into_iter()
		.find(|(key, _)| key == path)
		.map_or_else(|| panic!("no record for {path}"), |(_, record)| record.state)
}

fn output_of(setup: &support::Setup, path: &str) -> Output {
	let state = state(setup, path);
	state
		.output()
		.cloned()
		.unwrap_or_else(|| panic!("{path} has no output: {state:?}"))
}

fn native(path: &str) -> String {
	path.replace('/', MAIN_SEPARATOR_STR)
}

fn crash_during_a_run(setup: &support::Setup, encoder: &Arc<FakeEncoder>, fault: Fault, delete_confirmed: bool) {
	let project = support::open_with(setup, encoder, Some(fault));
	let mut snapshot = Snapshot::default();
	project.send(Command::Start { delete_confirmed });
	wait_for(&project, &mut snapshot, "the crash", |s| s.stopped);
}

fn run_to_the_end(setup: &support::Setup, encoder: &Arc<FakeEncoder>) -> Snapshot {
	let project = support::open(setup, encoder);
	let mut snapshot = Snapshot::default();
	project.send(Command::Start {
		delete_confirmed: false,
	});
	wait_for(&project, &mut snapshot, "the run to finish", |s| {
		s.overall.as_ref().is_some_and(|overall| overall.parity.is_some())
	});
	snapshot
}

/// Opens the project until its first scan is done, and closes it again, without a run.
fn open_and_close(setup: &support::Setup, encoder: &Arc<FakeEncoder>) -> Snapshot {
	let project = support::open(setup, encoder);
	let mut snapshot = Snapshot::default();
	wait_for(&project, &mut snapshot, "the scan", Snapshot::scan_complete);
	snapshot
}

fn problems(snapshot: &Snapshot) -> Vec<Problem> {
	snapshot.issues.iter().map(|issue| issue.problem).collect()
}

/// Every issue open at the end is the unknown destination file's.
fn assert_only_unknown_files_reported(snapshot: &Snapshot) {
	let problems = problems(snapshot);
	assert!(
		problems.iter().all(|problem| *problem == Problem::UnknownFile),
		"{problems:?}"
	);
}

fn enable_deleting(setup: &support::Setup, encoder: &Arc<FakeEncoder>) {
	let project = support::open(setup, encoder);
	project.send(Command::ChangeSettings(SettingsChange::DeleteOriginals {
		enabled: true,
		confirmation: DELETE_CONFIRMATION.into(),
	}));
	let mut snapshot = Snapshot::default();
	wait_for(&project, &mut snapshot, "the setting", |s| {
		s.settings.as_ref().is_some_and(|settings| settings.delete_originals)
	});
}

fn delete_originals(setup: &support::Setup, encoder: &Arc<FakeEncoder>, count: u64) -> Snapshot {
	let project = support::open(setup, encoder);
	let mut snapshot = Snapshot::default();
	project.send(Command::Start { delete_confirmed: true });
	wait_for(&project, &mut snapshot, "the delete", |s| {
		s.overall
			.as_ref()
			.is_some_and(|overall| overall.originals_deleted == count)
	});
	snapshot
}

#[test]
fn a_crash_after_the_commit_intent_requeues_the_file_and_leaves_nothing_half_written() {
	let setup = setup(|_| {});
	write(&setup.source, "a.jpg", &jpeg(1, 500));
	let encoder = FakeEncoder::new();

	crash_during_a_run(&setup, &encoder, Fault::AfterCommitIntent, false);

	assert!(matches!(state(&setup, "a.jpg"), FileState::Committing { .. }));
	assert!(!setup.destination.join("a.avif").exists());

	let snapshot = run_to_the_end(&setup, &encoder);

	assert!(snapshot.root().complete);
	assert_eq!(fs::read(setup.destination.join("a.avif")).unwrap(), b"encoded a.jpg");
	assert!(matches!(state(&setup, "a.jpg"), FileState::Done(_)));
	assert!(!setup.destination.join(".cryarchiver").join("work").exists());
}

#[test]
fn a_crash_that_loses_the_batch_after_the_rename_keeps_the_output() {
	let setup = setup(|_| {});
	write(&setup.source, "a.jpg", &jpeg(1, 500));
	let encoder = FakeEncoder::new();

	crash_during_a_run(&setup, &encoder, Fault::AfterPlaced, false);

	assert!(setup.destination.join("a.avif").exists());
	assert!(matches!(state(&setup, "a.jpg"), FileState::Committing { .. }));

	run_to_the_end(&setup, &encoder);

	assert_eq!(encoder.calls().len(), 1, "a committed output is never encoded again");
	assert!(matches!(state(&setup, "a.jpg"), FileState::Done(_)));
	assert_eq!(fs::read(setup.destination.join("a.avif")).unwrap(), b"encoded a.jpg");
}

#[test]
fn discoveries_lost_with_a_batch_are_found_again() {
	let setup = setup(|_| {});
	write(&setup.source, "a.jpg", &jpeg(1, 500));
	for index in 0..20 {
		write(&setup.source, &format!("Docs/{index}.txt"), b"doc");
	}
	let encoder = FakeEncoder::new();

	crash_during_a_run(&setup, &encoder, Fault::AfterCommitIntent, false);
	let snapshot = run_to_the_end(&setup, &encoder);

	assert_eq!(snapshot.root().files_total, 21);
	assert!(snapshot.root().complete);
	assert_eq!(records(&setup).len(), 21);
}

#[test]
fn a_crash_after_the_commit_intent_never_touches_a_same_sized_file_already_at_the_name() {
	let setup = setup(|_| {});
	write(&setup.source, "a.jpg", &jpeg(1, 500));
	let unknown = write(&setup.destination, "a.avif", UNKNOWN);
	let encoder = FakeEncoder::new();

	crash_during_a_run(&setup, &encoder, Fault::AfterCommitIntent, false);

	let FileState::Committing { output, .. } = state(&setup, "a.jpg") else {
		panic!("not committing");
	};
	assert_eq!(output.path.display(), "a (2).avif");
	assert_eq!(fs::read(&unknown).unwrap(), UNKNOWN);

	let snapshot = run_to_the_end(&setup, &encoder);

	assert_eq!(fs::read(&unknown).unwrap(), UNKNOWN);
	assert_eq!(
		fs::read(setup.destination.join("a (2).avif")).unwrap(),
		b"encoded a.jpg"
	);
	assert_eq!(output_of(&setup, "a.jpg").path.display(), "a (2).avif");
	assert!(matches!(state(&setup, "a.jpg"), FileState::Done(_)));
	assert_only_unknown_files_reported(&snapshot);
}

#[test]
fn a_same_sized_file_that_appears_at_the_named_path_after_the_commit_intent_is_never_adopted() {
	let setup = setup(|_| {});
	write(&setup.source, "a.jpg", &jpeg(1, 500));
	let encoder = FakeEncoder::new();

	crash_during_a_run(&setup, &encoder, Fault::AfterCommitIntent, false);
	let FileState::Committing { output, .. } = state(&setup, "a.jpg") else {
		panic!("not committing");
	};
	assert_eq!(output.path.display(), "a.avif");
	assert_eq!(output.size, UNKNOWN.len() as u64);
	let unknown = write(&setup.destination, "a.avif", UNKNOWN);

	drop(open_and_close(&setup, &encoder));

	assert_eq!(state(&setup, "a.jpg"), FileState::Queued);
	assert_eq!(fs::read(&unknown).unwrap(), UNKNOWN);

	let snapshot = run_to_the_end(&setup, &encoder);

	assert_eq!(fs::read(&unknown).unwrap(), UNKNOWN);
	assert_eq!(output_of(&setup, "a.jpg").path.display(), "a (2).avif");
	assert_eq!(
		fs::read(setup.destination.join("a (2).avif")).unwrap(),
		b"encoded a.jpg"
	);
	assert_only_unknown_files_reported(&snapshot);
}

#[test]
fn a_crash_after_the_rename_never_touches_a_same_sized_file_already_at_the_name() {
	let setup = setup(|_| {});
	write(&setup.source, "a.jpg", &jpeg(1, 500));
	let unknown = write(&setup.destination, "a.avif", UNKNOWN);
	let encoder = FakeEncoder::new();

	crash_during_a_run(&setup, &encoder, Fault::AfterPlaced, false);

	assert!(matches!(state(&setup, "a.jpg"), FileState::Committing { .. }));
	assert_eq!(fs::read(&unknown).unwrap(), UNKNOWN);
	assert_eq!(
		fs::read(setup.destination.join("a (2).avif")).unwrap(),
		b"encoded a.jpg"
	);

	let snapshot = run_to_the_end(&setup, &encoder);

	assert_eq!(encoder.calls().len(), 1, "a committed output is never encoded again");
	assert_eq!(fs::read(&unknown).unwrap(), UNKNOWN);
	assert_eq!(output_of(&setup, "a.jpg").path.display(), "a (2).avif");
	assert!(matches!(state(&setup, "a.jpg"), FileState::Done(_)));
	assert_only_unknown_files_reported(&snapshot);
}

#[test]
fn a_crash_before_a_re_archive_replaces_its_own_output_never_takes_the_old_output_for_the_new() {
	let setup = setup(|_| {});
	let notes = write(&setup.source, "notes.txt", b"version 1");
	let encoder = FakeEncoder::new();
	drop(run_to_the_end(&setup, &encoder));
	fs::write(&notes, b"version 2").unwrap();

	crash_during_a_run(&setup, &encoder, Fault::AfterCommitIntent, false);

	let FileState::Committing { output, .. } = state(&setup, "notes.txt") else {
		panic!("not committing");
	};
	assert_eq!(output.path.display(), "notes.txt");
	assert_eq!(fs::read(setup.destination.join("notes.txt")).unwrap(), b"version 1");

	drop(open_and_close(&setup, &encoder));

	assert_eq!(state(&setup, "notes.txt"), FileState::Queued);

	run_to_the_end(&setup, &encoder);

	assert_eq!(fs::read(setup.destination.join("notes.txt")).unwrap(), b"version 2");
	assert!(matches!(state(&setup, "notes.txt"), FileState::Done(_)));
}

#[test]
fn a_crash_after_a_re_archive_replaced_its_own_output_keeps_the_new_one() {
	let setup = setup(|_| {});
	let notes = write(&setup.source, "notes.txt", b"version 1");
	let encoder = FakeEncoder::new();
	drop(run_to_the_end(&setup, &encoder));
	fs::write(&notes, b"version 2").unwrap();

	crash_during_a_run(&setup, &encoder, Fault::AfterPlaced, false);

	assert!(matches!(state(&setup, "notes.txt"), FileState::Committing { .. }));
	assert_eq!(fs::read(setup.destination.join("notes.txt")).unwrap(), b"version 2");

	drop(open_and_close(&setup, &encoder));

	let FileState::Done(output) = state(&setup, "notes.txt") else {
		panic!("not done");
	};
	assert_eq!(output.path.display(), "notes.txt");
	assert_eq!(fs::read(setup.destination.join("notes.txt")).unwrap(), b"version 2");
}

#[test]
fn a_crash_after_delete_pending_checks_again_before_deleting() {
	let setup = setup(|_| {});
	let notes = write(&setup.source, "notes.txt", b"notes");
	let encoder = FakeEncoder::new();
	drop(run_to_the_end(&setup, &encoder));
	enable_deleting(&setup, &encoder);

	crash_during_a_run(&setup, &encoder, Fault::AfterDeleteIntent, true);

	assert!(matches!(state(&setup, "notes.txt"), FileState::DeletePending(_)));
	assert!(notes.exists());

	drop(open_and_close(&setup, &encoder));

	assert!(notes.exists(), "opening never deletes");
	assert!(matches!(state(&setup, "notes.txt"), FileState::Done(_)));

	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();
	project.send(Command::Start {
		delete_confirmed: false,
	});
	wait_for(&project, &mut snapshot, "the refusal to start", |s| {
		s.notices.iter().any(|notice| notice.level == NoticeLevel::Error)
	});
	drop(project);

	assert!(notes.exists(), "a start without the confirmation never deletes");
	assert!(matches!(state(&setup, "notes.txt"), FileState::Done(_)));

	drop(delete_originals(&setup, &encoder, 1));

	assert!(!notes.exists());
	assert!(matches!(state(&setup, "notes.txt"), FileState::OriginalDeleted(_)));
	assert_eq!(fs::read(setup.destination.join("notes.txt")).unwrap(), b"notes");
}

#[test]
fn a_crash_after_delete_pending_with_the_source_gone_counts_it_deleted() {
	let setup = setup(|_| {});
	let notes = write(&setup.source, "notes.txt", b"notes");
	let encoder = FakeEncoder::new();
	drop(run_to_the_end(&setup, &encoder));
	enable_deleting(&setup, &encoder);
	crash_during_a_run(&setup, &encoder, Fault::AfterDeleteIntent, true);
	fs::remove_file(&notes).unwrap();

	drop(support::open(&setup, &encoder));

	assert!(matches!(state(&setup, "notes.txt"), FileState::OriginalDeleted(_)));
	assert_eq!(fs::read(setup.destination.join("notes.txt")).unwrap(), b"notes");
}

#[test]
fn a_crash_after_delete_pending_with_the_whole_folder_gone_is_no_proof_of_the_delete() {
	let setup = setup(|_| {});
	write(&setup.source, "Docs/notes.txt", b"notes");
	let encoder = FakeEncoder::new();
	drop(run_to_the_end(&setup, &encoder));
	enable_deleting(&setup, &encoder);
	crash_during_a_run(&setup, &encoder, Fault::AfterDeleteIntent, true);
	assert!(matches!(state(&setup, "Docs/notes.txt"), FileState::DeletePending(_)));
	fs::remove_dir_all(setup.source.join("Docs")).unwrap();

	drop(open_and_close(&setup, &encoder));

	let state = state(&setup, "Docs/notes.txt");
	assert!(matches!(state, FileState::SourceGone(_)), "{state:?}");
	assert_eq!(
		fs::read(setup.destination.join("Docs").join("notes.txt")).unwrap(),
		b"notes"
	);
}

#[test]
fn a_crash_after_delete_pending_then_a_changed_source_is_archived_again_before_its_delete() {
	let setup = setup(|_| {});
	let notes = write(&setup.source, "notes.txt", b"notes");
	let encoder = FakeEncoder::new();
	drop(run_to_the_end(&setup, &encoder));
	enable_deleting(&setup, &encoder);
	crash_during_a_run(&setup, &encoder, Fault::AfterDeleteIntent, true);
	fs::write(&notes, b"newer notes").unwrap();

	drop(open_and_close(&setup, &encoder));

	assert_eq!(fs::read(&notes).unwrap(), b"newer notes");
	assert!(!matches!(
		state(&setup, "notes.txt"),
		FileState::OriginalDeleted(_) | FileState::DeletePending(_)
	));

	drop(delete_originals(&setup, &encoder, 1));

	assert!(!notes.exists());
	assert_eq!(fs::read(setup.destination.join("notes.txt")).unwrap(), b"newer notes");
	let FileState::OriginalDeleted(output) = state(&setup, "notes.txt") else {
		panic!("not deleted");
	};
	assert_eq!(output.size, 11);
}

/// `Deep/a.jpg` archived in a first run, then `a.jpg`, a shallower copy of it, appears and takes its output over.
fn a_shallower_copy_appears(setup: &support::Setup, encoder: &Arc<FakeEncoder>) {
	write(&setup.source, "Deep/a.jpg", &jpeg(3, BIG));
	drop(run_to_the_end(setup, encoder));
	assert!(setup.destination.join("Deep").join("a.avif").exists());
	write(&setup.source, "a.jpg", &jpeg(3, BIG));
}

fn assert_the_take_over_ended_with_the_placeholder(setup: &support::Setup, snapshot: &Snapshot, output: &str) {
	assert!(problems(snapshot).is_empty(), "{:?}", problems(snapshot));
	assert_eq!(fs::read(setup.destination.join(output)).unwrap(), b"encoded a.jpg");
	assert!(!setup.destination.join("Deep").join("a.avif").exists());

	let taken = output_of(setup, "a.jpg");
	assert_eq!(taken.path.display(), output);
	assert_eq!(taken.how, How::Encoded);
	assert!(matches!(state(setup, "a.jpg"), FileState::Done(_)));

	let FileState::Done(placeholder) = state(setup, "Deep/a.jpg") else {
		panic!("not done");
	};
	assert_eq!(
		placeholder.how,
		How::Placeholder {
			canonical: RelPath::from_bytes(b"a.jpg".to_vec()),
			lost: false,
		}
	);
	let text = fs::read_to_string(setup.destination.join(placeholder.path.display())).unwrap();
	assert!(text.contains(&native(&format!("../{output}"))), "{text}");
}

#[test]
fn a_crash_right_after_a_take_overs_first_commit_ends_with_the_placeholder() {
	let setup = setup(|_| {});
	let encoder = FakeEncoder::new();
	a_shallower_copy_appears(&setup, &encoder);

	crash_during_a_run(&setup, &encoder, Fault::AfterPlaced, false);

	let FileState::Committing { output, .. } = state(&setup, "a.jpg") else {
		panic!("not committing");
	};
	assert_eq!(output.how, How::Encoded);
	assert!(output.hash.is_some(), "a moved output keeps its hash");
	assert!(setup.destination.join("a.avif").exists());
	assert!(!setup.destination.join("Deep").join("a.avif").exists());
	assert_eq!(output.path.display(), "a.avif");

	let snapshot = run_to_the_end(&setup, &encoder);

	assert_eq!(encoder.calls().len(), 1, "a take-over never encodes again");
	assert_the_take_over_ended_with_the_placeholder(&setup, &snapshot, "a.avif");
}

#[test]
fn a_crash_after_a_take_overs_intent_leaves_the_output_where_it_was() {
	let setup = setup(|_| {});
	let encoder = FakeEncoder::new();
	a_shallower_copy_appears(&setup, &encoder);

	crash_during_a_run(&setup, &encoder, Fault::AfterCommitIntent, false);

	assert!(matches!(state(&setup, "a.jpg"), FileState::Committing { .. }));
	assert!(setup.destination.join("Deep").join("a.avif").exists());
	assert!(!setup.destination.join("a.avif").exists());

	drop(open_and_close(&setup, &encoder));

	assert!(!matches!(
		state(&setup, "a.jpg"),
		FileState::Committing { .. } | FileState::Done(_)
	));
	assert_eq!(output_of(&setup, "Deep/a.jpg").path.display(), "Deep/a.avif");

	let snapshot = run_to_the_end(&setup, &encoder);

	assert_eq!(encoder.calls().len(), 1, "a take-over never encodes again");
	assert_the_take_over_ended_with_the_placeholder(&setup, &snapshot, "a.avif");
}

#[test]
fn a_crash_right_after_a_take_over_never_touches_a_same_sized_file_at_the_name() {
	let setup = setup(|_| {});
	let encoder = FakeEncoder::new();
	a_shallower_copy_appears(&setup, &encoder);
	let unknown = write(&setup.destination, "a.avif", UNKNOWN);

	crash_during_a_run(&setup, &encoder, Fault::AfterPlaced, false);

	assert_eq!(fs::read(&unknown).unwrap(), UNKNOWN);

	let snapshot = run_to_the_end(&setup, &encoder);

	assert_eq!(encoder.calls().len(), 1, "a take-over never encodes again");
	assert_eq!(fs::read(&unknown).unwrap(), UNKNOWN);
	let problems = problems(&snapshot);
	assert!(
		problems.iter().all(|problem| *problem == Problem::UnknownFile),
		"{problems:?}"
	);
	let taken = output_of(&setup, "a.jpg");
	assert_eq!(taken.path.display(), "a (2).avif");
	assert_eq!(
		fs::read(setup.destination.join("a (2).avif")).unwrap(),
		b"encoded a.jpg"
	);
	let FileState::Done(placeholder) = state(&setup, "Deep/a.jpg") else {
		panic!("not done");
	};
	assert!(matches!(placeholder.how, How::Placeholder { lost: false, .. }));
	let text = fs::read_to_string(setup.destination.join(placeholder.path.display())).unwrap();
	assert!(text.contains(&native("../a (2).avif")), "{text}");
}

/// Task 17's concern: an output accepted as it is has no hash, so a crash-cut swap can't be recognised by it. Such an
/// output is never a canonical copy, so no take-over moves it: the shallower copy is archived on its own, and a crash
/// in its commit leaves the accepted output and its record alone.
#[test]
fn an_output_accepted_without_a_hash_is_never_taken_over_even_through_a_crash() {
	let setup = setup(|_| {});
	write(&setup.source, "Deep/a.jpg", &jpeg(3, BIG));
	let encoder = FakeEncoder::new();
	drop(run_to_the_end(&setup, &encoder));
	let accepted = setup.destination.join("Deep").join("a.avif");
	fs::write(&accepted, b"changed").unwrap();

	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();
	wait_for(&project, &mut snapshot, "the changed output", |s| {
		s.scan_complete() && problems(s) == [Problem::OutputMissing]
	});
	project.send(Command::Resolve(snapshot.issues[0].id, Verb::Accept));
	wait_for(&project, &mut snapshot, "the accept", |s| s.issues.is_empty());
	drop(project);
	let held = output_of(&setup, "Deep/a.jpg");
	assert_eq!(held.hash, None);
	assert_eq!(held.size, 7);

	write(&setup.source, "a.jpg", &jpeg(3, BIG));
	crash_during_a_run(&setup, &encoder, Fault::AfterPlaced, false);

	assert_eq!(fs::read(&accepted).unwrap(), b"changed");
	let FileState::Committing { output: committing, .. } = state(&setup, "a.jpg") else {
		panic!("not committing");
	};
	assert_eq!(committing.how, How::Encoded);
	assert!(committing.hash.is_some());

	let snapshot = run_to_the_end(&setup, &encoder);

	assert!(problems(&snapshot).is_empty(), "{:?}", problems(&snapshot));
	assert_eq!(encoder.calls().len(), 2, "a hashless output is never taken over");
	assert_eq!(fs::read(setup.destination.join("a.avif")).unwrap(), b"encoded a.jpg");
	assert_eq!(fs::read(&accepted).unwrap(), b"changed");
	assert_eq!(state(&setup, "Deep/a.jpg"), FileState::Done(held));
	assert!(matches!(state(&setup, "a.jpg"), FileState::Done(_)));
}
