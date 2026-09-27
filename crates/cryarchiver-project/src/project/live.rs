//! Settings that change during a run (project layer spec §4). The performance limit applies at once wherever the OS
//! can adjust running processes; what can't reach running jobs (fewer jobs, the Linux thread fallback) is pending
//! until those jobs end or the user restarts them.

use std::collections::HashSet;
use std::path::Path;
use std::time::Instant;

use cryarchiver_engine::commit::free_space;
use cryarchiver_engine::detect::read_header;
use cryarchiver_engine::process::limits::{self, Applied, Limits};
use infer::MatcherType;

use super::owner::Owner;
use crate::schedule::{Work, least_progress_first, space_estimate};
use crate::settings::SettingsChange;
use crate::tree::{FileId, FolderId};
use crate::view::{LogKind, NoticeLevel, Update};

#[derive(Debug)]
pub(crate) struct LiveState {
	pub(super) applied: Applied,
	/// Image encodes that started before the image thread count last changed; they keep their old count until they
	/// end.
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

fn is_image(path: &Path) -> bool {
	read_header(path)
		.is_ok_and(|(_, header)| infer::get(&header).is_some_and(|kind| kind.matcher_type() == MatcherType::Image))
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
			self.live.stale = self.image_jobs();
		}

		self.pipeline.workers_dirty = true;
		self.view.status_dirty = true;
	}

	/// The running jobs the image thread count reaches: encodes of files whose content is an image. Reads each such
	/// job's source header.
	fn image_jobs(&self) -> HashSet<FileId> {
		self.pipeline
			.jobs
			.iter()
			.filter(|(_, job)| job.work == Work::Encode)
			.map(|(file, _)| *file)
			.filter(|file| is_image(&self.source_path(&self.tree.file(*file).key)))
			.collect()
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

#[cfg(test)]
mod tests {
	use std::ffi::OsStr;
	use std::fs;

	use super::*;
	use crate::project::pipeline::tests::running_job;
	use crate::project::scanning::tests::owner;
	use crate::record::FileRecord;

	#[test]
	fn only_image_encodes_wait_for_the_new_image_thread_count() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		fs::create_dir_all(&source).unwrap();
		let (mut owner, _updates) = owner(&source, &dir.path().join("archive"));
		let jobs: [(&str, &[u8], Work); 4] = [
			("photo.jpg", b"\xFF\xD8\xFF\xE0\x00\x10JFIF\x00", Work::Encode),
			(
				"clip.mp4",
				b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom",
				Work::Encode,
			),
			("notes.txt", b"notes", Work::Copy),
			("kept.jpg", b"\xFF\xD8\xFF\xE0\x00\x10JFIF\x00", Work::Delete),
		];
		let files: Vec<FileId> = jobs
			.iter()
			.map(|(name, content, work)| {
				fs::write(source.join(name), content).unwrap();
				let file = owner
					.tree
					.add_file(FolderId::ROOT, OsStr::new(name), FileRecord::default());
				owner.pipeline.jobs.insert(file, running_job(*work));
				file
			})
			.collect();

		owner.live.stale = owner.image_jobs();
		assert_eq!(owner.live.stale, HashSet::from([files[0]]));

		owner.settings.performance_limit = Some(25);
		assert!(owner.limit_pending());
		owner.pipeline.jobs.remove(&files[0]);
		owner.live.stale.remove(&files[0]);
		assert!(
			!owner.limit_pending(),
			"the video, the copy and the delete never keep it pending"
		);
	}
}
