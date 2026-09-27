mod support;

use std::collections::HashMap;
use std::fs;
use std::path::{MAIN_SEPARATOR_STR, Path};
use std::sync::Arc;

use cryarchiver_engine::outcome::ErrorKind;
use cryarchiver_project::issues::{Problem, Verb};
use cryarchiver_project::keys::RelPath;
use cryarchiver_project::record::{CopyReason, FileRecord, FileState, How};
use cryarchiver_project::store::Store;
use cryarchiver_project::tree::FolderId;
use cryarchiver_project::view::RowState;
use cryarchiver_project::{Command, Fault, Project};
use support::{FakeEncoder, Rule, Snapshot, jpeg, setup, wait_for, write};

const BIG: usize = 100_000;

/// Bytes no header check takes for media, so the file is copied as it is.
fn data(seed: u8) -> Vec<u8> {
	(0..BIG)
		.map(|index| u8::try_from(index % 251).unwrap().wrapping_mul(7).wrapping_add(seed))
		.collect()
}

fn record(setup: &support::Setup, path: &str) -> FileRecord {
	let (_, _, loaded) = Store::open(&setup.project).unwrap();
	loaded
		.files
		.into_iter()
		.find(|(key, _)| key.display() == path)
		.map_or_else(|| panic!("no record for {path}"), |(_, record)| record)
}

/// How many records name `path` as their output.
fn owners(setup: &support::Setup, path: &str) -> usize {
	let (_, _, loaded) = Store::open(&setup.project).unwrap();
	loaded
		.files
		.iter()
		.filter(|(_, record)| {
			record
				.state
				.output()
				.is_some_and(|output| output.path.display() == path)
		})
		.count()
}

fn run_to_completion(setup: &support::Setup, encoder: &std::sync::Arc<FakeEncoder>) -> Snapshot {
	let project = support::open(setup, encoder);
	let mut snapshot = Snapshot::default();
	project.send(Command::Start {
		delete_confirmed: false,
	});
	wait_for(&project, &mut snapshot, "the run to finish", |s| {
		s.overall.as_ref().is_some_and(|o| o.parity.is_some())
	});
	snapshot
}

fn native(path: &str) -> String {
	path.replace('/', MAIN_SEPARATOR_STR)
}

#[test]
fn duplicates_become_placeholders_pointing_at_the_shallowest_copy() {
	let setup = setup(|_| {});
	write(&setup.source, "Backup/Old/a.jpg", &jpeg(7, BIG));
	write(&setup.source, "Backup/a.jpg", &jpeg(7, BIG));
	write(&setup.source, "a.jpg", &jpeg(7, BIG));
	write(&setup.source, "b.jpg", &jpeg(8, BIG));
	let encoder = FakeEncoder::new();

	let snapshot = run_to_completion(&setup, &encoder);

	assert_eq!(snapshot.overall.as_ref().unwrap().duplicates, 2);
	let mut calls = encoder.calls();
	calls.sort();
	assert_eq!(calls, ["a.jpg", "b.jpg"]);
	assert_eq!(fs::read(setup.destination.join("a.avif")).unwrap(), b"encoded a.jpg");

	let placeholder = fs::read_to_string(setup.destination.join("Backup").join("placeholder_a.jpg.txt")).unwrap();
	assert!(
		placeholder.starts_with("This file is a duplicate. The archived copy is here:\n"),
		"{placeholder}"
	);
	assert!(
		placeholder.contains(&format!("\n{}\n", native("../a.avif"))),
		"{placeholder}"
	);
	assert!(placeholder.contains("Original file: Backup/a.jpg"), "{placeholder}");
	let deeper = fs::read_to_string(
		setup
			.destination
			.join("Backup")
			.join("Old")
			.join("placeholder_a.jpg.txt"),
	)
	.unwrap();
	assert!(deeper.contains(&native("../../a.avif")), "{deeper}");

	let FileState::Done(output) = record(&setup, "Backup/a.jpg").state else {
		panic!("not done")
	};
	assert_eq!(
		output.how,
		How::Placeholder {
			canonical: RelPath::from_bytes(b"a.jpg".to_vec()),
			lost: false,
		}
	);
	assert!(!setup.destination.join("Backup").join("a.avif").exists());
}

#[test]
fn a_placeholder_never_replaces_a_file_holding_its_name() {
	let setup = setup(|_| {});
	write(&setup.source, "a.jpg", &jpeg(6, BIG));
	write(&setup.source, "Backup/a.jpg", &jpeg(6, BIG));
	let occupied = write(&setup.destination, "Backup/placeholder_a.jpg.txt", b"mine");
	let encoder = FakeEncoder::new();

	run_to_completion(&setup, &encoder);

	assert_eq!(fs::read(&occupied).unwrap(), b"mine");
	let placeholder = fs::read_to_string(setup.destination.join("Backup").join("placeholder_a.jpg (2).txt")).unwrap();
	assert!(placeholder.contains(&native("../a.avif")), "{placeholder}");
}

#[test]
fn files_under_64_kib_are_never_placeholders() {
	let setup = setup(|_| {});
	write(&setup.source, "a.jpg", &jpeg(1, 1000));
	write(&setup.source, "b.jpg", &jpeg(1, 1000));
	let encoder = FakeEncoder::new();

	run_to_completion(&setup, &encoder);

	assert_eq!(encoder.calls().len(), 2);
	assert!(setup.destination.join("a.avif").exists());
	assert!(setup.destination.join("b.avif").exists());
}

#[test]
fn a_shallower_copy_found_later_takes_the_archived_output_over() {
	let setup = setup(|_| {});
	write(&setup.source, "Deep/a.jpg", &jpeg(3, BIG));
	let encoder = FakeEncoder::new();
	drop(run_to_completion(&setup, &encoder));
	assert!(setup.destination.join("Deep").join("a.avif").exists());

	write(&setup.source, "a.jpg", &jpeg(3, BIG));
	run_to_completion(&setup, &encoder);

	assert_eq!(encoder.calls().len(), 1, "the swap must not encode again");
	assert_eq!(fs::read(setup.destination.join("a.avif")).unwrap(), b"encoded a.jpg");
	assert!(!setup.destination.join("Deep").join("a.avif").exists());
	let placeholder = fs::read_to_string(setup.destination.join("Deep").join("placeholder_a.jpg.txt")).unwrap();
	assert!(placeholder.contains(&native("../a.avif")), "{placeholder}");
	assert_eq!(record(&setup, "a.jpg").state.output().unwrap().how, How::Encoded);
}

#[test]
fn a_crash_between_the_two_commits_of_a_swap_still_ends_with_the_placeholder() {
	let setup = setup(|_| {});
	write(&setup.source, "Deep/a.jpg", &jpeg(3, BIG));
	let encoder = FakeEncoder::new();
	drop(run_to_completion(&setup, &encoder));
	write(&setup.source, "a.jpg", &jpeg(3, BIG));

	let project = support::open_with(&setup, &encoder, Some(Fault::AfterPlaced));
	let mut snapshot = Snapshot::default();
	project.send(Command::Start {
		delete_confirmed: false,
	});
	wait_for(&project, &mut snapshot, "the crash", |s| s.stopped);
	drop(project);
	assert!(setup.destination.join("a.avif").exists());
	assert!(!setup.destination.join("Deep").join("placeholder_a.jpg.txt").exists());

	let snapshot = run_to_completion(&setup, &encoder);

	assert_eq!(encoder.calls().len(), 1);
	let placeholder = fs::read_to_string(setup.destination.join("Deep").join("placeholder_a.jpg.txt")).unwrap();
	assert!(placeholder.contains(&native("../a.avif")), "{placeholder}");
	assert_eq!(snapshot.overall.unwrap().parity.unwrap().archive_files, 2);
}

#[test]
fn a_copy_of_a_file_whose_source_is_gone_points_at_its_archived_output() {
	let setup = setup(|_| {});
	let first = write(&setup.source, "a.jpg", &jpeg(2, BIG));
	write(&setup.source, "Backup/a.jpg", &jpeg(2, BIG));
	let encoder = FakeEncoder::new();
	drop(run_to_completion(&setup, &encoder));
	fs::remove_file(first).unwrap();
	write(&setup.source, "Other/a.jpg", &jpeg(2, BIG));

	run_to_completion(&setup, &encoder);

	assert_eq!(encoder.calls(), ["a.jpg"]);
	let placeholder = fs::read_to_string(setup.destination.join("Other").join("placeholder_a.jpg.txt")).unwrap();
	assert!(placeholder.contains(&native("../a.avif")), "{placeholder}");
}

#[test]
fn a_hard_link_is_a_duplicate_without_hashing() {
	let setup = setup(|_| {});
	let original = write(&setup.source, "a.jpg", &jpeg(5, BIG));
	fs::create_dir_all(setup.source.join("Links")).unwrap();
	fs::hard_link(&original, setup.source.join("Links").join("b.jpg")).unwrap();
	let encoder = FakeEncoder::new();

	run_to_completion(&setup, &encoder);

	let placeholder = fs::read_to_string(setup.destination.join("Links").join("placeholder_b.jpg.txt")).unwrap();
	assert!(
		placeholder.contains("the same file as the archived copy"),
		"{placeholder}"
	);
	assert_eq!(record(&setup, "Links/b.jpg").full_hash, None);
}

#[test]
fn excluding_a_failed_canonical_copy_makes_the_next_one_canonical() {
	let setup = setup(|_| {});
	write(&setup.source, "a.jpg", &jpeg(9, BIG));
	let encoder = FakeEncoder::new();
	encoder.rule("a.jpg", Rule::Fail(ErrorKind::SourceDamaged("broken".into())));
	drop(run_to_completion(&setup, &encoder));
	write(&setup.source, "Sub/b.jpg", &jpeg(9, BIG));

	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();
	project.send(Command::Start {
		delete_confirmed: false,
	});
	wait_for(&project, &mut snapshot, "the duplicate to wait", |s| {
		!s.issues.is_empty() && s.overall.as_ref().is_some_and(|overall| overall.parity.is_some())
	});
	assert!(!setup.destination.join("Sub").join("b.avif").exists());

	encoder.rule("a.jpg", Rule::Encode("avif"));
	project.send(Command::Resolve(snapshot.issues[0].id, Verb::Exclude));
	wait_for(&project, &mut snapshot, "the run to finish", |s| {
		s.overall
			.as_ref()
			.and_then(|overall| overall.parity)
			.is_some_and(|parity| parity.excluded == 1)
	});
	drop(project);

	assert_eq!(
		fs::read(setup.destination.join("Sub").join("b.avif")).unwrap(),
		b"encoded b.jpg"
	);
	assert!(!setup.destination.join("Sub").join("placeholder_b.jpg.txt").exists());
	assert_eq!(record(&setup, "a.jpg").state, FileState::Excluded);
}

/// The content a file's record says is archived: its own output's, or its canonical copy's.
fn archived_content(setup: &support::Setup, path: &str) -> Vec<u8> {
	let output = record(setup, path)
		.state
		.output()
		.cloned()
		.unwrap_or_else(|| panic!("{path} has no output"));
	let held = match output.how {
		How::Placeholder { canonical, lost: false } => {
			record(setup, &canonical.display()).state.output().unwrap().path.clone()
		}
		How::Placeholder { lost: true, .. } => panic!("{path}'s archived content is lost"),
		How::Encoded | How::Copied(_) => output.path,
	};
	fs::read(setup.destination.join(native(&held.display()))).unwrap()
}

/// The canonical copy `a.bin` changed after `Backup/a.bin` was archived as its placeholder: the duplicate owns the
/// old output, and the new content has a name of its own. Returns the duplicate's state.
fn assert_the_duplicate_kept_the_old_content(setup: &support::Setup) -> FileState {
	assert_eq!(fs::read(setup.destination.join("a.bin")).unwrap(), data(1));
	assert_eq!(fs::read(setup.destination.join("a (2).bin")).unwrap(), data(2));
	assert!(!setup.destination.join("Backup").join("placeholder_a.bin.txt").exists());

	let kept = record(setup, "Backup/a.bin").state;
	let output = kept
		.output()
		.unwrap_or_else(|| panic!("the duplicate has no output: {kept:?}"));
	assert_eq!(output.path.display(), "a.bin");
	assert_eq!(output.how, How::Copied(CopyReason::NotMedia));
	assert_eq!(
		record(setup, "a.bin").state.output().unwrap().path.display(),
		"a (2).bin"
	);
	kept
}

fn assert_the_gone_duplicate_kept_the_old_content(setup: &support::Setup) {
	let kept = assert_the_duplicate_kept_the_old_content(setup);
	assert!(matches!(kept, FileState::SourceGone(_)), "{kept:?}");
}

#[test]
fn a_changed_canonical_copy_leaves_its_output_to_its_placeholder() {
	let setup = setup(|_| {});
	let canonical = write(&setup.source, "a.bin", &data(1));
	write(&setup.source, "Backup/a.bin", &data(1));
	let encoder = FakeEncoder::new();
	drop(run_to_completion(&setup, &encoder));
	assert!(setup.destination.join("Backup").join("placeholder_a.bin.txt").exists());

	fs::write(&canonical, data(2)).unwrap();
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();
	wait_for(&project, &mut snapshot, "the scan", Snapshot::scan_complete);
	drop(project);

	let adopted = record(&setup, "Backup/a.bin").state;
	assert_eq!(
		adopted.output().map(|output| output.path.display()),
		Some("a.bin".to_owned()),
		"{adopted:?}"
	);
	assert_eq!(fs::read(setup.destination.join("a.bin")).unwrap(), data(1));

	drop(run_to_completion(&setup, &encoder));
	assert!(matches!(
		assert_the_duplicate_kept_the_old_content(&setup),
		FileState::Done(_)
	));

	run_to_completion(&setup, &encoder);
	assert!(matches!(
		assert_the_duplicate_kept_the_old_content(&setup),
		FileState::Done(_)
	));
}

#[test]
fn a_changed_canonical_copy_leaves_its_output_to_a_placeholder_whose_original_is_gone() {
	let setup = setup(|_| {});
	let canonical = write(&setup.source, "a.bin", &data(1));
	let duplicate = write(&setup.source, "Backup/a.bin", &data(1));
	let encoder = FakeEncoder::new();
	drop(run_to_completion(&setup, &encoder));
	fs::remove_file(duplicate).unwrap();
	drop(run_to_completion(&setup, &encoder));
	assert!(matches!(record(&setup, "Backup/a.bin").state, FileState::SourceGone(_)));

	fs::write(&canonical, data(2)).unwrap();
	run_to_completion(&setup, &encoder);

	assert_the_gone_duplicate_kept_the_old_content(&setup);
}

#[test]
fn a_duplicate_deleted_in_the_same_scan_its_canonical_copy_changed_in_keeps_its_content() {
	let setup = setup(|_| {});
	let canonical = write(&setup.source, "a.bin", &data(1));
	let duplicate = write(&setup.source, "Backup/a.bin", &data(1));
	let encoder = FakeEncoder::new();
	drop(run_to_completion(&setup, &encoder));

	fs::remove_file(duplicate).unwrap();
	fs::write(&canonical, data(2)).unwrap();
	run_to_completion(&setup, &encoder);

	assert_the_gone_duplicate_kept_the_old_content(&setup);
}

#[test]
fn a_new_file_where_a_gone_source_was_never_replaces_its_output() {
	let setup = setup(|_| {});
	let source = write(&setup.source, "a.bin", &data(1));
	let encoder = FakeEncoder::new();
	drop(run_to_completion(&setup, &encoder));
	fs::remove_file(&source).unwrap();
	drop(run_to_completion(&setup, &encoder));

	let newer = [data(3), b"more".to_vec()].concat();
	fs::write(&source, &newer).unwrap();
	run_to_completion(&setup, &encoder);

	assert_eq!(fs::read(setup.destination.join("a.bin")).unwrap(), data(1));
	assert_eq!(fs::read(setup.destination.join("a (2).bin")).unwrap(), newer);
}

#[test]
fn a_candidate_gone_before_the_hashes_start_leaves_nothing_pending() {
	let setup = setup(|_| {});
	write(&setup.source, "x.bin", &data(1));
	let y = write(&setup.source, "y.bin", &data(2));
	let encoder = FakeEncoder::new();
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();
	project.send(Command::Watch(FolderId::ROOT, true));
	wait_for(&project, &mut snapshot, "both to wait for their hashes", |s| {
		["x.bin", "y.bin"].iter().all(|name| {
			s.rows
				.get(&FolderId::ROOT)
				.and_then(|rows| rows.iter().find(|row| row.name == *name))
				.is_some_and(|row| row.status.state == RowState::Waiting)
		})
	});

	fs::remove_file(y).unwrap();
	project.send(Command::Rescan);
	wait_for(&project, &mut snapshot, "y to go", |s| {
		s.rows
			.get(&FolderId::ROOT)
			.is_some_and(|rows| rows.iter().all(|row| row.name != "y.bin"))
	});
	project.send(Command::Start {
		delete_confirmed: false,
	});
	wait_for(&project, &mut snapshot, "the run to finish", |s| {
		s.overall.as_ref().is_some_and(|o| o.parity.is_some())
	});
	drop(project);

	assert_eq!(fs::read(setup.destination.join("x.bin")).unwrap(), data(1));
}

#[test]
fn identical_sidecars_beside_different_videos_are_both_copied() {
	let setup = setup(|_| {});
	write(&setup.source, "A/movie.mp4", &jpeg(1, 100));
	write(&setup.source, "B/film.mp4", &jpeg(2, 100));
	write(&setup.source, "A/movie.srt", &data(5));
	write(&setup.source, "B/film.srt", &data(5));
	let encoder = FakeEncoder::new();
	encoder.rule(".mp4", Rule::Encode("mkv"));
	drop(run_to_completion(&setup, &encoder));

	run_to_completion(&setup, &encoder);

	assert_eq!(
		fs::read(setup.destination.join("A").join("movie.srt")).unwrap(),
		data(5)
	);
	assert_eq!(fs::read(setup.destination.join("B").join("film.srt")).unwrap(), data(5));
	assert!(!setup.destination.join("B").join("placeholder_film.srt.txt").exists());
}

#[test]
fn a_crash_right_after_an_adoption_leaves_the_output_one_owner() {
	let setup = setup(|_| {});
	let canonical = write(&setup.source, "a.bin", &data(1));
	let duplicate = write(&setup.source, "Backup/a.bin", &data(1));
	let encoder = FakeEncoder::new();
	drop(run_to_completion(&setup, &encoder));
	fs::remove_file(duplicate).unwrap();
	drop(run_to_completion(&setup, &encoder));

	fs::write(&canonical, data(2)).unwrap();
	let project = support::open_with(&setup, &encoder, Some(Fault::AfterAdoption));
	let mut snapshot = Snapshot::default();
	wait_for(&project, &mut snapshot, "the crash", |s| s.stopped);
	drop(project);

	assert_eq!(owners(&setup, "a.bin"), 1);
	assert!(setup.destination.join("Backup").join("placeholder_a.bin.txt").exists());

	run_to_completion(&setup, &encoder);

	assert_the_gone_duplicate_kept_the_old_content(&setup);
	assert_eq!(owners(&setup, "a.bin"), 1);
}

#[test]
fn a_crash_right_after_the_changed_canonical_copys_intent_leaves_the_output_one_owner() {
	let setup = setup(|_| {});
	let canonical = write(&setup.source, "a.bin", &data(1));
	let duplicate = write(&setup.source, "Backup/a.bin", &data(1));
	let encoder = FakeEncoder::new();
	drop(run_to_completion(&setup, &encoder));

	fs::write(&canonical, data(2)).unwrap();
	let project = support::open_with(&setup, &encoder, Some(Fault::AfterCommitIntent));
	let mut snapshot = Snapshot::default();
	project.send(Command::Start {
		delete_confirmed: false,
	});
	wait_for(&project, &mut snapshot, "the crash", |s| s.stopped);
	drop(project);

	let committing = record(&setup, "a.bin").state;
	assert!(matches!(committing, FileState::Committing { .. }), "{committing:?}");
	assert_eq!(owners(&setup, "a.bin"), 1);
	assert_eq!(fs::read(setup.destination.join("a.bin")).unwrap(), data(1));

	fs::remove_file(duplicate).unwrap();
	run_to_completion(&setup, &encoder);

	assert_the_gone_duplicate_kept_the_old_content(&setup);
	assert_eq!(owners(&setup, "a.bin"), 1);
}

fn scans_completed(snapshot: &Snapshot) -> usize {
	snapshot
		.logs
		.iter()
		.filter(|entry| entry.message.starts_with("scan complete"))
		.count()
}

#[test]
fn a_placeholder_whose_file_went_missing_keeps_the_content_when_its_canonical_copy_changes() {
	let setup = setup(|_| {});
	let canonical = write(&setup.source, "a.bin", &data(1));
	write(&setup.source, "Backup/a.bin", &data(1));
	let encoder = FakeEncoder::new();
	drop(run_to_completion(&setup, &encoder));
	fs::remove_file(setup.destination.join("Backup").join("placeholder_a.bin.txt")).unwrap();

	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();
	wait_for(&project, &mut snapshot, "the missing placeholder", |s| {
		!s.issues.is_empty() && scans_completed(s) == 1
	});
	let issue = snapshot.issues[0].id;
	fs::write(&canonical, data(2)).unwrap();
	project.send(Command::Rescan);
	wait_for(&project, &mut snapshot, "the change", |s| scans_completed(s) == 2);
	project.send(Command::Resolve(issue, Verb::Accept));
	drop(project);

	run_to_completion(&setup, &encoder);

	assert_eq!(fs::read(setup.destination.join("a.bin")).unwrap(), data(1));
	assert_eq!(archived_content(&setup, "Backup/a.bin"), data(1));
	assert_eq!(archived_content(&setup, "a.bin"), data(2));
}

#[test]
fn a_canonical_copy_touched_then_changed_never_costs_its_duplicate_the_content() {
	let setup = setup(|_| {});
	let canonical = write(&setup.source, "a.bin", &data(1));
	let duplicate = write(&setup.source, "Backup/a.bin", &data(1));
	let encoder = FakeEncoder::new();
	drop(run_to_completion(&setup, &encoder));

	fs::write(&canonical, data(1)).unwrap();
	drop(run_to_completion(&setup, &encoder));
	assert_eq!(archived_content(&setup, "Backup/a.bin"), data(1));

	fs::write(&canonical, data(2)).unwrap();
	drop(run_to_completion(&setup, &encoder));
	fs::remove_file(duplicate).unwrap();
	run_to_completion(&setup, &encoder);

	assert!(matches!(record(&setup, "Backup/a.bin").state, FileState::SourceGone(_)));
	assert_eq!(archived_content(&setup, "Backup/a.bin"), data(1));
	assert_eq!(archived_content(&setup, "a.bin"), data(2));
}

#[test]
fn a_placeholder_whose_gone_canonical_copy_is_replaced_by_a_new_file_keeps_its_content_when_it_vanishes() {
	let setup = setup(|_| {});
	let canonical = write(&setup.source, "a.bin", &data(1));
	let duplicate = write(&setup.source, "Backup/a.bin", &data(1));
	let encoder = FakeEncoder::new();
	drop(run_to_completion(&setup, &encoder));
	fs::remove_file(&canonical).unwrap();
	drop(run_to_completion(&setup, &encoder));
	assert!(matches!(record(&setup, "a.bin").state, FileState::SourceGone(_)));

	fs::write(&canonical, data(2)).unwrap();
	fs::remove_file(duplicate).unwrap();
	run_to_completion(&setup, &encoder);

	assert_the_gone_duplicate_kept_the_old_content(&setup);
}

#[test]
fn a_redo_of_a_missing_output_never_hands_it_to_a_gone_placeholder() {
	let setup = setup(|_| {});
	write(&setup.source, "a.bin", &data(1));
	let duplicate = write(&setup.source, "Backup/a.bin", &data(1));
	let encoder = FakeEncoder::new();
	drop(run_to_completion(&setup, &encoder));
	fs::remove_file(duplicate).unwrap();
	drop(run_to_completion(&setup, &encoder));
	fs::remove_file(setup.destination.join("a.bin")).unwrap();

	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();
	wait_for(&project, &mut snapshot, "the missing output", |s| !s.issues.is_empty());
	project.send(Command::Resolve(snapshot.issues[0].id, Verb::Retry));
	project.send(Command::Start {
		delete_confirmed: false,
	});
	wait_for(&project, &mut snapshot, "the run to finish", |s| {
		s.overall.as_ref().is_some_and(|o| o.parity.is_some())
	});
	drop(project);

	assert_eq!(fs::read(setup.destination.join("a.bin")).unwrap(), data(1));
	assert_eq!(record(&setup, "a.bin").state.output().unwrap().path.display(), "a.bin");
	assert!(setup.destination.join("Backup").join("placeholder_a.bin.txt").exists());
	let FileState::SourceGone(kept) = record(&setup, "Backup/a.bin").state else {
		panic!("the gone duplicate lost its record")
	};
	assert_eq!(
		kept.how,
		How::Placeholder {
			canonical: RelPath::from_bytes(b"a.bin".to_vec()),
			lost: true,
		}
	);
}

/// Every placeholder's canonical copy is in the project and holds an archived file that is there.
fn assert_no_record_points_at_a_missing_canonical_copy(setup: &support::Setup) {
	let (_, _, loaded) = Store::open(&setup.project).unwrap();
	let records: HashMap<String, FileRecord> = loaded
		.files
		.into_iter()
		.map(|(key, record)| (key.display(), record))
		.collect();

	for (path, record) in &records {
		let recorded = match &record.state {
			FileState::Issue { output, .. } => output.as_ref(),
			state => state.output(),
		};
		let Some(How::Placeholder { canonical, lost: false }) = recorded.map(|output| &output.how) else {
			continue;
		};

		let held = records
			.get(&canonical.display())
			.and_then(|canonical| canonical.state.output())
			.filter(|output| !matches!(output.how, How::Placeholder { .. }))
			.unwrap_or_else(|| panic!("{path} points at {}, which holds no archived file", canonical.display()));
		assert!(
			setup.destination.join(native(&held.path.display())).exists(),
			"{path} points at a missing {}",
			held.path.display()
		);
	}
}

/// `lose` changes or removes the canonical copy's archived `output` after a run, and the user asks for a Redo. Returns
/// once the canonical copy passed its ready check, which holds its source open, so the duplicate `Backup/<duplicate>`
/// waits for it again.
fn redo_a_lost_canonical_output(
	setup: &support::Setup,
	encoder: &Arc<FakeEncoder>,
	output: &str,
	duplicate: &str,
	lose: impl FnOnce(&Path),
) -> (Project, Snapshot) {
	drop(run_to_completion(setup, encoder));
	lose(&setup.destination.join(output));

	let project = support::open(setup, encoder);
	let mut snapshot = Snapshot::default();
	wait_for(&project, &mut snapshot, "the lost output", |s| {
		!s.issues.is_empty() && scans_completed(s) == 1
	});
	project.send(Command::Resolve(snapshot.issues[0].id, Verb::Retry));
	wait_for(&project, &mut snapshot, "the redo", |s| s.issues.is_empty());

	let backup = snapshot.folder_named("Backup");
	project.send(Command::Watch(backup, true));
	wait_for(&project, &mut snapshot, "the duplicate to wait", |s| {
		s.rows
			.get(&backup)
			.and_then(|rows| rows.iter().find(|row| row.name == duplicate))
			.is_some_and(|row| row.status.state == RowState::Waiting)
	});
	(project, snapshot)
}

fn finish(project: &Project, snapshot: &mut Snapshot) {
	project.send(Command::Start {
		delete_confirmed: false,
	});
	wait_for(project, snapshot, "the run to finish", |s| {
		s.overall.as_ref().is_some_and(|o| o.parity.is_some())
	});
}

#[test]
fn a_redo_of_a_changed_canonical_output_then_a_source_change_leaves_the_duplicate_its_own_content() {
	let setup = setup(|_| {});
	let canonical = write(&setup.source, "a.bin", &data(1));
	write(&setup.source, "Backup/a.bin", &data(1));
	let encoder = FakeEncoder::new();
	let (project, mut snapshot) = redo_a_lost_canonical_output(&setup, &encoder, "a.bin", "a.bin", |path| {
		fs::write(path, b"changed").unwrap();
	});

	fs::write(&canonical, data(2)).unwrap();
	project.send(Command::Rescan);
	wait_for(&project, &mut snapshot, "the change", |s| scans_completed(s) == 2);
	finish(&project, &mut snapshot);
	drop(project);

	assert_no_record_points_at_a_missing_canonical_copy(&setup);
	assert_eq!(archived_content(&setup, "Backup/a.bin"), data(1));
	assert_eq!(archived_content(&setup, "a.bin"), data(2));
}

#[test]
fn a_redo_of_a_missing_canonical_output_whose_source_then_vanishes_leaves_the_duplicate_its_own_content() {
	let setup = setup(|_| {});
	let canonical = write(&setup.source, "a.bin", &data(1));
	write(&setup.source, "Backup/a.bin", &data(1));
	let encoder = FakeEncoder::new();
	let (project, mut snapshot) = redo_a_lost_canonical_output(&setup, &encoder, "a.bin", "a.bin", |path| {
		fs::remove_file(path).unwrap();
	});

	fs::remove_file(canonical).unwrap();
	project.send(Command::Rescan);
	wait_for(&project, &mut snapshot, "the removal", |s| scans_completed(s) == 2);
	finish(&project, &mut snapshot);
	drop(project);

	assert_no_record_points_at_a_missing_canonical_copy(&setup);
	assert_eq!(archived_content(&setup, "Backup/a.bin"), data(1));
}

#[test]
fn a_redo_that_fails_then_is_excluded_leaves_the_duplicate_its_own_content() {
	let setup = setup(|_| {});
	write(&setup.source, "a.jpg", &jpeg(4, BIG));
	write(&setup.source, "Backup/b.jpg", &jpeg(4, BIG));
	let encoder = FakeEncoder::new();
	let (project, mut snapshot) = redo_a_lost_canonical_output(&setup, &encoder, "a.avif", "b.jpg", |path| {
		assert!(path.with_file_name("Backup").join("placeholder_b.jpg.txt").exists());
		encoder.rule("a.jpg", Rule::Fail(ErrorKind::SourceDamaged("broken".into())));
		fs::remove_file(path).unwrap();
	});

	project.send(Command::Start {
		delete_confirmed: false,
	});
	wait_for(&project, &mut snapshot, "the failed redo", |s| {
		s.issues.iter().any(|issue| issue.problem == Problem::SourceDamaged)
	});
	project.send(Command::Resolve(snapshot.issues[0].id, Verb::Exclude));
	wait_for(&project, &mut snapshot, "the run to finish", |s| {
		s.overall
			.as_ref()
			.and_then(|overall| overall.parity)
			.is_some_and(|parity| parity.excluded == 1)
	});
	drop(project);

	assert_no_record_points_at_a_missing_canonical_copy(&setup);
	assert_eq!(archived_content(&setup, "Backup/b.jpg"), b"encoded b.jpg");
}

#[test]
fn accepting_a_missing_canonical_output_leaves_the_duplicate_its_own_content() {
	let setup = setup(|_| {});
	write(&setup.source, "a.bin", &data(1));
	write(&setup.source, "Backup/a.bin", &data(1));
	let encoder = FakeEncoder::new();
	drop(run_to_completion(&setup, &encoder));
	fs::remove_file(setup.destination.join("a.bin")).unwrap();

	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();
	wait_for(&project, &mut snapshot, "the missing output", |s| {
		!s.issues.is_empty() && scans_completed(s) == 1
	});
	project.send(Command::Resolve(snapshot.issues[0].id, Verb::Accept));
	wait_for(&project, &mut snapshot, "the accept", |s| s.issues.is_empty());
	finish(&project, &mut snapshot);
	drop(project);

	assert_eq!(record(&setup, "a.bin").state, FileState::Excluded);
	assert_no_record_points_at_a_missing_canonical_copy(&setup);
	assert_eq!(archived_content(&setup, "Backup/a.bin"), data(1));
}

#[test]
fn accepting_a_changed_canonical_output_leaves_the_duplicate_its_own_content() {
	let setup = setup(|_| {});
	write(&setup.source, "a.bin", &data(1));
	write(&setup.source, "Backup/a.bin", &data(1));
	let encoder = FakeEncoder::new();
	drop(run_to_completion(&setup, &encoder));
	fs::write(setup.destination.join("a.bin"), b"changed").unwrap();

	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();
	wait_for(&project, &mut snapshot, "the changed output", |s| {
		!s.issues.is_empty() && scans_completed(s) == 1
	});
	project.send(Command::Resolve(snapshot.issues[0].id, Verb::Accept));
	wait_for(&project, &mut snapshot, "the accept", |s| s.issues.is_empty());
	finish(&project, &mut snapshot);
	drop(project);

	assert_no_record_points_at_a_missing_canonical_copy(&setup);
	assert_eq!(archived_content(&setup, "Backup/a.bin"), data(1));
	assert_eq!(archived_content(&setup, "a.bin"), b"changed");
}
