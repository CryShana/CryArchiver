mod support;

use std::fs;
use std::path::MAIN_SEPARATOR_STR;

use cryarchiver_engine::outcome::ErrorKind;
use cryarchiver_project::issues::Verb;
use cryarchiver_project::keys::RelPath;
use cryarchiver_project::record::{FileRecord, FileState, How};
use cryarchiver_project::store::Store;
use cryarchiver_project::{Command, Fault};
use support::{FakeEncoder, Rule, Snapshot, jpeg, setup, wait_for, write};

const BIG: usize = 100_000;

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
