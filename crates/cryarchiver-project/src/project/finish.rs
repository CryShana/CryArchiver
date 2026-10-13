//! Folder finish and the completion check (project layer spec §8.1 and §10.3). A folder whose subtree is scanned
//! and settled gets its source timestamps, children before parents; once the run is idle, the destination is walked
//! and compared with the source.

use std::collections::HashSet;
use std::fs;

use super::owner::{Message, Owner};
use crate::complete::{self, ParityReport};
use crate::keys::RelPath;
use crate::record::FileState;
use crate::run::apply_folder_times;
use crate::scan::{DestinationFile, walk_destination};
use crate::store::{DECISION_BUDGET_BYTES, LOG_BUDGET_BYTES};
use crate::tree::FolderId;
use crate::view::{LogKind, NoticeLevel};

const LISTED_GAPS: usize = 10;
const TIDY_GROWTH_BYTES: u64 = 1 << 20;

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
	/// The last report logged: the periodic safety pass sets the completion back to pending, and the same result
	/// found again is not logged again.
	reported: Option<ParityReport>,
	walks: u64,
	tidied_bytes: u64,
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

			if entry.record.timestamps_applied || entry.record.left_out() || self.finish.applying.contains(&folder) {
				continue;
			}

			let children_pending = entry.children.iter().any(|child| {
				let child = self.tree.folder(*child);
				!child.record.timestamps_applied && !child.record.left_out() && !child.vanished
			});
			if children_pending {
				self.finish.candidates.insert(folder);
				continue;
			}

			self.finish.applying.insert(folder);
			let source = self.source_path(&entry.key);
			let destination = self.destination_path(&self.destination_folder(folder));
			let (folders, messages) = (self.pipeline.folders.clone(), self.messages.clone());
			tokio::task::spawn_blocking(move || {
				let result = folders
					.prepare(&destination)
					.and_then(|_| apply_folder_times(&source, &destination))
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

	/// Nothing is running or waiting to run, and the scan is done, with no listing or safety pass under way.
	fn idle(&self) -> bool {
		self.started
			&& self.scan.complete
			&& !self.scanning()
			&& !self.watching.safety_pass_running
			&& self.pipeline.jobs.is_empty()
			&& self.pipeline.queues.values().all(crate::schedule::Queue::is_empty)
			&& self.pipeline.observed.is_empty()
			&& self.pipeline.retry_at.is_empty()
			&& self.pipeline.requeued.is_empty()
			&& !self.delete_retries_pending()
			&& !self.hashes_pending()
			&& !self.verifying()
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

		let present: HashSet<RelPath> = files
			.iter()
			.filter(|file| !file.link)
			.map(|file| file.path.clone())
			.collect();
		let mut report = complete::check(
			self.tree
				.file_ids()
				.map(|id| self.tree.file(id))
				.filter(|file| !file.removed)
				.map(|file| (&file.key, &file.record.state)),
			&present,
		);
		report.unreadable_folders = self.unreadable_issues().count() as u64;

		if self.finish.reported.as_ref() != Some(&report) {
			self.report_completion(&report);
			self.finish.reported = Some(report.clone());
		}

		self.remove_empty_work_folders();
		self.finish.completion = Completion::Done(report);
		if self.store_grew() {
			self.tidy_store();
		}
		self.view.status_dirty = true;
	}

	/// Compacts the project file and caps its failure logs and decision records once it has grown by a megabyte or a
	/// tenth since the last time. Called when a run finishes and when a project opens after a crash.
	pub(super) fn tidy_store(&mut self) {
		self.save_quality();

		let failing: HashSet<RelPath> = self
			.tree
			.file_ids()
			.map(|id| self.tree.file(id))
			.filter(|file| matches!(file.record.state, FileState::Issue { .. }))
			.map(|file| file.key.clone())
			.collect();

		let tidied = self
			.store
			.prune_logs(&failing, LOG_BUDGET_BYTES)
			.and_then(|_| self.store.prune_decisions(DECISION_BUDGET_BYTES))
			.and_then(|_| self.store.compact());
		match tidied {
			Ok(_) => self.after_flush(),
			Err(err) => self.log(
				LogKind::Action,
				format!(
					"the project file could not be tidied (pruning old failure logs and decision records and giving \
					 free space back): {err}"
				),
			),
		}
		self.finish.tidied_bytes = fs::metadata(&self.project_file).map_or(0, |meta| meta.len());
	}

	fn store_grew(&self) -> bool {
		let bytes = fs::metadata(&self.project_file).map_or(0, |meta| meta.len());
		let since = self.finish.tidied_bytes;
		bytes > since + TIDY_GROWTH_BYTES.max(since / 10)
	}

	fn report_completion(&self, report: &ParityReport) {
		self.log(LogKind::Action, format!("complete: {}", report.summary()));
		for missing in report.missing.iter().take(LISTED_GAPS) {
			self.log(LogKind::Action, format!("no archived file for {}", missing.display()));
		}
		if !report.missing.is_empty() {
			self.notice(NoticeLevel::Warning, files_without_an_archive(report.missing.len()));
		}
		if report.unreadable_folders > 0 {
			self.notice(
				NoticeLevel::Warning,
				match report.unreadable_folders {
					1 => "1 source folder can't be read, so the archive isn't complete".to_owned(),
					count => format!("{count} source folders can't be read, so the archive isn't complete"),
				},
			);
		}
	}

	pub(super) fn parity(&self) -> Option<&ParityReport> {
		match &self.finish.completion {
			Completion::Done(report) => Some(report),
			Completion::Pending | Completion::Checking(_) => None,
		}
	}
}

fn files_without_an_archive(count: usize) -> String {
	match count {
		1 => "1 source file has no archived file".to_owned(),
		count => format!("{count} source files have no archived file"),
	}
}

#[cfg(test)]
mod tests {
	use std::fs;

	use crate::project::scanning::tests::{key, list, owner};

	use super::*;

	#[test]
	fn gaps_are_counted_in_words() {
		assert_eq!(files_without_an_archive(1), "1 source file has no archived file");
		assert_eq!(files_without_an_archive(3), "3 source files have no archived file");
	}

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
	fn completion_waits_for_a_running_safety_pass() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		fs::create_dir_all(&source).unwrap();
		let (mut owner, _updates) = owner(&source, &dir.path().join("archive"));
		list(&mut owner, &RelPath::root(), true);
		owner.started = true;
		owner.finish.candidates.clear();
		assert!(owner.idle());

		owner.watching.safety_pass_running = true;
		assert!(!owner.idle());

		owner.on_folders_changed(Vec::new());
		assert!(owner.idle());
	}

	#[test]
	fn the_project_file_is_tidied_again_only_once_it_has_grown() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		fs::create_dir_all(&source).unwrap();
		let (mut owner, _updates) = owner(&source, &dir.path().join("archive"));

		owner.tidy_store();

		assert_eq!(
			owner.finish.tidied_bytes,
			fs::metadata(&owner.project_file).unwrap().len()
		);
		assert!(!owner.store_grew());

		for index in 0..30_000_u64 {
			let record = crate::record::FileRecord::discovered(index, i64::try_from(index).unwrap());
			owner.store.set_file(&key(&format!("{index}.txt")), &record).unwrap();
		}
		owner.store.flush().unwrap();

		assert!(owner.store_grew());
	}

	#[test]
	fn a_project_opened_after_a_crash_is_tidied_at_once() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		fs::create_dir_all(&source).unwrap();
		let (owner, _updates) = owner(&source, &dir.path().join("archive"));
		assert_eq!(owner.finish.tidied_bytes, 0);
		let project = owner.project_file.clone();
		drop(owner);
		crate::store::tests::leave_unclean(&project);

		let (owner, _updates) = crate::project::scanning::tests::reopen(&project);

		assert_eq!(owner.finish.tidied_bytes, fs::metadata(&project).unwrap().len());
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

	#[test]
	fn a_safety_pass_that_finds_nothing_logs_the_completion_only_once() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		fs::create_dir_all(&source).unwrap();
		let (mut owner, updates) = owner(&source, &dir.path().join("archive"));
		list(&mut owner, &RelPath::root(), true);
		owner.started = true;
		owner.finish.candidates.clear();
		let completed = |updates: &std::sync::mpsc::Receiver<crate::view::Update>| {
			updates
				.try_iter()
				.filter(
					|update| matches!(update, crate::view::Update::Log(entry) if entry.message.starts_with("complete:")),
				)
				.count()
		};

		for walk in 1..=3 {
			owner.watching.safety_pass_running = true;
			owner.check_completion();
			assert_eq!(owner.finish.completion, Completion::Pending);
			owner.on_folders_changed(Vec::new());
			owner.finish.completion = Completion::Checking(walk);
			owner.on_destination_walked(walk, &[]);
			assert!(matches!(owner.finish.completion, Completion::Done(_)));
		}

		assert_eq!(completed(&updates), 1);
	}
}
