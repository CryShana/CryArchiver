//! The performance limit changes this process's priority and CPU rate, so these tests live in their own binary.

mod support;

use cryarchiver_project::Command;
use cryarchiver_project::settings::SettingsChange;
use cryarchiver_project::tree::FolderId;
use cryarchiver_project::view::RowState;
use support::{FakeEncoder, Rule, Snapshot, jpeg, setup, wait_for, write};

#[test]
fn the_performance_limit_runs_one_job_at_a_time_and_restart_applies_it_to_running_jobs() {
	let setup = setup(|_| {});
	for name in ["a.jpg", "b.jpg", "c.jpg"] {
		write(&setup.source, name, &jpeg(name.as_bytes()[0], 100));
	}
	let encoder = FakeEncoder::new();
	encoder.rule(".jpg", Rule::Hold);
	let project = support::open(&setup, &encoder);
	let mut snapshot = Snapshot::default();
	project.send(Command::Start {
		delete_confirmed: false,
	});
	wait_for(&project, &mut snapshot, "three jobs", |s| s.workers.len() == 3);

	project.send(Command::ChangeSettings(SettingsChange::PerformanceLimit(Some(25))));
	wait_for(&project, &mut snapshot, "the pending limit", |s| {
		s.status.as_ref().is_some_and(|status| status.limit_pending)
			&& s.workers.iter().filter(|worker| worker.over_limit).count() == 2
	});

	project.send(Command::Watch(FolderId::ROOT, true));
	wait_for(&project, &mut snapshot, "the rows", |s| {
		s.rows.contains_key(&FolderId::ROOT)
	});
	project.send(Command::RestartPendingJobs);
	wait_for(
		&project,
		&mut snapshot,
		"two jobs requeued and one still running",
		|s| {
			let states: Vec<RowState> = s.rows[&FolderId::ROOT].iter().map(|row| row.status.state).collect();
			let count = |state| states.iter().filter(|&&found| found == state).count();
			count(RowState::Queued) == 2
				&& count(RowState::Encoding) == 1
				&& s.workers.len() == 1
				&& s.status.as_ref().is_some_and(|status| !status.limit_pending)
		},
	);
	let queued = snapshot.rows[&FolderId::ROOT]
		.iter()
		.find(|row| row.status.state == RowState::Queued)
		.unwrap()
		.id;
	project.send(Command::Details(queued));
	wait_for(
		&project,
		&mut snapshot,
		"the project to answer after the requeue",
		|s| s.details.contains_key(&queued),
	);
	support::drain(&project, &mut snapshot);
	assert_eq!(snapshot.workers.len(), 1);
	assert_eq!(encoder.calls().len(), 3, "no requeued job started again");
	wait_for(&project, &mut snapshot, "low priority", |s| {
		s.system.as_ref().is_some_and(|system| system.low_priority)
	});

	project.send(Command::ChangeSettings(SettingsChange::PerformanceLimit(None)));
	wait_for(&project, &mut snapshot, "all jobs again", |s| s.workers.len() == 3);
}
