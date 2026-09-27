//! Settings that change during a run (project layer spec §4). The performance limit applies at once wherever the OS
//! can adjust running processes; what can't reach running jobs (fewer jobs, the Linux thread fallback) is pending
//! until those jobs end or the user restarts them.

use std::collections::HashSet;
use std::time::Instant;

use cryarchiver_engine::commit::free_space;
use cryarchiver_engine::process::limits::{self, Applied, Limits};

use super::owner::Owner;
use crate::schedule::{least_progress_first, space_estimate};
use crate::settings::SettingsChange;
use crate::tree::{FileId, FolderId};
use crate::view::{LogKind, NoticeLevel, Update};

#[derive(Debug)]
pub(crate) struct LiveState {
	pub(super) applied: Applied,
	/// Jobs that started before the image thread count last changed; they keep their old count until they end.
	pub(super) stale: HashSet<FileId>,
	last_space_check: Option<Instant>,
	space_warned: bool,
}

impl LiveState {
	pub(crate) fn new(applied: Applied) -> Self {
		Self {
			applied,
			stale: HashSet::new(),
			last_space_check: None,
			space_warned: false,
		}
	}
}

/// Applies the project's limit to this process and the tools it runs. Called on open, before any tool starts.
pub(crate) fn apply_limit(cap: Option<u8>) -> Applied {
	limits::apply(Limits {
		low_priority: cap.is_some(),
		cpu_cap: cap,
	})
}

/// Image encoder threads for new jobs: the whole machine, or the capped share where the cap can't be applied live.
pub(crate) fn image_threads(cap: Option<u8>, applied: Applied) -> usize {
	let cores = std::thread::available_parallelism().map_or(1, usize::from);
	match cap {
		Some(cap) if !applied.cpu_cap_live => (cores * usize::from(cap) / 100).max(1),
		_ => cores,
	}
}

impl Owner {
	pub(super) fn change_settings(&mut self, change: &SettingsChange) {
		if let Err(err) = self.settings.apply(change.clone()) {
			self.notice(NoticeLevel::Warning, err.to_string());
			return;
		}

		if let Err(err) = self.store.set_settings(&self.settings) {
			self.fail(err);
		}
		self.send(Update::Settings(self.settings.clone()));

		match change {
			SettingsChange::PerformanceLimit(cap) => self.limit_changed(*cap),
			SettingsChange::DeleteOriginals { enabled: true, .. } => {
				self.delete_confirmed = true;
				self.log(LogKind::Action, "deleting originals is on".into());
				self.queue_all_deletes();
			}
			SettingsChange::DeleteOriginals { enabled: false, .. } => {
				self.delete_confirmed = false;
				self.log(LogKind::Action, "deleting originals is off".into());
				self.cancel_deletes();
			}
			SettingsChange::StabilityWindow(_) | SettingsChange::Watch(_) => self.view.status_dirty = true,
			SettingsChange::Speed(_) | SettingsChange::MinSavings(_) | SettingsChange::KeepAuxiliaryImages(_) => {}
		}
	}

	fn limit_changed(&mut self, cap: Option<u8>) {
		self.pipeline.governor.set_limit(cap.is_some());
		self.live.applied = apply_limit(cap);

		let threads = image_threads(cap, self.live.applied);
		if threads != self.pipeline.image_threads {
			self.pipeline.image_threads = threads;
			self.pipeline.encoder = self.backend.encoder(threads, self.pipeline.memory.clone());
			self.live.stale = self.pipeline.jobs.keys().copied().collect();
		}

		self.pipeline.workers_dirty = true;
		self.view.status_dirty = true;
	}

	/// Part of the limit waits for running jobs: more of them run than the limit allows, or their thread count can
	/// only change with a restart.
	pub(super) fn limit_pending(&self) -> bool {
		self.settings.performance_limit.is_some()
			&& (self.pipeline.governor.over_limit() > 0 || !self.live.stale.is_empty())
	}

	/// Restart now: cancels and requeues the jobs the limit wouldn't allow, least progress first (spec §8.4), and the
	/// jobs that still run with the old thread count.
	pub(super) fn restart_pending_jobs(&mut self) {
		if self.settings.performance_limit.is_none() {
			return;
		}

		let running: Vec<(FileId, f64)> = self
			.pipeline
			.jobs
			.iter()
			.map(|(file, job)| (*file, job.fraction))
			.collect();
		let mut restarted = least_progress_first(&running, 1);
		for file in &self.live.stale {
			if !restarted.contains(file) && self.pipeline.jobs.contains_key(file) {
				restarted.push(*file);
			}
		}

		self.log(
			LogKind::Action,
			format!("restarting {} jobs under the new limit", restarted.len()),
		);
		self.cancel_and_requeue(&restarted);
	}

	/// Warns when the rest of the archive looks larger than the free space (spec §8.5).
	pub(super) fn check_space_estimate(&mut self, now: Instant) {
		let due = self
			.live
			.last_space_check
			.is_none_or(|last| now.saturating_duration_since(last) >= self.tuning.disk_poll);
		if !due {
			return;
		}

		self.live.last_space_check = Some(now);
		let totals = self.tree.totals(FolderId::ROOT);
		let estimate = space_estimate(
			totals.size.saturating_sub(totals.bytes_in),
			totals.bytes_in,
			totals.bytes_out,
		);
		let free = free_space(&self.settings.destination).unwrap_or(u64::MAX);
		let short = estimate > free;
		if short && !self.live.space_warned {
			self.notice(
				NoticeLevel::Warning,
				format!("the rest of the archive may need about {estimate} bytes, and {free} are free"),
			);
		}
		self.live.space_warned = short;
	}
}
