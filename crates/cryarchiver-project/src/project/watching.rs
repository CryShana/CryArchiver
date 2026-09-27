//! Changes during a run (project layer spec §6.2 and §6.3): watcher hints re-list their folder after a quiet
//! period, lost events trigger a full reconcile, the safety pass re-lists folders whose time changed, and the
//! destination is compared with the project's outputs on open and on a manual re-scan.
//!
//! The destination walk skips what it can't read, so it never proves a file absent: an output it didn't find is
//! looked up directly before it becomes an issue, and nothing it reports is ever removed or overwritten.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::time::Instant;

use cryarchiver_engine::text::display_name;

use super::owner::{Message, Owner};
use crate::issues::{IssueKey, Problem};
use crate::keys::RelPath;
use crate::record::{FileState, IssueKind, Output, UnknownRecord};
use crate::scan::{DestinationFile, walk_destination};
use crate::tree::{FileId, FolderId};
use crate::view::{IssueView, LogKind, NoticeLevel};
use crate::watch::{Debouncer, Hint, SourceWatcher, changed_folders, watch_source};

pub(crate) struct WatchState {
	pub(super) watcher: Option<SourceWatcher>,
	pub(super) debouncer: Debouncer,
	last_safety_pass: Option<Instant>,
	safety_pass_running: bool,
	destination_walk: bool,
	placed_during_walk: HashSet<FileId>,
	/// Outputs the project removed while the walk ran, which it may still have listed.
	vacated_during_walk: HashSet<RelPath>,
	pub(super) unknown: HashMap<RelPath, bool>,
}

impl WatchState {
	pub(crate) fn new(debouncer: Debouncer, unknown: HashMap<RelPath, bool>) -> Self {
		Self {
			watcher: None,
			debouncer,
			last_safety_pass: None,
			safety_pass_running: false,
			destination_walk: false,
			placed_during_walk: HashSet::new(),
			vacated_during_walk: HashSet::new(),
			unknown,
		}
	}
}

/// What a direct look at an output's path found.
enum Found {
	Size(u64),
	Missing,
	Unclear(String),
}

impl Owner {
	pub(super) fn start_watching(&mut self) {
		if !self.tuning.watcher {
			return;
		}

		let messages = self.messages.clone();
		match watch_source(&self.settings.source, move |hint| {
			let _ = messages.send(Message::Hint(hint));
		}) {
			Ok(watcher) => self.watching.watcher = Some(watcher),
			Err(err) => self.notice(
				NoticeLevel::Warning,
				format!("changes in the source can't be watched ({err}); the safety pass still finds them"),
			),
		}
	}

	pub(super) fn on_hint(&mut self, hint: Hint) {
		match hint {
			Hint::Folder(folder) => self.watching.debouncer.hint(folder, Instant::now()),
			Hint::Rescan => {
				self.log(
					LogKind::Action,
					"the watcher lost events: scanning the source again".into(),
				);
				self.start_scan();
			}
		}
	}

	pub(super) fn relist_hinted(&mut self, now: Instant) {
		for folder in self.watching.debouncer.due(now) {
			self.relist(folder);
		}
	}

	/// Lists one folder again, or its nearest ancestor the project knows and still sees: a folder it didn't know, or
	/// one that vanished and came back, is then found by its parent's listing and listed in full.
	pub(super) fn relist(&mut self, folder: RelPath) {
		let mut candidate = Some(folder);
		let known = loop {
			let Some(path) = candidate else {
				break FolderId::ROOT;
			};
			match self.tree.folder_id(&path) {
				Some(id) if !self.tree.folder(id).vanished => break id,
				_ => candidate = path.parent(),
			}
		};

		self.queue_listing(self.tree.folder(known).key.clone(), false);
	}

	pub(super) fn safety_pass(&mut self, now: Instant) {
		let due = self
			.watching
			.last_safety_pass
			.is_none_or(|last| now.saturating_duration_since(last) >= self.tuning.safety_pass);
		if !due || self.watching.safety_pass_running || self.scanning() {
			return;
		}

		if self.watching.last_safety_pass.is_none() {
			self.watching.last_safety_pass = Some(now);
			return;
		}

		self.watching.last_safety_pass = Some(now);
		self.watching.safety_pass_running = true;
		let folders: Vec<(RelPath, Option<i64>)> = self
			.tree
			.folder_ids()
			.map(|id| self.tree.folder(id))
			.filter(|folder| folder.record.cycle.is_none() && !folder.vanished)
			.map(|folder| (folder.key.clone(), folder.record.mtime_ns))
			.collect();
		let root = self.settings.source.clone();
		let messages = self.messages.clone();
		tokio::task::spawn_blocking(move || {
			let changed = changed_folders(&root, &folders);
			let _ = messages.send(Message::FoldersChanged(changed));
		});
	}

	pub(super) fn on_folders_changed(&mut self, folders: Vec<RelPath>) {
		self.watching.safety_pass_running = false;
		for folder in folders {
			if self.tree.folder_id(&folder).is_some() {
				self.relist(folder);
			}
		}
	}

	/// Compares the destination with the outputs the project wrote: missing or changed outputs and unknown files.
	pub(super) fn reconcile_destination(&mut self) {
		if self.watching.destination_walk {
			return;
		}

		self.watching.destination_walk = true;
		self.watching.placed_during_walk.clear();
		self.watching.vacated_during_walk.clear();
		let root = self.settings.destination.clone();
		let project_file = self.project_file.file_name().unwrap_or_default().to_os_string();
		let messages = self.messages.clone();
		tokio::task::spawn_blocking(move || {
			let files = walk_destination(&root, &project_file);
			let _ = messages.send(Message::DestinationListed(files));
		});
	}

	pub(super) fn output_placed_during_walk(&mut self, file: FileId) {
		if self.watching.destination_walk {
			self.watching.placed_during_walk.insert(file);
		}
	}

	pub(super) fn output_vacated_during_walk(&mut self, path: &RelPath) {
		if self.watching.destination_walk {
			self.watching.vacated_during_walk.insert(path.clone());
		}
	}

	pub(super) fn on_destination_listed(&mut self, files: &[DestinationFile]) {
		self.watching.destination_walk = false;
		let sizes: HashMap<&RelPath, u64> = files.iter().map(|file| (&file.path, file.size)).collect();

		let mut ours: HashSet<RelPath> = HashSet::new();
		let ids: Vec<FileId> = self.tree.file_ids().collect();
		for file in ids {
			ours.extend(self.recorded_outputs(file));

			let entry = self.tree.file(file);
			let Some(output) = entry.record.state.output().cloned() else {
				continue;
			};
			if entry.removed
				|| self.watching.placed_during_walk.contains(&file)
				|| self.pipeline.jobs.contains_key(&file)
				|| sizes.get(&output.path).copied() == Some(output.size)
				|| self.output_taken_over(file, &output)
			{
				continue;
			}

			self.check_output(file, output);
		}

		self.reconcile_unknown(files, &ours);
		self.view.issues_dirty = true;
	}

	/// Every destination path the file's record names as its own, now or until a re-archive replaces it.
	fn recorded_outputs(&self, file: FileId) -> Vec<RelPath> {
		let record = &self.tree.file(file).record;
		let issue_output = match &record.state {
			FileState::Issue { output, .. } => output.as_ref(),
			_ => None,
		};
		let pending = self.pipeline.jobs.get(&file).and_then(|job| job.pending_output());

		record
			.state
			.output()
			.into_iter()
			.chain(issue_output)
			.chain(pending)
			.map(|output| output.path.clone())
			.chain(record.replaces.clone())
			.collect()
	}

	/// The walk didn't find the output as recorded; only a direct look decides.
	fn check_output(&mut self, file: FileId, output: Output) {
		let found = match fs::symlink_metadata(self.destination_path(&output.path)) {
			Ok(meta) if meta.is_file() => Found::Size(meta.len()),
			Ok(_) => Found::Unclear("something other than a file is in its place".into()),
			Err(err) if err.kind() == io::ErrorKind::NotFound => Found::Missing,
			Err(err) => Found::Unclear(err.to_string()),
		};

		let reason = match found {
			Found::Size(size) if size == output.size => return,
			Found::Size(_) => "the archived file changed size",
			Found::Missing => "the archived file is missing",
			Found::Unclear(reason) => {
				self.log(
					LogKind::Action,
					format!(
						"{}: the archived file can't be checked: {reason}",
						output.path.display()
					),
				);
				return;
			}
		};

		match self.tree.file(file).record.state {
			FileState::Done(_) => self.set_state(
				file,
				FileState::Issue {
					kind: IssueKind::OutputMissing,
					attempt: 0,
					reason: reason.into(),
					output: Some(output),
				},
			),
			_ => self.log(LogKind::Action, format!("{}: {reason}", output.path.display())),
		}
	}

	/// Files the project didn't write are remembered once; a remembered one is forgotten only once a direct look
	/// confirms it is gone, or when it has become one of the project's outputs.
	fn reconcile_unknown(&mut self, files: &[DestinationFile], ours: &HashSet<RelPath>) {
		let listed: HashSet<&RelPath> = files.iter().map(|file| &file.path).collect();
		let forgotten: Vec<RelPath> = self
			.watching
			.unknown
			.keys()
			.filter(|path| {
				ours.contains(*path)
					|| (!listed.contains(path)
						&& fs::symlink_metadata(self.destination_path(path))
							.is_err_and(|err| err.kind() == io::ErrorKind::NotFound))
			})
			.cloned()
			.collect();
		for path in forgotten {
			self.watching.unknown.remove(&path);
			self.store.remove_unknown(&path);
		}

		for file in files {
			let known = ours.contains(&file.path)
				|| self.watching.vacated_during_walk.contains(&file.path)
				|| self.watching.unknown.contains_key(&file.path);
			if known {
				continue;
			}

			self.watching.unknown.insert(file.path.clone(), false);
			if let Err(err) = self.store.set_unknown(&file.path, &UnknownRecord::default()) {
				self.fail(err);
			}
		}
	}

	pub(super) fn acknowledge_unknown(&mut self, path: &RelPath) {
		let Some(acknowledged) = self.watching.unknown.get_mut(path) else {
			return;
		};

		*acknowledged = true;
		if let Err(err) = self.store.set_unknown(path, &UnknownRecord { acknowledged: true }) {
			self.fail(err);
		}
		self.view.issues_dirty = true;
	}

	pub(super) fn unknown_issues(&mut self) -> Vec<IssueView> {
		let mut paths: Vec<RelPath> = self
			.watching
			.unknown
			.iter()
			.filter(|(_, acknowledged)| !**acknowledged)
			.map(|(path, _)| path.clone())
			.collect();
		paths.sort();

		paths
			.into_iter()
			.map(|path| IssueView {
				id: self.issue_ids.id(&IssueKey::Unknown(path.clone())),
				problem: Problem::UnknownFile,
				file: None,
				folder: None,
				name: display_name(&path.name()),
				location: path.display(),
				reason: "a file in the destination that the project didn't write; it is never deleted".into(),
				resolution: "waiting for you: acknowledge".into(),
				verbs: Problem::UnknownFile.verbs(),
				attempts: 0,
			})
			.collect()
	}
}

#[cfg(test)]
mod tests {
	use std::ffi::OsStr;
	use std::sync::mpsc::Receiver;

	use super::*;
	use crate::project::scanning::tests::{key, list, list_queued, owner};
	use crate::record::{CopyReason, FileRecord, How};
	use crate::view::Update;

	fn copied(path: &str, size: u64) -> Output {
		Output {
			how: How::Copied(CopyReason::NotMedia),
			path: key(path),
			size,
			hash: None,
		}
	}

	/// An owner over an empty source, and its destination.
	fn fixture() -> (tempfile::TempDir, Owner, Receiver<Update>, std::path::PathBuf) {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		fs::create_dir_all(&source).unwrap();
		let destination = dir.path().join("archive");
		let (owner, updates) = owner(&source, &destination);
		(dir, owner, updates, destination)
	}

	fn add(owner: &mut Owner, name: &str, state: FileState) -> FileId {
		let record = FileRecord {
			state,
			..FileRecord::discovered(1, 1)
		};
		owner.tree.add_file(FolderId::ROOT, OsStr::new(name), record)
	}

	fn state(owner: &Owner, file: FileId) -> &FileState {
		&owner.tree.file(file).record.state
	}

	fn logs(updates: &Receiver<Update>) -> Vec<String> {
		updates
			.try_iter()
			.filter_map(|update| match update {
				Update::Log(entry) => Some(entry.message),
				_ => None,
			})
			.collect()
	}

	#[test]
	fn an_output_the_walk_missed_is_looked_up_before_it_is_reported() {
		let (_dir, mut owner, _updates, destination) = fixture();
		fs::write(destination.join("kept.txt"), b"kept").unwrap();
		fs::write(destination.join("old.doc"), b"from before").unwrap();
		let kept = add(&mut owner, "kept.txt", FileState::Done(copied("kept.txt", 4)));
		let lost = add(&mut owner, "lost.txt", FileState::Done(copied("lost.txt", 4)));
		owner.watching.unknown.insert(key("old.doc"), true);

		owner.on_destination_listed(&[]);

		assert_eq!(*state(&owner, kept), FileState::Done(copied("kept.txt", 4)));
		assert!(matches!(
			state(&owner, lost),
			FileState::Issue { kind: IssueKind::OutputMissing, reason, .. } if reason == "the archived file is missing"
		));
		assert_eq!(owner.watching.unknown.get(&key("old.doc")), Some(&true));
	}

	#[test]
	fn only_files_no_record_names_are_unknown() {
		let (_dir, mut owner, _updates, destination) = fixture();
		for name in ["a.txt", "a old.txt", "removed.txt", "stranger.txt", "now ours.txt"] {
			fs::write(destination.join(name), b"x").unwrap();
		}
		let file = add(&mut owner, "a.txt", FileState::Done(copied("a.txt", 1)));
		let mut record = owner.tree.file(file).record.clone();
		record.replaces = Some(key("a old.txt"));
		owner.tree.set_record(file, record);
		add(&mut owner, "now ours.txt", FileState::Done(copied("now ours.txt", 1)));
		owner.watching.unknown.insert(key("now ours.txt"), false);
		owner.watching.destination_walk = true;
		owner.output_vacated_during_walk(&key("removed.txt"));
		let listed: Vec<DestinationFile> = ["a.txt", "a old.txt", "removed.txt", "stranger.txt", "now ours.txt"]
			.into_iter()
			.map(|name| DestinationFile {
				path: key(name),
				size: 1,
			})
			.collect();

		owner.on_destination_listed(&listed);

		let unknown: Vec<&RelPath> = owner.watching.unknown.keys().collect();
		assert_eq!(unknown, [&key("stranger.txt")]);
		for name in ["a.txt", "a old.txt", "removed.txt", "stranger.txt", "now ours.txt"] {
			assert!(destination.join(name).exists(), "{name}");
		}
	}

	#[test]
	fn a_gone_originals_missing_output_is_logged() {
		let (_dir, mut owner, updates, _) = fixture();
		let gone = add(&mut owner, "heir.txt", FileState::SourceGone(copied("adopted.txt", 4)));

		owner.on_destination_listed(&[]);

		assert_eq!(*state(&owner, gone), FileState::SourceGone(copied("adopted.txt", 4)));
		assert!(
			logs(&updates).contains(&"adopted.txt: the archived file is missing".to_owned()),
			"{:?}",
			logs(&updates)
		);
	}

	#[test]
	fn a_hint_inside_a_folder_that_came_back_lists_it_again_in_full() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		fs::create_dir_all(source.join("A").join("B")).unwrap();
		fs::write(source.join("A").join("B").join("b.txt"), b"b").unwrap();
		let (mut owner, _updates) = owner(&source, &dir.path().join("archive"));
		list(&mut owner, &RelPath::root(), true);
		list_queued(&mut owner);
		fs::rename(source.join("A"), dir.path().join("moved")).unwrap();
		list(&mut owner, &RelPath::root(), false);
		fs::rename(dir.path().join("moved"), source.join("A")).unwrap();

		owner.relist(key("A/B"));
		list_queued(&mut owner);

		for folder in ["A", "A/B"] {
			let id = owner.tree.folder_id(&key(folder)).unwrap();
			assert!(!owner.tree.folder(id).vanished, "{folder}");
		}
		let file = owner.tree.file_id(&key("A/B/b.txt")).unwrap();
		assert!(!owner.tree.file(file).removed);
	}
}
