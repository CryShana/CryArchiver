use std::collections::BTreeMap;
use std::time::Duration;

use super::sim::PROBES;
use super::{HISTORY_LEN, Job, MediaKind, MockSource, Phase, SAVINGS_WINDOW};
use crate::format::{as_f64, fraction};
use crate::view::{FileId, FileState, FileStatus, JobActivity, NodeId, Parity, RunState, WorkKind, WorkerJob};

#[derive(Debug, Default, Clone, Copy)]
struct Tally {
	done: u64,
	total: u64,
	partial: f64,
	bytes_in: u64,
	bytes_out: u64,
	issues: u32,
	unscanned: bool,
}

impl Tally {
	fn add(&mut self, other: &Self) {
		self.done += other.done;
		self.total += other.total;
		self.partial += other.partial;
		self.bytes_in += other.bytes_in;
		self.bytes_out += other.bytes_out;
		self.issues += other.issues;
		self.unscanned |= other.unscanned;
	}

	fn complete(&self) -> bool {
		self.done == self.total && self.issues == 0 && !self.unscanned
	}

	fn progress(&self) -> f64 {
		if self.total == 0 {
			return 1.0;
		}

		((as_f64(self.done) + self.partial) / as_f64(self.total)).clamp(0.0, 1.0)
	}
}

#[derive(Debug, Default)]
struct Counts {
	queued: u64,
	kept: u64,
	duplicates: u64,
	excluded: u64,
}

impl MockSource {
	pub(super) fn summarize(&mut self) {
		let (tallies, counts) = self.tally();
		self.write_folder_stats(&tallies);

		let root = tallies[self.view.library.root().index()];
		self.write_overall(&root, &counts);
		self.write_workers();
	}

	pub(super) fn record_savings(&mut self, saved_before: u64) {
		let delta = self.view.overall.saved_bytes.saturating_sub(saved_before) / 1_000_000;
		self.recent_savings.push_back(delta);
		if self.recent_savings.len() > SAVINGS_WINDOW {
			self.recent_savings.pop_front();
		}

		let window = self.recent_savings.iter().sum();
		push_history(&mut self.view.overall.savings_history, window);
	}

	fn tally(&self) -> (Vec<Tally>, Counts) {
		let library = &self.view.library;
		let mut tallies = vec![Tally::default(); library.len()];
		let mut counts = Counts::default();

		let mut issues_by_folder: BTreeMap<NodeId, u32> = BTreeMap::new();
		for folder in self.view.issues.iter().filter_map(|issue| issue.folder) {
			*issues_by_folder.entry(folder).or_default() += 1;
		}
		let open_files: Vec<FileId> = self.view.issues.iter().filter_map(|issue| issue.file).collect();

		for file in self.files.ids() {
			let status = &self.files.get(file).status;
			let own = file_tally(status, open_files.contains(&file), &mut counts);
			tallies[self.files.folder(file).index()].add(&own);
		}

		for folder in library.ids().rev() {
			let tally = &mut tallies[folder.index()];
			tally.issues += issues_by_folder.get(&folder).copied().unwrap_or(0);
			tally.unscanned |= folder == self.scan_target && self.scan_remaining > 0;

			let own = *tally;
			if let Some(parent) = library.node(folder).parent {
				tallies[parent.index()].add(&own);
			}
		}

		(tallies, counts)
	}

	fn write_folder_stats(&mut self, tallies: &[Tally]) {
		for folder in self.view.library.ids() {
			let tally = tallies[folder.index()];
			let stats = &mut self.view.library.node_mut(folder).stats;
			stats.files_done = tally.done;
			stats.files_total = tally.total;
			stats.progress = tally.progress();
			stats.bytes_in = tally.bytes_in;
			stats.bytes_out = tally.bytes_out;
			stats.open_issues = tally.issues;
			stats.complete = tally.complete();
		}
	}

	fn write_overall(&mut self, root: &Tally, counts: &Counts) {
		let active = self.jobs.len() as u64;
		let open_issues = self.view.issues.len() as u64;
		let idle = self.queue.is_empty() && self.jobs.is_empty();
		let scan_complete = self.scan_remaining == 0;
		let saved = root.bytes_in.saturating_sub(root.bytes_out);

		let header = &mut self.view.header;
		if idle
			&& scan_complete
			&& matches!(
				header.run_state,
				RunState::Running | RunState::Finished | RunState::Watching
			) {
			header.run_state = if self.view.settings.watch {
				RunState::Watching
			} else {
				RunState::Finished
			};
		}

		let overall = &mut self.view.overall;
		overall.files_done = root.done;
		overall.files_active = active;
		overall.files_queued = counts.queued;
		overall.files_total = root.total;
		overall.scan_complete = scan_complete;
		overall.progress = root.progress();
		overall.kept = counts.kept;
		overall.duplicates = counts.duplicates;
		overall.open_issues = open_issues;
		overall.saved_bytes = saved;
		overall.saved_fraction = fraction(saved, root.bytes_in);
		overall.eta = (!idle).then(|| Duration::from_secs(counts.queued * 6 / 5 + active * 20));
		overall.parity = (idle && scan_complete && open_issues == 0).then(|| Parity {
			source_files: root.total,
			archive_files: root.total - counts.excluded,
			excluded: counts.excluded,
		});
	}

	fn write_workers(&mut self) {
		let jobs = self
			.jobs
			.iter()
			.map(|job| {
				let kind = self.files.kind(job.file);
				let row = self.files.get(job.file);
				WorkerJob {
					file: job.file,
					folder: self.files.folder(job.file),
					work: job.work,
					codec: kind.codec().unwrap_or("copy").to_owned(),
					name: row.name.clone(),
					state: row.status.state,
					activity: activity(job, kind),
					over_limit: job.over_limit,
				}
			})
			.collect();
		self.view.workers.jobs = jobs;
	}
}

fn file_tally(status: &FileStatus, open_issue: bool, counts: &mut Counts) -> Tally {
	match status.state {
		FileState::Queued | FileState::Waiting => counts.queued += 1,
		FileState::KeptOriginal => counts.kept += 1,
		FileState::Duplicate => counts.duplicates += 1,
		FileState::Excluded => counts.excluded += 1,
		_ => {}
	}

	let finished = !open_issue
		&& matches!(
			status.state,
			FileState::Done
				| FileState::KeptOriginal
				| FileState::Copied
				| FileState::Duplicate
				| FileState::Warning
				| FileState::Excluded
				| FileState::SourceGone
				| FileState::DeletePending
				| FileState::OriginalDeleted
		);
	let written = status.bytes_out.filter(|_| finished);

	Tally {
		done: u64::from(finished),
		total: 1,
		partial: if status.state.is_active() {
			status.progress.unwrap_or(0.0)
		} else {
			0.0
		},
		bytes_in: written.map_or(0, |_| status.bytes_in),
		bytes_out: written.unwrap_or(0),
		issues: 0,
		unscanned: false,
	}
}

fn activity(job: &Job, kind: MediaKind) -> JobActivity {
	let encoding = job.work == WorkKind::Encode && kind.is_video();
	match job.phase {
		Phase::Search { probe } => {
			let (crf, score) = PROBES[(probe.clamp(1, 4) - 1) as usize];
			JobActivity::QualitySearch {
				probe,
				probes: 4,
				crf,
				metric: "VMAF".to_owned(),
				score,
			}
		}
		Phase::Encode { fraction } => JobActivity::Progress {
			fraction,
			fps: encoding.then_some(job.fps),
			speed: encoding.then_some(job.fps / 30.0),
		},
		Phase::Verify { fraction } => JobActivity::Progress {
			fraction: fraction.min(0.99),
			fps: None,
			speed: None,
		},
	}
}

pub(super) fn push_history(history: &mut Vec<u64>, value: u64) {
	history.push(value);
	if history.len() > HISTORY_LEN {
		history.remove(0);
	}
}
