use cryarchiver_tui::view::{Entry, FileState, RunState};
use cryarchiver_tui::{Action, ArchiveSource, MockSource};

fn stepped(seed: u64, steps: usize) -> MockSource {
	let mut source = MockSource::new(seed);
	for _ in 0..steps {
		source.step();
	}
	source
}

#[test]
fn same_seed_and_steps_give_the_same_view() {
	assert_eq!(stepped(7, 40).view(), stepped(7, 40).view());
	assert_eq!(stepped(7, 40).details(stepped(7, 40).view().workers.jobs[0].file), {
		let source = stepped(7, 40);
		source.details(source.view().workers.jobs[0].file)
	});
}

#[test]
fn different_seeds_or_steps_diverge() {
	assert_ne!(stepped(7, 40).view(), stepped(8, 40).view());
	assert_ne!(stepped(7, 40).view(), stepped(7, 41).view());
}

#[test]
fn folders_roll_up_their_whole_subtree() {
	let source = stepped(3, 25);
	let view = source.view();
	let library = &view.library;

	let files = library.ids().filter(|&id| !library.is_folder(id)).count() as u64;
	let root = library.folder(library.root()).unwrap();
	assert_eq!(root.files_total, files);
	assert_eq!(view.overall.files_total, files);

	for id in library.ids().filter(|&id| library.is_folder(id)) {
		let stats = library.folder(id).unwrap();
		let children_total: u64 = library
			.node(id)
			.children
			.iter()
			.map(|&child| match &library.node(child).entry {
				Entry::Folder(child_stats) => child_stats.files_total,
				Entry::File(_) => 1,
			})
			.sum();
		assert_eq!(stats.files_total, children_total, "{}", library.node(id).name);
		assert!(stats.files_done <= stats.files_total);
	}
}

#[test]
fn open_issues_keep_folders_below_complete() {
	let source = MockSource::new(1);
	let library = &source.view().library;
	let issue = &source.view().issues[0];
	let folder = library.node(issue.file).parent.unwrap();
	let stats = library.folder(folder).unwrap();

	assert!(stats.open_issues > 0);
	assert!(!stats.complete);
	assert!(source.view().overall.progress < 1.0);
}

#[test]
fn the_scan_grows_the_total_until_it_finishes() {
	let start = MockSource::new(5);
	let later = stepped(5, 30);
	assert!(later.view().overall.files_total > start.view().overall.files_total);
	assert!(!later.view().overall.scan_complete);

	let finished = stepped(5, 400);
	assert!(finished.view().overall.scan_complete);
	assert!(finished.view().library.structure_version() > start.view().library.structure_version());
}

#[test]
fn workers_progress_and_finish_files() {
	let start = MockSource::new(9);
	let later = stepped(9, 60);

	assert!(later.view().overall.files_done > start.view().overall.files_done);
	assert!(!later.view().workers.jobs.is_empty());
	assert!(later.view().log.len() > start.view().log.len());
	assert!(later.view().workers.jobs.iter().all(|job| job.state.is_active()));
}

#[test]
fn a_long_run_produces_new_failures() {
	let source = stepped(11, 400);

	assert!(source.view().issues.len() > 4);
}

#[test]
fn retry_requeues_and_copy_or_exclude_resolve() {
	let mut source = MockSource::new(2);
	let issues: Vec<_> = source.view().issues.iter().map(|issue| issue.file).collect();

	source.perform(Action::Retry(issues[0]));
	assert_eq!(source.view().library.file(issues[0]).unwrap().state, FileState::Queued);

	source.perform(Action::CopyOriginal(issues[1]));
	assert_eq!(source.view().library.file(issues[1]).unwrap().state, FileState::Copied);

	let done_before = source.view().overall.files_done;
	source.perform(Action::Exclude(issues[2]));
	assert_eq!(source.view().overall.files_done, done_before + 1);
	assert_eq!(source.view().issues.len(), 1);
}

#[test]
fn pause_freezes_progress() {
	let mut source = stepped(4, 5);
	source.perform(Action::TogglePause);
	let paused = source.view().overall.clone();

	for _ in 0..20 {
		source.step();
	}

	assert_eq!(source.view().header.run_state, RunState::Paused);
	assert_eq!(source.view().overall.files_done, paused.files_done);
	assert_eq!(source.view().overall.files_total, paused.files_total);
}

#[test]
fn completed_run_reports_parity() {
	let source = MockSource::completed(6);
	let overall = &source.view().overall;
	let parity = overall.parity.expect("a finished run has a parity line");

	assert_eq!(source.view().header.run_state, RunState::Finished);
	assert_eq!(parity.source_files, overall.files_total);
	assert_eq!(parity.archive_files + parity.excluded, parity.source_files);
	assert_eq!(parity.excluded, 1);
	assert!(
		source
			.view()
			.library
			.folder(source.view().library.root())
			.unwrap()
			.complete
	);
}
