mod support;

use std::fs;

use cryarchiver_engine::outcome::ErrorKind;
use cryarchiver_project::Command;
use cryarchiver_project::issues::{Problem, Verb};
use cryarchiver_project::record::{CopyReason, FileRecord, FileState, How};
use cryarchiver_project::store::Store;
use cryarchiver_project::tree::FolderId;
use cryarchiver_project::view::RowState;
use support::{FakeEncoder, Rule, Snapshot, jpeg, setup, wait_for, write};

fn records(setup: &support::Setup) -> Vec<(String, FileRecord)> {
	let (_, _, loaded) = Store::open(&setup.project).unwrap();
	loaded
		.files
		.into_iter()
		.map(|(key, record)| (key.display(), record))
		.collect()
}

fn record(setup: &support::Setup, path: &str) -> FileRecord {
	records(setup)
		.into_iter()
		.find(|(key, _)| key == path)
		.map_or_else(|| panic!("no record for {path}"), |(_, record)| record)
}

fn start(project: &cryarchiver_project::Project) {
	project.send(Command::Start {
		delete_confirmed: false,
	});
}

fn complete(snapshot: &Snapshot) -> bool {
	snapshot.scan_complete() && snapshot.root().complete
}

#[test]
fn media_is_encoded_and_other_files_are_copied() {
	let setup = setup(|_| {});
	write(&setup.source, "Photos/a.jpg", &jpeg(1, 5000));
	write(&setup.source, "notes.txt", b"plain notes");
	write(&setup.source, "page.html", b"<html><body>saved page</body></html>");
	let encoder = FakeEncoder::new();
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();

	start(&project);
	wait_for(&project, &mut snapshot, "everything archived", complete);
	drop(project);

	assert_eq!(
		fs::read(setup.destination.join("Photos").join("a.avif")).unwrap(),
		b"encoded a.jpg"
	);
	assert_eq!(fs::read(setup.destination.join("notes.txt")).unwrap(), b"plain notes");
	assert_eq!(
		fs::read(setup.destination.join("page.html")).unwrap(),
		b"<html><body>saved page</body></html>"
	);
	let mut calls = encoder.calls();
	calls.sort();
	assert_eq!(calls, ["a.jpg", "notes.txt"]);

	let FileState::Done(photo) = record(&setup, "Photos/a.jpg").state else {
		panic!("not done")
	};
	assert_eq!(photo.how, How::Encoded);
	assert_eq!(photo.path.display(), "Photos/a.avif");
	let FileState::Done(notes) = record(&setup, "notes.txt").state else {
		panic!("not done")
	};
	assert_eq!(notes.how, How::Copied(CopyReason::NotMedia));
	assert_eq!(notes.hash.unwrap().0, *blake3::hash(b"plain notes").as_bytes());
	assert!(
		!setup
			.destination
			.join(".cryarchiver")
			.join("work")
			.join("staged")
			.exists()
	);
}

#[test]
fn kept_originals_and_engine_skips_are_copied() {
	let setup = setup(|_| {});
	write(&setup.source, "kept.jpg", &jpeg(1, 100));
	write(&setup.source, "odd.jpg", &jpeg(2, 100));
	let encoder = FakeEncoder::new();
	encoder.rule("kept.jpg", Rule::Keep);
	encoder.rule("odd.jpg", Rule::NotMedia);
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();

	start(&project);
	wait_for(&project, &mut snapshot, "everything archived", complete);
	drop(project);

	assert_eq!(fs::read(setup.destination.join("kept.jpg")).unwrap(), jpeg(1, 100));
	let how = |path| record(&setup, path).state.output().unwrap().how.clone();
	assert_eq!(how("kept.jpg"), How::Copied(CopyReason::KeptOriginal));
	assert_eq!(how("odd.jpg"), How::Copied(CopyReason::NotMedia));
}

#[test]
fn unrecoverable_failures_become_issues_with_their_log_until_retried() {
	let setup = setup(|_| {});
	write(&setup.source, "broken.jpg", &jpeg(1, 100));
	let encoder = FakeEncoder::new();
	encoder.rule(
		"broken.jpg",
		Rule::Fail(ErrorKind::SourceDamaged("no end marker".into())),
	);
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();

	start(&project);
	wait_for(&project, &mut snapshot, "the issue", |s| !s.issues.is_empty());
	let issue = snapshot.issues[0].clone();
	assert_eq!(issue.problem, Problem::SourceDamaged);
	assert_eq!(issue.attempts, 1);
	assert_eq!(issue.verbs.accept, Some("Copy original"));
	assert!(!snapshot.root().complete);

	project.send(Command::IssueLog(issue.id, 1));
	wait_for(&project, &mut snapshot, "the log", |s| {
		s.issue_logs.contains_key(&(issue.id, 1))
	});
	assert_eq!(
		snapshot.issue_logs[&(issue.id, 1)],
		["source damaged: no end marker", "fake tool output"]
	);

	encoder.rule("broken.jpg", Rule::Encode("avif"));
	project.send(Command::Resolve(issue.id, Verb::Retry));
	wait_for(&project, &mut snapshot, "the retry", complete);
	assert!(snapshot.issues.is_empty());
	assert_eq!(encoder.calls().len(), 2);
}

#[test]
fn automatic_retries_back_off_and_then_become_an_issue() {
	let setup = setup(|_| {});
	write(&setup.source, "flaky.jpg", &jpeg(1, 100));
	let encoder = FakeEncoder::new();
	encoder.rule(
		"flaky.jpg",
		Rule::Fail(ErrorKind::DestinationIo("network reset".into())),
	);
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();

	start(&project);
	wait_for(&project, &mut snapshot, "the issue", |s| !s.issues.is_empty());

	assert_eq!(encoder.calls().len(), 3);
	assert_eq!(snapshot.issues[0].problem, Problem::EncodeFailed);
}

#[test]
fn copy_original_and_exclude_resolve_a_failure() {
	let setup = setup(|_| {});
	write(&setup.source, "copy-me.jpg", &jpeg(1, 100));
	write(&setup.source, "skip-me.jpg", &jpeg(2, 100));
	let encoder = FakeEncoder::new();
	encoder.rule(
		".jpg",
		Rule::Fail(ErrorKind::EncodeFailed("the encoder crashed".into())),
	);
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();

	start(&project);
	wait_for(&project, &mut snapshot, "two issues", |s| s.issues.len() == 2);
	let issue = |name: &str| snapshot.issues.iter().find(|issue| issue.name == name).unwrap().id;
	project.send(Command::Resolve(issue("copy-me.jpg"), Verb::Accept));
	project.send(Command::Resolve(issue("skip-me.jpg"), Verb::Exclude));
	wait_for(&project, &mut snapshot, "both resolved", complete);
	drop(project);

	assert_eq!(fs::read(setup.destination.join("copy-me.jpg")).unwrap(), jpeg(1, 100));
	assert_eq!(
		record(&setup, "copy-me.jpg").state.output().unwrap().how,
		How::Copied(CopyReason::Chosen)
	);
	assert_eq!(record(&setup, "skip-me.jpg").state, FileState::Excluded);
	assert!(!setup.destination.join("skip-me.jpg").exists());
}

#[test]
fn stop_now_requeues_running_jobs_without_output() {
	let setup = setup(|_| {});
	write(&setup.source, "long.jpg", &jpeg(1, 100));
	let encoder = FakeEncoder::new();
	encoder.rule("long.jpg", Rule::Hold);
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();

	start(&project);
	wait_for(&project, &mut snapshot, "the job", |s| !s.workers.is_empty());
	project.send(Command::StopNow);
	wait_for(&project, &mut snapshot, "the requeue", |s| s.workers.is_empty());
	drop(project);

	assert_eq!(record(&setup, "long.jpg").state, FileState::Queued);
	assert!(!setup.destination.join("long.avif").exists());
}

#[test]
fn outputs_never_replace_files_the_project_did_not_write() {
	let setup = setup(|_| {});
	write(&setup.source, "a.jpg", &jpeg(1, 100));
	write(&setup.destination, "a.avif", b"someone else's file");
	let encoder = FakeEncoder::new();
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();

	start(&project);
	wait_for(&project, &mut snapshot, "everything archived", complete);
	drop(project);

	assert_eq!(
		fs::read(setup.destination.join("a.avif")).unwrap(),
		b"someone else's file"
	);
	assert_eq!(
		fs::read(setup.destination.join("a (2).avif")).unwrap(),
		b"encoded a.jpg"
	);
}

#[test]
fn a_changed_source_is_archived_again_and_replaces_its_output() {
	let setup = setup(|_| {});
	let notes = write(&setup.source, "notes.txt", b"first version");
	let encoder = FakeEncoder::new();
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();
	start(&project);
	wait_for(&project, &mut snapshot, "the first archive", complete);

	fs::write(&notes, b"second, longer version").unwrap();
	project.send(Command::Rescan);
	wait_for(&project, &mut snapshot, "the re-archive", |s| {
		complete(s) && s.root().bytes_in == 22
	});
	drop(project);

	assert_eq!(
		fs::read(setup.destination.join("notes.txt")).unwrap(),
		b"second, longer version"
	);
	assert_eq!(record(&setup, "notes.txt").replaces, None);
}

#[test]
fn nothing_starts_before_the_start_command() {
	let setup = setup(|_| {});
	write(&setup.source, "a.jpg", &jpeg(1, 100));
	let encoder = FakeEncoder::new();
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();

	wait_for(&project, &mut snapshot, "the scan", Snapshot::scan_complete);
	std::thread::sleep(std::time::Duration::from_millis(200));
	project.send(Command::Watch(FolderId::ROOT, true));
	wait_for(&project, &mut snapshot, "the rows", |s| !s.rows.is_empty());

	assert!(encoder.calls().is_empty());
	assert_eq!(snapshot.row(FolderId::ROOT, "a.jpg").status.state, RowState::Queued);

	start(&project);
	wait_for(&project, &mut snapshot, "everything archived", complete);
}

#[test]
fn a_source_that_changes_while_its_job_runs_is_archived_again() {
	let setup = setup(|_| {});
	let long = write(&setup.source, "long.jpg", &jpeg(1, 100));
	let encoder = FakeEncoder::new();
	encoder.rule("long.jpg", Rule::Hold);
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();

	start(&project);
	wait_for(&project, &mut snapshot, "the job", |s| !s.workers.is_empty());
	encoder.rule("long.jpg", Rule::Encode("avif"));
	fs::write(&long, jpeg(2, 300)).unwrap();
	project.send(Command::Rescan);
	wait_for(&project, &mut snapshot, "the re-archive", |s| {
		complete(s) && s.root().bytes_in == 311
	});
	drop(project);

	assert_eq!(encoder.calls().len(), 2);
	assert_eq!(
		fs::read(setup.destination.join("long.avif")).unwrap(),
		b"encoded long.jpg"
	);
	let record = record(&setup, "long.jpg");
	assert_eq!(record.size, 311);
	assert!(matches!(record.state, FileState::Done(_)));
}
