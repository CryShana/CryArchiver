mod support;

use std::fs;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use cryarchiver_engine::decision::{CrfChoice, Decision, Measured, VideoDecision, Vmaf};
use cryarchiver_engine::outcome::ErrorKind;
use cryarchiver_project::issues::{Problem, Verb};
use cryarchiver_project::keys::RelPath;
use cryarchiver_project::record::{CopyReason, FileRecord, FileState, How, Output};
use cryarchiver_project::store::Store;
use cryarchiver_project::tree::FolderId;
use cryarchiver_project::view::RowState;
use cryarchiver_project::{Command, Fault, Tuning};
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

fn searched(crf: f64) -> Decision {
	Decision {
		video: Some(VideoDecision {
			encoder: "SVT-AV1 preset 5".into(),
			crf: Some(crf),
			chosen: Some(CrfChoice::Searched),
			target: Some(Vmaf { mean: 93.0, p5: 89.0 }),
			probes: vec![Measured::default(); 3],
			checks: vec![Measured {
				crf,
				mean: 93.4,
				p5: 89.6,
			}],
			..VideoDecision::default()
		}),
		attempts: 1,
		..Decision::default()
	}
}

fn decision_shown(
	project: &cryarchiver_project::Project,
	snapshot: &mut Snapshot,
	name: &str,
) -> Vec<(String, String)> {
	project.send(Command::Watch(FolderId::ROOT, true));
	wait_for(project, snapshot, "the rows", |s| {
		s.rows
			.get(&FolderId::ROOT)
			.is_some_and(|rows| rows.iter().any(|row| row.name == name))
	});
	let file = snapshot.row(FolderId::ROOT, name).id;
	snapshot.details.clear();
	project.send(Command::Details(file));
	wait_for(project, snapshot, "the details", |s| s.details.contains_key(&file));

	snapshot.details[&file]
		.decision
		.iter()
		.map(|line| (line.label.clone(), line.value.clone()))
		.collect()
}

#[test]
fn what_the_engine_decided_is_kept_with_the_file_and_shown_after_a_reopen() {
	let setup = setup(|_| {});
	write(&setup.source, "clip.jpg", &jpeg(1, 100));
	write(&setup.source, "kept.jpg", &jpeg(2, 100));
	let encoder = FakeEncoder::new();
	encoder.decide("clip.jpg", searched(36.75));
	encoder.rule("kept.jpg", Rule::Keep);
	encoder.decide("kept.jpg", searched(20.0));
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();

	start(&project);
	wait_for(&project, &mut snapshot, "everything archived", complete);
	let shown = decision_shown(&project, &mut snapshot, "clip.jpg");
	drop(project);

	let expected = [
		("Encoder".to_owned(), "SVT-AV1 preset 5".to_owned()),
		("CRF".to_owned(), "36.75 (searched, 3 probes)".to_owned()),
		("Check".to_owned(), "VMAF 93.4 / 89.6 vs 93 / 89".to_owned()),
	];
	assert_eq!(shown, expected);
	let (store, _, _) = Store::open(&setup.project).unwrap();
	let stored = |name: &str| store.latest_decision(&RelPath::from_bytes(name.into())).unwrap();
	assert_eq!(stored("clip.jpg"), Some(searched(36.75)));
	assert_eq!(
		stored("kept.jpg"),
		Some(searched(20.0)),
		"a kept original keeps its record too"
	);
	drop(store);

	let project = support::open(&setup, &FakeEncoder::new());
	let mut snapshot = Snapshot::default();
	assert_eq!(decision_shown(&project, &mut snapshot, "clip.jpg"), expected);
}

#[test]
fn a_new_job_clears_the_records_of_earlier_jobs_even_at_a_higher_attempt() {
	let setup = setup(|_| {});
	write(&setup.source, "page.html", b"<html><body>saved page</body></html>");
	write(&setup.source, "encoded.jpg", &jpeg(1, 100));
	let (mut store, _, _) = Store::open(&setup.project).unwrap();
	for name in ["page.html", "encoded.jpg"] {
		store
			.set_decision(&RelPath::from_bytes(name.into()), 3, &searched(20.0))
			.unwrap();
	}
	store.flush().unwrap();
	drop(store);
	let encoder = FakeEncoder::new();
	encoder.decide("encoded.jpg", searched(30.0));
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();

	start(&project);
	wait_for(&project, &mut snapshot, "everything archived", complete);
	drop(project);

	let (store, _, _) = Store::open(&setup.project).unwrap();
	let stored = |name: &str| store.latest_decision(&RelPath::from_bytes(name.into())).unwrap();
	assert_eq!(
		stored("page.html"),
		None,
		"a copy leaves no record of an earlier encode"
	);
	assert_eq!(
		stored("encoded.jpg"),
		Some(searched(30.0)),
		"the attempt 1 record shows, not the old attempt 3"
	);
}

#[test]
fn a_file_that_leaves_the_project_takes_its_failure_logs_with_it() {
	let setup = setup(|_| {});
	let broken = write(&setup.source, "broken.jpg", &jpeg(1, 100));
	write(&setup.source, "other.jpg", &jpeg(2, 100));
	let encoder = FakeEncoder::new();
	encoder.rule("jpg", Rule::Fail(ErrorKind::SourceDamaged("no end marker".into())));
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();

	start(&project);
	wait_for(&project, &mut snapshot, "the issues", |s| s.issues.len() == 2);
	fs::remove_file(&broken).unwrap();
	project.send(Command::Rescan);
	wait_for(&project, &mut snapshot, "the file to leave", |s| s.issues.len() == 1);
	drop(project);

	let (_, _, loaded) = Store::open(&setup.project).unwrap();
	let attempts: Vec<String> = loaded.last_attempts.keys().map(RelPath::display).collect();
	assert_eq!(attempts, ["other.jpg"]);
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
	project.send(Command::Watch(FolderId::ROOT, true));
	wait_for(&project, &mut snapshot, "the rows", |s| !s.rows.is_empty());
	let file = snapshot.row(FolderId::ROOT, "a.jpg").id;
	let read = |s: &Snapshot| {
		s.details
			.get(&file)
			.is_some_and(|details| details.plan.contains(&("Kind".to_owned(), "media".to_owned())))
	};
	while !read(&snapshot) {
		project.send(Command::Details(file));
		wait_for(&project, &mut snapshot, "the details", |s| {
			s.details.contains_key(&file)
		});
		if !read(&snapshot) {
			snapshot.details.clear();
		}
	}

	assert_eq!(
		snapshot.details[&file].state,
		RowState::Queued,
		"ready, and still not started"
	);
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

fn crash_after_the_intent(setup: &support::Setup, encoder: &std::sync::Arc<FakeEncoder>) -> Output {
	let project = support::open_with(setup, encoder, Some(Fault::AfterCommitIntent));
	let mut snapshot = Snapshot::default();
	start(&project);
	wait_for(&project, &mut snapshot, "the crash", |s| s.stopped);
	drop(project);

	match record(setup, "notes.txt").state {
		FileState::Committing { output, .. } => output,
		state => panic!("no intent: {state:?}"),
	}
}

#[test]
fn a_same_sized_file_found_at_the_named_path_after_a_crash_is_not_taken_for_the_output() {
	let setup = setup(|_| {});
	write(&setup.source, "notes.txt", b"our notes");
	let encoder = FakeEncoder::new();
	let intent = crash_after_the_intent(&setup, &encoder);
	assert_eq!(intent.path.display(), "notes.txt");
	write(&setup.destination, "notes.txt", b"not ours!");

	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();
	wait_for(&project, &mut snapshot, "the scan", Snapshot::scan_complete);
	drop(project);
	assert_eq!(record(&setup, "notes.txt").state, FileState::Queued);

	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();
	start(&project);
	wait_for(&project, &mut snapshot, "everything archived", complete);
	drop(project);

	assert_eq!(fs::read(setup.destination.join("notes.txt")).unwrap(), b"not ours!");
	assert_eq!(fs::read(setup.destination.join("notes (2).txt")).unwrap(), b"our notes");
	assert_eq!(
		record(&setup, "notes.txt").state.output().unwrap().path.display(),
		"notes (2).txt"
	);
}

#[test]
fn the_first_intent_already_avoids_a_name_something_occupies() {
	let setup = setup(|_| {});
	write(&setup.source, "notes.txt", b"our notes");
	write(&setup.destination, "notes.txt", b"not ours!");
	let encoder = FakeEncoder::new();

	let intent = crash_after_the_intent(&setup, &encoder);

	assert_eq!(intent.path.display(), "notes (2).txt");
	assert_eq!(fs::read(setup.destination.join("notes.txt")).unwrap(), b"not ours!");
}

#[test]
fn a_source_that_changes_under_a_slow_job_never_has_two_jobs_at_once() {
	let setup = setup(|_| {});
	let long = write(&setup.source, "long.jpg", &jpeg(1, 100));
	let encoder = FakeEncoder::new();
	encoder.rule("long.jpg", Rule::Slow(Duration::from_millis(500)));
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();

	start(&project);
	wait_for(&project, &mut snapshot, "the job", |s| !s.workers.is_empty());
	fs::write(&long, jpeg(2, 300)).unwrap();
	project.send(Command::Rescan);
	wait_for(&project, &mut snapshot, "the re-archive", |s| {
		complete(s) && s.root().bytes_in == 311 && s.workers.is_empty()
	});
	drop(project);

	assert_eq!(encoder.peak.load(Ordering::SeqCst), 1);
	assert_eq!(encoder.calls().len(), 2);
	assert_eq!(fs::read(setup.destination.join("long.jpg")).unwrap(), jpeg(2, 300));
	let record = record(&setup, "long.jpg");
	assert_eq!(record.size, 311);
	assert!(matches!(record.state, FileState::Done(_)));
}

#[test]
fn stop_now_leaves_nothing_staged() {
	let setup = setup(|_| {});
	write(&setup.source, "slow.jpg", &jpeg(1, 100));
	let encoder = FakeEncoder::new();
	encoder.rule("slow.jpg", Rule::Slow(Duration::from_millis(300)));
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();

	start(&project);
	wait_for(&project, &mut snapshot, "the job", |s| !s.workers.is_empty());
	project.send(Command::StopNow);
	wait_for(&project, &mut snapshot, "the requeue", |s| s.workers.is_empty());
	let staged = setup.destination.join(".cryarchiver").join("work").join("staged");
	let leftovers = fs::read_dir(&staged).map_or(0, Iterator::count);
	drop(project);

	assert_eq!(leftovers, 0);
	assert_eq!(record(&setup, "slow.jpg").state, FileState::Queued);
	assert!(!setup.destination.join("slow.jpg").exists());
}

#[test]
fn outputs_competing_for_one_name_are_numbered_in_turn() {
	let setup = setup(|_| {});
	write(&setup.source, "a.txt", b"copied as it is");
	write(&setup.source, "a.dat", b"encoded one");
	write(&setup.source, "a.bin", b"encoded two");
	let encoder = FakeEncoder::new();
	encoder.rule("a.dat", Rule::Encode("txt"));
	encoder.rule("a.bin", Rule::Encode("txt"));
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();

	start(&project);
	wait_for(&project, &mut snapshot, "everything archived", complete);
	drop(project);

	let expected: [(&str, &[u8]); 3] = [
		("a.txt", b"copied as it is"),
		("a.dat", b"encoded a.dat"),
		("a.bin", b"encoded a.bin"),
	];
	let mut names = Vec::new();
	for (source, content) in expected {
		let output = record(&setup, source).state.output().unwrap().path.display();
		assert_eq!(fs::read(setup.destination.join(&output)).unwrap(), content, "{source}");
		names.push(output);
	}
	names.sort();
	assert_eq!(names, ["a (2).txt", "a (3).txt", "a.txt"]);
}

#[test]
fn work_starts_as_soon_as_there_is_room_not_on_the_next_tick() {
	const FILES: u32 = 200;

	let setup = setup(|_| {});
	for index in 0..FILES {
		write(&setup.source, &format!("notes/{index}.txt"), b"notes");
	}
	let tick = Duration::from_millis(250);
	let tuning = Tuning {
		tick,
		..support::tuning()
	};
	let project = support::open_tuned(&setup, &FakeEncoder::new(), tuning, None);
	let mut snapshot = Snapshot::default();

	let started = Instant::now();
	start(&project);
	wait_for(&project, &mut snapshot, "everything archived", complete);

	let one_start_per_disk_slot_and_tick = tick * FILES / 2;
	assert!(
		started.elapsed() < one_start_per_disk_slot_and_tick / 3,
		"{:?}",
		started.elapsed()
	);
}

#[test]
fn the_low_disk_pause_names_its_sizes_in_readable_units() {
	let setup = setup(|_| {});
	write(&setup.source, "a.jpg", &jpeg(1, 100));
	let encoder = FakeEncoder::new();
	encoder.rule(
		"a.jpg",
		Rule::Fail(ErrorKind::InsufficientSpace {
			need: 5_000_000_000,
			free: 1_200_000,
		}),
	);
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();
	start(&project);

	wait_for(&project, &mut snapshot, "the low-disk notice", |s| {
		s.notices
			.iter()
			.any(|notice| notice.text == "paused: low disk space (1.20 MB free, 5.00 GB needed)")
	});
}

#[test]
fn a_mirrored_folder_that_is_a_link_into_the_source_gets_nothing_written_through_it() {
	let setup = setup(|_| {});
	write(&setup.source, "Photos/a.jpg", &jpeg(1, 5000));
	write(&setup.source, "Photos/b.txt", b"notes");
	write(&setup.source, "Photos/Deeper/c.txt", b"deeper notes");
	write(&setup.source, "Other/d.txt", b"other notes");
	let link = setup.destination.join("Photos");
	if !support::link_folder(&link, &setup.source.join("Photos")) {
		eprintln!("skipped: no folder link can be made here");
		return;
	}
	let encoder = FakeEncoder::new();
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();

	start(&project);
	wait_for(&project, &mut snapshot, "the run to finish", |s| {
		s.overall.as_ref().is_some_and(|overall| overall.parity.is_some())
	});
	drop(project);

	assert_eq!(
		support::names_in(&setup.source.join("Photos")),
		["Deeper", "a.jpg", "b.txt"]
	);
	assert_eq!(
		support::names_in(&setup.source.join("Photos").join("Deeper")),
		["c.txt"]
	);
	assert_eq!(
		fs::read(setup.destination.join("Other").join("d.txt")).unwrap(),
		b"other notes"
	);
	for name in ["Photos/a.jpg", "Photos/b.txt", "Photos/Deeper/c.txt"] {
		let FileState::Issue { reason, .. } = record(&setup, name).state else {
			panic!("{name} is not an issue: {:?}", record(&setup, name).state);
		};
		assert!(
			reason.contains(&support::shown(&link)) && reason.contains("link"),
			"{name}: {reason}"
		);
	}
}
