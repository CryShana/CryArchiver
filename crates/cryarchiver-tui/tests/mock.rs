use cryarchiver_tui::source::SettingsChange;
use cryarchiver_tui::view::{FileState, FilterEntry, IssueKind, Limit, NodeId, RunState, Verb, WorkKind};
use cryarchiver_tui::{Action, ArchiveSource, MockSource};

fn stepped(seed: u64, steps: usize) -> MockSource {
	let mut source = MockSource::new(seed);
	for _ in 0..steps {
		source.step();
	}
	source
}

fn expand_all(source: &mut MockSource) {
	let folders: Vec<NodeId> = source.view().library.ids().collect();
	for folder in folders {
		source.perform(Action::Expanded(folder, true));
	}
}

fn find_folder(source: &MockSource, name: &str) -> NodeId {
	let library = &source.view().library;
	library.ids().find(|&id| library.node(id).name == name).unwrap()
}

#[test]
fn same_seed_and_steps_give_the_same_view() {
	let first = stepped(7, 40);
	let second = stepped(7, 40);
	let file = first.view().workers.jobs[0].file;

	assert_eq!(first.view(), second.view());
	assert_eq!(first.details(file), second.details(file));
}

#[test]
fn different_seeds_or_steps_diverge() {
	assert_ne!(stepped(7, 40).view(), stepped(8, 40).view());
	assert_ne!(stepped(7, 40).view(), stepped(7, 41).view());
}

#[test]
fn file_rows_exist_only_for_expanded_folders() {
	let mut source = MockSource::new(3);
	let bled = find_folder(&source, "Bled");
	assert!(source.files(bled).is_none());

	let version = source.rows_version();
	source.perform(Action::Expanded(bled, true));
	assert!(source.files(bled).is_some_and(|rows| rows.len() == 31));
	assert!(source.rows_version() > version);

	source.perform(Action::Expanded(bled, false));
	assert!(source.files(bled).is_none());
}

#[test]
fn a_slow_source_loads_rows_on_a_later_tick() {
	let mut source = MockSource::new(3).with_load_latency(2);
	let bled = find_folder(&source, "Bled");

	source.perform(Action::Expanded(bled, true));
	source.step();
	assert!(source.files(bled).is_none());

	source.step();
	assert!(source.files(bled).is_some());
}

#[test]
fn folders_roll_up_their_whole_subtree() {
	let mut source = stepped(3, 25);
	expand_all(&mut source);
	let view = source.view();
	let library = &view.library;

	let files: u64 = library
		.ids()
		.map(|folder| source.files(folder).unwrap().len() as u64)
		.sum();
	assert_eq!(library.stats(library.root()).files_total, files);
	assert_eq!(view.overall.files_total, files);

	for folder in library.ids() {
		let stats = library.stats(folder);
		let listed = source.files(folder).unwrap().len() as u64;
		let below: u64 = library
			.node(folder)
			.children
			.iter()
			.map(|&child| library.stats(child).files_total)
			.sum();
		assert_eq!(stats.files_total, listed + below, "{}", library.node(folder).name);
		assert!(stats.files_done <= stats.files_total);
	}
}

#[test]
fn filter_returns_matches_with_their_ancestors() {
	let mut source = MockSource::new(3);
	source.perform(Action::SetFilter("テスト".to_owned()));
	let result = source.filter().unwrap();

	assert_eq!(result.matching_files, 1);
	assert_eq!(result.rows.len(), 4);
	assert!(matches!(&result.rows[3].entry, FilterEntry::File { row, .. } if row.name == "01.テスト.mp3"));

	source.perform(Action::SetFilter(String::new()));
	assert!(source.filter().is_none());
}

#[test]
fn open_issues_keep_folders_below_complete() {
	let source = MockSource::new(1);
	let library = &source.view().library;
	let issue = &source.view().issues[0];
	let stats = library.stats(issue.folder.unwrap());

	assert!(stats.open_issues > 0);
	assert!(!stats.complete);
	assert!(source.view().overall.progress < 1.0);
}

#[test]
fn issue_kinds_carry_their_verbs() {
	let source = MockSource::new(1);
	let issue = |kind| source.view().issues.iter().find(|issue| issue.kind == kind).unwrap();

	let damaged = issue(IssueKind::SourceDamaged);
	assert_eq!(damaged.verbs.accept.as_deref(), Some("Copy original"));
	assert!(damaged.verbs.exclude);

	let missing = issue(IssueKind::OutputMissing);
	assert_eq!(missing.verbs.retry.as_deref(), Some("Redo"));
	assert_eq!(missing.verbs.accept.as_deref(), Some("Accept as is"));
	assert!(!missing.verbs.exclude);

	let unknown = issue(IssueKind::UnknownFile);
	assert_eq!(unknown.file, None);
	assert_eq!(unknown.verbs.retry, None);
	assert_eq!(unknown.verbs.accept.as_deref(), Some("Acknowledge"));

	let cycle = issue(IssueKind::DirectoryCycle);
	assert_eq!(cycle.file, None);

	let failed = issue(IssueKind::EncodeFailed);
	assert_eq!(failed.attempts.len(), 3);
}

#[test]
fn resolving_follows_the_verb() {
	let mut source = MockSource::new(2);
	let id_of = |source: &MockSource, kind| source.view().issues.iter().find(|issue| issue.kind == kind).unwrap().id;
	let file_of = |source: &MockSource, kind| {
		source
			.view()
			.issues
			.iter()
			.find(|issue| issue.kind == kind)
			.unwrap()
			.file
			.unwrap()
	};
	let state = |source: &MockSource, file| {
		let folder = find_folder_of(source, file);
		source
			.files(folder)
			.unwrap()
			.iter()
			.find(|row| row.id == file)
			.unwrap()
			.status
			.state
	};
	expand_all(&mut source);

	let damaged = file_of(&source, IssueKind::SourceDamaged);
	source.perform(Action::Resolve(id_of(&source, IssueKind::SourceDamaged), Verb::Retry));
	assert_eq!(state(&source, damaged), FileState::Queued);

	let failed = file_of(&source, IssueKind::EncodeFailed);
	let done_before = source.view().overall.files_done;
	source.perform(Action::Resolve(id_of(&source, IssueKind::EncodeFailed), Verb::Exclude));
	assert_eq!(state(&source, failed), FileState::Excluded);
	assert_eq!(source.view().overall.files_done, done_before + 1);

	let missing = id_of(&source, IssueKind::OutputMissing);
	let issues = source.view().issues.len();
	source.perform(Action::Resolve(missing, Verb::Exclude));
	assert_eq!(
		source.view().issues.len(),
		issues,
		"exclude isn't offered for a missing output"
	);

	source.perform(Action::Resolve(id_of(&source, IssueKind::UnknownFile), Verb::Accept));
	assert_eq!(source.view().issues.len(), issues - 1);
}

fn find_folder_of(source: &MockSource, file: cryarchiver_tui::view::FileId) -> NodeId {
	let library = &source.view().library;
	library
		.ids()
		.find(|&folder| {
			source
				.files(folder)
				.is_some_and(|rows| rows.iter().any(|row| row.id == file))
		})
		.unwrap()
}

#[test]
fn delete_originals_needs_the_typed_confirmation() {
	let mut source = MockSource::new(4);
	let refuse = SettingsChange::DeleteOriginals {
		enabled: true,
		confirmation: "delete".to_owned(),
	};
	source.perform(Action::ChangeSettings(refuse));
	assert!(!source.view().header.deleting_originals);
	assert_eq!(source.take_notifications().len(), 1);

	let confirm = SettingsChange::DeleteOriginals {
		enabled: true,
		confirmation: "DELETE".to_owned(),
	};
	source.perform(Action::ChangeSettings(confirm));
	assert!(source.view().header.deleting_originals);
	assert!(source.view().settings.delete_originals);
}

#[test]
fn deleting_originals_runs_delete_jobs() {
	let mut source = MockSource::new(4);
	let confirm = SettingsChange::DeleteOriginals {
		enabled: true,
		confirmation: "DELETE".to_owned(),
	};
	source.perform(Action::ChangeSettings(confirm));
	source.step();

	assert!(
		source
			.view()
			.workers
			.jobs
			.iter()
			.any(|job| job.work == WorkKind::Delete)
	);
	for _ in 0..4 {
		source.step();
	}
	assert!(
		source
			.view()
			.log
			.newest_first()
			.any(|line| line.message.starts_with("original deleted"))
	);
}

#[test]
fn a_new_limit_is_pending_until_restart() {
	let mut source = MockSource::new(5);
	source.perform(Action::ChangeSettings(SettingsChange::PerformanceLimit(Some(25))));

	assert_eq!(
		source.view().header.performance_limit,
		Some(Limit { cap: 25, pending: true })
	);
	let over = source.view().workers.jobs.iter().filter(|job| job.over_limit).count();
	assert_eq!(over, source.view().workers.jobs.len() - 1);

	source.perform(Action::RestartPendingJobs);
	assert_eq!(
		source.view().header.performance_limit,
		Some(Limit {
			cap: 25,
			pending: false
		})
	);
	assert_eq!(source.view().workers.jobs.len(), 1);

	source.perform(Action::ChangeSettings(SettingsChange::PerformanceLimit(None)));
	assert_eq!(source.view().header.performance_limit, None);
}

#[test]
fn low_disk_space_pauses_the_whole_run() {
	let paused = stepped(6, 85);
	assert_eq!(paused.view().header.run_state, RunState::PausedLowDisk);

	let resumed = stepped(6, 125);
	assert_eq!(resumed.view().header.run_state, RunState::Running);
}

#[test]
fn watch_mode_idles_instead_of_finishing() {
	let mut source = MockSource::completed(6);
	assert_eq!(source.view().header.run_state, RunState::Finished);

	source.perform(Action::ChangeSettings(SettingsChange::Watch(true)));
	assert_eq!(source.view().header.run_state, RunState::Watching);
}

#[test]
fn pause_start_and_stop_now() {
	let mut source = stepped(4, 5);
	source.perform(Action::Pause);
	let paused = source.view().overall.clone();
	for _ in 0..20 {
		source.step();
	}
	assert_eq!(source.view().header.run_state, RunState::Paused);
	assert_eq!(source.view().overall.files_done, paused.files_done);

	source.perform(Action::Start);
	assert_eq!(source.view().header.run_state, RunState::Running);

	source.perform(Action::StopNow);
	assert_eq!(source.view().header.run_state, RunState::Stopping);
	assert!(source.view().workers.jobs.is_empty());
}

#[test]
fn the_scan_grows_the_total_until_it_finishes() {
	let start = MockSource::new(5);
	let later = stepped(5, 30);
	assert!(later.view().overall.files_total > start.view().overall.files_total);
	assert!(!later.view().overall.scan_complete);

	let finished = stepped(5, 400);
	assert!(finished.view().overall.scan_complete);
}

#[test]
fn workers_progress_and_finish_files() {
	let start = MockSource::new(9);
	let later = stepped(9, 60);

	assert!(later.view().overall.files_done > start.view().overall.files_done);
	assert!(!later.view().workers.jobs.is_empty());
	assert!(later.view().log.len() > start.view().log.len());
}

#[test]
fn a_long_run_produces_new_failures() {
	let source = stepped(11, 400);

	assert!(source.view().issues.len() > 6);
}

#[test]
fn completed_run_reports_parity() {
	let source = MockSource::completed(6);
	let overall = &source.view().overall;
	let parity = overall.parity.expect("a finished run has a parity line");

	assert_eq!(parity.source_files, overall.files_total);
	assert_eq!(parity.archive_files + parity.excluded, parity.source_files);
	assert_eq!(parity.excluded, 2);
	assert!(source.view().library.stats(source.view().library.root()).complete);
}
