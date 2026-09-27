mod support;

use std::fs;
use std::path::MAIN_SEPARATOR_STR;

use cryarchiver_engine::outcome::ErrorKind;
use cryarchiver_project::issues::Verb;
use cryarchiver_project::keys::RelPath;
use cryarchiver_project::record::{CopyReason, FileRecord, FileState, How};
use cryarchiver_project::store::Store;
use cryarchiver_project::tree::FolderId;
use cryarchiver_project::view::RowState;
use cryarchiver_project::{Command, Fault};
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
			canonical: RelPath::from_bytes(b"a.jpg".to_vec())
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

#[test]
fn a_changed_canonical_copy_lets_its_placeholders_archive_their_own_content() {
	let setup = setup(|_| {});
	let canonical = write(&setup.source, "a.bin", &data(1));
	write(&setup.source, "Backup/a.bin", &data(1));
	let encoder = FakeEncoder::new();
	drop(run_to_completion(&setup, &encoder));
	assert!(setup.destination.join("Backup").join("placeholder_a.bin.txt").exists());

	fs::write(&canonical, data(2)).unwrap();
	run_to_completion(&setup, &encoder);

	assert_eq!(fs::read(setup.destination.join("a.bin")).unwrap(), data(2));
	assert_eq!(
		fs::read(setup.destination.join("Backup").join("a.bin")).unwrap(),
		data(1)
	);
	assert!(!setup.destination.join("Backup").join("placeholder_a.bin.txt").exists());
	assert_eq!(
		record(&setup, "Backup/a.bin").state.output().unwrap().how,
		How::Copied(CopyReason::NotMedia)
	);
}

fn assert_the_gone_duplicate_kept_the_old_content(setup: &support::Setup) {
	assert_eq!(fs::read(setup.destination.join("a.bin")).unwrap(), data(1));
	assert_eq!(fs::read(setup.destination.join("a (2).bin")).unwrap(), data(2));
	assert!(!setup.destination.join("Backup").join("placeholder_a.bin.txt").exists());

	let FileState::SourceGone(kept) = record(setup, "Backup/a.bin").state else {
		panic!("the gone duplicate has no output of its own")
	};
	assert_eq!(kept.path.display(), "a.bin");
	assert_eq!(kept.how, How::Copied(CopyReason::NotMedia));
	assert_eq!(
		record(setup, "a.bin").state.output().unwrap().path.display(),
		"a (2).bin"
	);
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
