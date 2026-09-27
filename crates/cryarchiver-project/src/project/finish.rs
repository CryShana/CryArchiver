//! Folder finish and the completion check (project layer spec §8.1 and §10.3). A folder whose subtree is scanned
//! and settled gets its source timestamps, children before parents; once the run is idle, the destination is walked
//! and compared with the source.

use std::collections::HashSet;
use std::fs;

use super::owner::{Message, Owner};
use crate::complete::{self, ParityReport};
use crate::keys::RelPath;
use crate::run::apply_folder_times;
use crate::scan::{DestinationFile, walk_destination};
use crate::tree::FolderId;
use crate::view::{LogKind, NoticeLevel};

const LISTED_GAPS: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum Completion {
	#[default]
	Pending,
	/// The destination walk with this number is under way.
	Checking(u64),
	Done(ParityReport),
}

#[derive(Debug, Default)]
pub(crate) struct FinishState {
	candidates: HashSet<FolderId>,
	applying: HashSet<FolderId>,
	pub(super) completion: Completion,
	walks: u64,
}

impl Owner {
	pub(super) fn folder_may_finish(&mut self, folder: FolderId) {
		self.finish.candidates.insert(folder);
	}

	pub(super) fn finish_folders(&mut self) {
		let mut candidates: Vec<FolderId> = self.finish.candidates.drain().collect();
		candidates.sort_by_key(|folder| std::cmp::Reverse(self.tree.folder(*folder).key.depth()));

		for folder in candidates {
			if self.tree.folder(folder).vanished {
				continue;
			}

			let complete = self.tree.is_complete(folder);
			let entry = self.tree.folder(folder);
			if !complete {
				if entry.record.timestamps_applied {
					self.tree.folder_mut(folder).record.timestamps_applied = false;
					self.save_folder(folder);
				}
				continue;
			}

			if entry.record.timestamps_applied || entry.record.cycle.is_some() || self.finish.applying.contains(&folder)
			{
				continue;
			}

			let children_pending = entry.children.iter().any(|child| {
				let child = self.tree.folder(*child);
				!child.record.timestamps_applied && child.record.cycle.is_none() && !child.vanished
			});
			if children_pending {
				self.finish.candidates.insert(folder);
				continue;
			}

			self.finish.applying.insert(folder);
			let source = self.source_path(&entry.key);
			let destination = self.destination_path(&self.destination_folder(folder));
			let messages = self.messages.clone();
			tokio::task::spawn_blocking(move || {
				let result = fs::create_dir_all(&destination)
					.and_then(|()| apply_folder_times(&source, &destination))
					.map_err(|err| err.to_string());
				let _ = messages.send(Message::FolderFinished(folder, result));
			});
		}
	}

	/// A folder that received a file while its timestamps were set stays pending; it finishes again once complete.
	pub(super) fn on_folder_finished(&mut self, folder: FolderId, result: Result<(), String>) {
		self.finish.applying.remove(&folder);
		if let Err(reason) = result {
			let label = self.folder_label(folder);
			self.log(LogKind::Action, format!("{label}: folder timestamps not set: {reason}"));
		}

		if !self.tree.is_complete(folder) {
			return;
		}

		self.tree.folder_mut(folder).record.timestamps_applied = true;
		self.save_folder(folder);
		if let Some(parent) = self.tree.folder(folder).parent {
			self.finish.candidates.insert(parent);
		}
	}

	/// Nothing is running or waiting to run, and the scan is done.
	fn idle(&self) -> bool {
		self.started
			&& self.scan.complete
			&& !self.scanning()
			&& self.pipeline.jobs.is_empty()
			&& self.pipeline.queues.values().all(crate::schedule::Queue::is_empty)
			&& self.pipeline.observed.is_empty()
			&& self.pipeline.retry_at.is_empty()
			&& !self.delete_retries_pending()
			&& !self.hashes_pending()
			&& self.groups_planned()
			&& self.watching.debouncer.is_empty()
			&& !self.watching.destination_walk
			&& self.finish.applying.is_empty()
			&& self.finish.candidates.is_empty()
	}

	pub(super) fn check_completion(&mut self) {
		if !self.idle() {
			if self.finish.completion != Completion::Pending {
				self.finish.completion = Completion::Pending;
				self.view.status_dirty = true;
			}
			return;
		}

		if self.finish.completion != Completion::Pending {
			return;
		}

		self.finish.walks += 1;
		let walk = self.finish.walks;
		self.finish.completion = Completion::Checking(walk);
		let root = self.settings.destination.clone();
		let project_file = self.project_file.file_name().unwrap_or_default().to_os_string();
		let messages = self.messages.clone();
		tokio::task::spawn_blocking(move || {
			let files = walk_destination(&root, &project_file);
			let _ = messages.send(Message::DestinationWalked(walk, files));
		});
	}

	/// Only the latest walk counts, and only while the run is still idle: whatever changed since it started may not be
	/// in its listing.
	pub(super) fn on_destination_walked(&mut self, walk: u64, files: &[DestinationFile]) {
		if self.finish.completion != Completion::Checking(walk) {
			return;
		}

		if !self.idle() {
			self.finish.completion = Completion::Pending;
			return;
		}

		let present: HashSet<RelPath> = files.iter().map(|file| file.path.clone()).collect();
		let report = complete::check(
			self.tree
				.file_ids()
				.map(|id| self.tree.file(id))
				.filter(|file| !file.removed)
				.map(|file| (&file.key, &file.record.state)),
			&present,
		);

		self.log(LogKind::Action, format!("complete: {}", report.summary()));
		for missing in report.missing.iter().take(LISTED_GAPS) {
			self.log(LogKind::Action, format!("no archived file for {}", missing.display()));
		}
		if !report.is_clean() {
			self.notice(
				NoticeLevel::Warning,
				format!("{} source files have no archived file", report.missing.len()),
			);
		}

		let _ = fs::remove_dir(self.pipeline.sink.staged_dir());
		let _ = fs::remove_dir(self.pipeline.sink.archive_work_root());
		self.finish.completion = Completion::Done(report);
		self.view.status_dirty = true;
	}

	pub(super) fn parity(&self) -> Option<&ParityReport> {
		match &self.finish.completion {
			Completion::Done(report) => Some(report),
			Completion::Pending | Completion::Checking(_) => None,
		}
	}
}

#[cfg(test)]
mod tests {
	use crate::project::scanning::tests::{key, list, owner};

	use super::*;

	#[test]
	fn a_folder_that_got_a_new_file_while_its_timestamps_were_set_stays_pending() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		fs::create_dir_all(source.join("Photos")).unwrap();
		let (mut owner, _updates) = owner(&source, &dir.path().join("archive"));
		list(&mut owner, &RelPath::root(), true);
		list(&mut owner, &key("Photos"), true);
		let photos = owner.tree.folder_id(&key("Photos")).unwrap();
		assert!(owner.tree.is_complete(photos));

		fs::write(source.join("Photos").join("a.txt"), b"a").unwrap();
		list(&mut owner, &key("Photos"), false);
		owner.on_folder_finished(photos, Ok(()));

		assert!(!owner.tree.folder(photos).record.timestamps_applied);
	}

	#[test]
	fn completion_waits_for_the_destination_reconcile() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		fs::create_dir_all(&source).unwrap();
		let (mut owner, _updates) = owner(&source, &dir.path().join("archive"));
		list(&mut owner, &RelPath::root(), true);
		owner.started = true;
		owner.finish.candidates.clear();
		assert!(owner.idle());
		owner.watching.destination_walk = true;

		assert!(!owner.idle());
		owner.check_completion();
		assert_eq!(owner.finish.completion, Completion::Pending);
	}

	#[test]
	fn a_destination_walk_that_ends_after_the_run_went_on_reports_nothing() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		fs::create_dir_all(&source).unwrap();
		let (mut owner, _updates) = owner(&source, &dir.path().join("archive"));
		list(&mut owner, &RelPath::root(), true);
		owner.started = true;
		owner.finish.candidates.clear();
		assert!(owner.idle());
		owner.finish.completion = Completion::Checking(2);

		owner.on_destination_walked(1, &[]);
		assert_eq!(owner.finish.completion, Completion::Checking(2));

		fs::write(source.join("a.txt"), b"a").unwrap();
		list(&mut owner, &RelPath::root(), false);
		owner.on_destination_walked(2, &[]);

		assert_eq!(owner.finish.completion, Completion::Pending);
	}
}
