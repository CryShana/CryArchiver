//! Applying source listings (project layer spec §6.1 and §6.2): new, changed, vanished and unreadable files, new
//! folders, and folder links that lead back into their own tree.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};

use cryarchiver_engine::text::display_name;

use super::duplicates::is_lost;
use super::owner::{Message, Owner};
use super::resolve::Deferred;
use crate::keys::RelPath;
use crate::record::{CycleRecord, FileRecord, FileState, FolderRecord, Identity, IssueKind, MediaClass};
use crate::scan::{self, EntryKind, Listing};
use crate::tree::{FileId, FolderId};
use crate::view::{LogKind, NoticeLevel, Update};

#[derive(Debug, Default)]
pub(crate) struct ScanState {
	/// Each folder at most once, in the order first asked for.
	queue: VecDeque<RelPath>,
	/// The queued folders, and whether each listing goes on into the folder's subfolders.
	queued: HashMap<RelPath, bool>,
	active: usize,
	pub(super) complete: bool,
	/// A rescan asked for while one already runs: exactly one more starts once it lands.
	rescan_pending: bool,
	/// Folders whose last listing failed, and why. Each is an open issue and counts as not scanned until a listing
	/// succeeds, unless the user acknowledged it.
	pub(super) unreadable: BTreeMap<FolderId, String>,
	/// The volume's own folders at the source root this run has logged as left out.
	skipped_logged: HashSet<OsString>,
}

impl ScanState {
	/// A folder already queued keeps its place; asking for it again recursively makes that listing recursive.
	fn push(&mut self, folder: RelPath, recursive: bool) {
		match self.queued.entry(folder) {
			Entry::Occupied(mut queued) => *queued.get_mut() |= recursive,
			Entry::Vacant(slot) => {
				self.queue.push_back(slot.key().clone());
				slot.insert(recursive);
			}
		}
	}

	fn pop(&mut self) -> Option<(RelPath, bool)> {
		let folder = self.queue.pop_front()?;
		let recursive = self.queued.remove(&folder).unwrap_or(false);
		Some((folder, recursive))
	}
}

impl Owner {
	/// The full reconcile of the source: every folder is listed again.
	pub(super) fn start_scan(&mut self) {
		self.scan.complete = false;
		self.view.status_dirty = true;
		self.queue_listing(RelPath::root(), true);
	}

	pub(super) fn queue_listing(&mut self, folder: RelPath, recursive: bool) {
		self.scan.push(folder, recursive);
		self.pump_listings();
	}

	/// A rescan the watcher lost events asked for: coalesced into the running scan when one is already going, so a
	/// burst of hints never queues more than one follow-up, and is logged only when it asks for one.
	pub(super) fn request_rescan(&mut self) {
		if self.scanning() && self.scan.rescan_pending {
			return;
		}

		self.log(
			LogKind::Action,
			"the watcher lost events: scanning the source again".into(),
		);
		if self.scanning() {
			self.scan.rescan_pending = true;
		} else {
			self.start_scan();
		}
	}

	pub(super) fn pump_listings(&mut self) {
		while self.scan.active < self.tuning.listings {
			let Some((folder, recursive)) = self.scan.pop() else {
				break;
			};

			self.scan.active += 1;
			let root = self.settings.source.clone();
			let messages = self.messages.clone();
			tokio::task::spawn_blocking(move || {
				let listing = scan::list_folder(&root, &folder);
				let _ = messages.send(Message::Listed(listing, recursive));
			});
		}
	}

	pub(super) fn scanning(&self) -> bool {
		self.scan.active > 0 || !self.scan.queue.is_empty()
	}

	/// A folder that is gone is left to the listing of the nearest folder above it that is still there, which sees it
	/// vanish; one that can't be read is an issue and changes nothing below it.
	pub(super) fn on_listed(&mut self, listing: &Listing, recursive: bool) {
		self.scan.active -= 1;

		let Some(folder) = self.tree.folder_id(&listing.folder) else {
			self.after_listing();
			return;
		};

		if let Some(present) = &listing.gone {
			self.folder_readable(folder);
			self.scan.push(present.clone(), false);
		} else if let Some(error) = &listing.error {
			self.folder_unreadable(folder, error);
		} else {
			if self.tree.folder(folder).record.unreadable_acknowledged {
				let label = self.folder_label(folder);
				self.log(LogKind::Action, format!("{label} can be read again and is archived"));
			}
			self.folder_readable(folder);
			self.log_skipped(&listing.skipped);
			if self.leads_back(folder, listing.identity) {
				self.found_cycle(folder);
			} else {
				self.apply_listing(folder, listing, recursive);
			}
			self.tree.set_scanned(folder, true);
		}

		self.save_folder(folder);
		self.touch_folder(folder);
		self.after_listing();
	}

	/// The folder is an open issue and counts as not scanned, which holds the run below 100 % and its completion
	/// check short of clean; nothing below it changes. One the user acknowledged counts as settled instead.
	fn folder_unreadable(&mut self, folder: FolderId, reason: &str) {
		let acknowledged = self.tree.folder(folder).record.unreadable_acknowledged;
		self.tree.set_scanned(folder, acknowledged);
		if self.scan.unreadable.get(&folder).is_some_and(|known| known == reason) {
			return;
		}

		let label = self.folder_label(folder);
		if acknowledged {
			self.log(
				LogKind::Action,
				format!("{label} can't be read and is left out, as acknowledged: {reason}"),
			);
		} else {
			self.log(LogKind::Action, format!("{label} can't be read: {reason}"));
		}
		self.scan.unreadable.insert(folder, reason.to_owned());
		self.view.issues_dirty = true;
	}

	fn folder_readable(&mut self, folder: FolderId) {
		if self.scan.unreadable.remove(&folder).is_some() {
			self.view.issues_dirty = true;
		}
		self.tree.folder_mut(folder).record.unreadable_acknowledged = false;
	}

	/// The unreadable folders the user hasn't acknowledged: each is an open issue.
	pub(super) fn unreadable_issues(&self) -> impl Iterator<Item = (FolderId, &String)> {
		self.scan
			.unreadable
			.iter()
			.filter(|(folder, _)| !self.tree.folder(**folder).record.unreadable_acknowledged)
			.map(|(folder, reason)| (*folder, reason))
	}

	/// The folder is left out and counts as settled, until a listing of it succeeds.
	pub(super) fn acknowledge_unreadable(&mut self, folder: FolderId) {
		if !self.scan.unreadable.contains_key(&folder) || self.tree.folder(folder).record.unreadable_acknowledged {
			return;
		}

		self.tree.folder_mut(folder).record.unreadable_acknowledged = true;
		self.tree.set_scanned(folder, true);
		self.save_folder(folder);
		self.touch_folder(folder);
		self.view.issues_dirty = true;

		let label = self.folder_label(folder);
		self.log(
			LogKind::Action,
			format!("{label}: acknowledged; it is left out until it can be read"),
		);
	}

	fn log_skipped(&mut self, names: &[OsString]) {
		for name in names {
			if self.scan.skipped_logged.insert(name.clone()) {
				self.log(
					LogKind::Action,
					format!("{} is the volume's own folder and is left out", display_name(name)),
				);
			}
		}
	}

	fn apply_listing(&mut self, folder: FolderId, listing: &Listing, recursive: bool) {
		{
			let entry = self.tree.folder_mut(folder);
			entry.identity = listing.identity;
			entry.record.mtime_ns = listing.mtime_ns;
		}

		let stored: HashMap<OsString, (u64, i64)> = self
			.tree
			.folder(folder)
			.files
			.iter()
			.map(|&file| self.tree.file(file))
			.filter(|file| !file.removed)
			.map(|file| (file.name.clone(), (file.record.size, file.record.mtime_ns)))
			.collect();
		let diff = scan::diff(&stored, listing);

		for &index in &diff.new_files {
			if let EntryKind::File { size, mtime_ns } = listing.entries[index].kind {
				self.discover(folder, &listing.entries[index].name, size, mtime_ns);
			}
		}

		for &index in &diff.changed_files {
			if let EntryKind::File { size, mtime_ns } = listing.entries[index].kind
				&& let Some(file) = self
					.tree
					.file_id(&self.tree.folder(folder).key.join(&listing.entries[index].name))
			{
				self.source_changed(file, size, mtime_ns);
			}
		}

		for &index in &diff.unchanged_files {
			if let Some(file) = self
				.tree
				.file_id(&self.tree.folder(folder).key.join(&listing.entries[index].name))
			{
				self.source_returned(file);
			}
		}

		let mut present = HashSet::new();
		for &index in &diff.broken {
			let entry = &listing.entries[index];
			let EntryKind::Broken(reason) = &entry.kind else {
				continue;
			};

			match self.tree.folder_id(&self.tree.folder(folder).key.join(&entry.name)) {
				Some(child) => {
					present.insert(entry.name.clone());
					self.folder_unreadable(child, reason);
					self.save_folder(child);
					self.touch_folder(child);
				}
				None => self.source_broken(folder, &entry.name, reason),
			}
		}

		for name in &diff.missing_files {
			if let Some(file) = self.tree.file_id(&self.tree.folder(folder).key.join(name)) {
				self.source_missing(file);
			}
		}

		for &index in &diff.folders {
			let name = &listing.entries[index].name;
			present.insert(name.clone());
			let (child, new) = self.add_folder(folder, name);
			let returned = std::mem::take(&mut self.tree.folder_mut(child).vanished);
			if recursive || new || returned || !self.tree.folder(child).record.scanned {
				self.scan.push(self.tree.folder(child).key.clone(), true);
			}
		}

		let vanished: Vec<FolderId> = self
			.tree
			.folder(folder)
			.children
			.iter()
			.copied()
			.filter(|child| !self.tree.folder(*child).vanished && !present.contains(&self.tree.folder(*child).name))
			.collect();
		for child in vanished {
			self.subtree_missing(child);
		}
	}

	fn after_listing(&mut self) {
		self.pump_listings();

		if !self.scanning() && !self.scan.complete {
			self.scan.complete = true;
			self.view.status_dirty = true;
			self.log(
				LogKind::Action,
				format!("scan complete: {} files", self.tree.totals(FolderId::ROOT).files),
			);

			if std::mem::take(&mut self.scan.rescan_pending) {
				self.start_scan();
			}
		}
	}

	pub(super) fn add_folder(&mut self, parent: FolderId, name: &OsStr) -> (FolderId, bool) {
		let known = self.tree.folder_id(&self.tree.folder(parent).key.join(name));
		if let Some(existing) = known {
			return (existing, false);
		}

		let child = self.tree.add_folder(parent, name, FolderRecord::default());
		self.save_folder(child);
		self.send(Update::FolderAdded {
			id: child,
			parent,
			name: display_name(name),
		});
		self.touch_folder(child);
		(child, true)
	}

	fn discover(&mut self, folder: FolderId, name: &OsStr, size: u64, mtime_ns: i64) {
		let key = self.tree.folder(folder).key.join(name);
		let known = self
			.tree
			.file_id(&key)
			.is_some_and(|file| !self.tree.file(file).removed);

		let file = self.tree.add_file(folder, name, FileRecord::discovered(size, mtime_ns));
		self.set_state(file, FileState::Discovered);
		self.observe(file);
		if !known {
			self.file_added(file);
		}
	}

	/// The source changed after the project saw it: it is archived again, and the new output replaces the old one.
	/// A file that appears where a gone source was is a new file: the old output stays, and the new one gets a name
	/// of its own. So does a re-archive whose old output a placeholder adopted.
	pub(super) fn source_changed(&mut self, file: FileId, size: u64, mtime_ns: i64) {
		if self.verify_first(file, Deferred::Changed { size, mtime_ns }) {
			return;
		}

		let adopted = self.give_up_output(file);
		self.withdraw(file);

		let old = &self.tree.file(file).record;
		let replaces = match &old.state {
			_ if adopted => None,
			FileState::Committing { .. } => old.replaces.clone(),
			FileState::SourceGone(_) | FileState::DeletePending(_) | FileState::OriginalDeleted(_) => None,
			state => state
				.output()
				.map(|output| output.path.clone())
				.or(old.replaces.clone()),
		};
		let record = FileRecord {
			size,
			mtime_ns,
			identity: None,
			class: MediaClass::Unknown,
			window_hash: None,
			full_hash: None,
			replaces,
			retrying: None,
			state: FileState::Discovered,
		};
		self.tree.set_record(file, record);
		self.set_state(file, FileState::Discovered);
		self.observe(file);
	}

	/// The source file is gone. An archived file keeps its output, and its size group decides again without it as a
	/// source; anything else leaves the project.
	pub(super) fn source_missing(&mut self, file: FileId) {
		if let Some(kept) = self.pending_retry(file) {
			self.retry_source_gone(file, kept);
			return;
		}

		let state = self.tree.file(file).record.state.clone();
		match state {
			FileState::Done(output) => self.set_state(file, FileState::SourceGone(output)),
			FileState::DeletePending(output) => self.set_state(file, FileState::OriginalDeleted(output)),
			FileState::SourceGone(_) | FileState::OriginalDeleted(_) => return,
			_ => {
				if !self.verify_first(file, Deferred::Missing) {
					self.forget_file(file);
				}
				return;
			}
		}

		self.member_changed(file);
	}

	/// An archived file listed again unchanged, after its source was gone or its original deleted, is Done as it was.
	/// One whose archived content is lost is archived again from its source, replacing its placeholder.
	pub(super) fn source_returned(&mut self, file: FileId) {
		let record = &self.tree.file(file).record;
		let (FileState::SourceGone(output) | FileState::OriginalDeleted(output)) = &record.state else {
			return;
		};
		let output = output.clone();

		if is_lost(&record.state) {
			let mut record = record.clone();
			record.replaces = Some(output.path);
			self.tree.set_record(file, record);
			self.set_state(file, FileState::Ready);
			self.gate(file);
			return;
		}

		self.set_state(file, FileState::Done(output));
		self.queue_delete(file);
	}

	/// The file leaves the project; the placeholders that point at it adopt its verified output, or are released, and
	/// whatever waited for it goes on without it.
	pub(super) fn forget_file(&mut self, file: FileId) {
		self.give_up_output(file);
		self.withdraw(file);
		self.release_all_claims(file);
		let folder = self.tree.file(file).folder;
		self.tree.remove_file(file);
		self.track_issue(file);
		self.save_file(file);
		self.touch_file(file);
		if self.view.watched.contains(&folder) {
			self.send(Update::FileRemoved { folder, file });
		}

		self.file_settled(file);
	}

	/// An unreadable entry is an issue, unless the file is archived already: then its output stays and a warning says
	/// what happened. The placeholders that point at it first adopt the output an earlier issue was about, once it is
	/// verified, or are released.
	pub(super) fn source_broken(&mut self, folder: FolderId, name: &OsStr, reason: &str) {
		let key = self.tree.folder(folder).key.join(name);
		let known = self.tree.file_id(&key).filter(|&file| !self.tree.file(file).removed);
		if known.is_some_and(|file| self.tree.file(file).record.state.output().is_some()) {
			self.notice(
				NoticeLevel::Warning,
				format!("{} can't be read: {reason}", key.display()),
			);
			return;
		}

		let file = if let Some(file) = known {
			if self.verify_first(file, Deferred::Broken(reason.to_owned())) {
				return;
			}

			self.give_up_output(file);
			self.withdraw(file);
			file
		} else {
			let file = self.tree.add_file(folder, name, FileRecord::default());
			self.file_added(file);
			file
		};

		self.set_state(
			file,
			FileState::Issue {
				kind: IssueKind::SourceUnreadable,
				attempt: 0,
				reason: reason.to_owned(),
				output: None,
			},
		);
	}

	/// Takes a file out of every queue and wait, and cancels its job; used when its source changes or vanishes.
	pub(super) fn withdraw(&mut self, file: FileId) {
		self.unobserve(file);
		for queue in self.pipeline.queues.values_mut() {
			queue.remove(file);
		}
		self.pipeline.waits.forget(file);
		self.pipeline.deferred.remove(&file);
		self.pipeline.retries.remove(&file);
		self.pipeline.retry_threads.remove(&file);
		self.pipeline.retry_at.retain(|(_, waiting)| *waiting != file);
		self.pipeline.requeued.retain(|(waiting, _)| *waiting != file);
		self.pipeline.chosen_copies.remove(&file);
		self.leave_dedup(file);
		self.forget_reference(file);
		if let Some(job) = self.pipeline.jobs.get(&file) {
			job.cancel.cancel();
		}
	}

	/// The folder is gone, with everything below it, including any cycle it was.
	fn subtree_missing(&mut self, folder: FolderId) {
		self.tree.folder_mut(folder).vanished = true;
		self.folder_readable(folder);
		if self.tree.folder_mut(folder).record.cycle.take().is_some() {
			self.view.issues_dirty = true;
		}

		self.contents_missing(folder);
	}

	fn contents_missing(&mut self, folder: FolderId) {
		let files = self.tree.folder(folder).files.clone();
		for file in files {
			if !self.tree.file(file).removed {
				self.source_missing(file);
			}
		}

		let children = self.tree.folder(folder).children.clone();
		for child in children {
			self.subtree_missing(child);
		}

		self.tree.set_scanned(folder, true);
		self.save_folder(folder);
	}

	/// A folder whose identity is one of its ancestors', a folder holding the source, or the destination's, would be
	/// archived forever.
	fn leads_back(&self, folder: FolderId, identity: Option<Identity>) -> bool {
		let Some(identity) = identity else {
			return false;
		};

		self.destination_identity == Some(identity)
			|| self.enclosing_identities.contains(&identity)
			|| self
				.tree
				.ancestors(folder)
				.skip(1)
				.any(|ancestor| self.tree.folder(ancestor).identity == Some(identity))
	}

	/// Whatever the folder held before it became a link back is no longer followed.
	fn found_cycle(&mut self, folder: FolderId) {
		self.contents_missing(folder);

		let record = &mut self.tree.folder_mut(folder).record;
		if record.cycle.is_none() {
			record.cycle = Some(CycleRecord::default());
			let label = self.folder_label(folder);
			self.log(
				LogKind::Action,
				format!("{label} leads back into its own tree and is not followed"),
			);
		}

		self.view.issues_dirty = true;
	}

	pub(super) fn folder_label(&self, folder: FolderId) -> String {
		let key = &self.tree.folder(folder).key;
		if key.is_root() {
			"the source folder".to_owned()
		} else {
			key.display()
		}
	}
}

#[cfg(test)]
pub(super) mod tests {
	use std::fs;
	use std::path::Path;
	use std::pin::Pin;
	use std::sync::Arc;
	use std::sync::mpsc::{Receiver, channel};

	use cryarchiver_engine::commit::OutputSink;
	use cryarchiver_engine::engine::{Encode, Job, JobEvent, MemoryGate};
	use cryarchiver_engine::outcome::JobOutcome;
	use cryarchiver_engine::quality::memory::QualityMemory;
	use tokio_util::sync::CancellationToken;

	use super::*;
	use crate::issues::{Problem, Verb};
	use crate::project::{Backend, ProjectOptions, SystemSample, Tuning, create};
	use crate::record::{CopyReason, How, Output};
	use crate::store::Store;
	use crate::watch::Hint;

	struct NoBackend;

	struct NoEncoder;

	impl Encode for NoEncoder {
		fn encode<'a>(
			&'a self,
			_job: &'a Job,
			_sink: &'a dyn OutputSink,
			_on_event: &'a (dyn Fn(JobEvent) + Sync),
			_cancel: &'a CancellationToken,
		) -> Pin<Box<dyn Future<Output = JobOutcome> + Send + 'a>> {
			unreachable!("scanning never encodes")
		}
	}

	impl Backend for NoBackend {
		fn encoder(
			&self,
			_image_threads: usize,
			_memory: Arc<dyn MemoryGate>,
			_quality: Arc<dyn QualityMemory>,
		) -> Arc<dyn Encode> {
			Arc::new(NoEncoder)
		}

		fn toolchain(&self) -> String {
			String::new()
		}

		fn sample(&self) -> SystemSample {
			SystemSample::default()
		}
	}

	/// An owner whose listings run only when the test lists a folder: nothing is spawned.
	pub(crate) fn owner(source: &Path, destination: &Path) -> (Owner, Receiver<Update>) {
		let project = create(source, destination, OsStr::new("archive"), |_| {}).unwrap();
		reopen(&project)
	}

	/// Opens the project file; an owner dropped before it, without a flush, loses its unwritten batch as in a crash.
	pub(crate) fn reopen(project: &Path) -> (Owner, Receiver<Update>) {
		let (store, meta, loaded) = Store::open(project).unwrap();
		let (updates, received) = channel();
		let options = ProjectOptions {
			backend: Arc::new(NoBackend),
			tuning: Tuning {
				listings: 0,
				..Tuning::default()
			},
			fault: None,
		};

		(
			Owner::open(project, store, meta.settings, loaded, options, updates).unwrap(),
			received,
		)
	}

	pub(crate) fn list(owner: &mut Owner, folder: &RelPath, recursive: bool) {
		let listing = scan::list_folder(&owner.settings.source, folder);
		owner.scan.active += 1;
		owner.on_listed(&listing, recursive);
	}

	pub(crate) fn list_queued(owner: &mut Owner) {
		while let Some((folder, recursive)) = owner.scan.pop() {
			list(owner, &folder, recursive);
		}
	}

	pub(crate) fn queued(owner: &Owner) -> Vec<(RelPath, bool)> {
		owner
			.scan
			.queue
			.iter()
			.map(|folder| (folder.clone(), owner.scan.queued[folder]))
			.collect()
	}

	pub(crate) fn key(path: &str) -> RelPath {
		RelPath::from_bytes(path.as_bytes().to_vec())
	}

	#[test]
	fn a_folder_that_comes_back_is_listed_again_in_full() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		fs::create_dir_all(source.join("A").join("B")).unwrap();
		let (mut owner, _updates) = owner(&source, &dir.path().join("archive"));
		list(&mut owner, &RelPath::root(), true);
		list_queued(&mut owner);

		fs::rename(source.join("A"), dir.path().join("moved")).unwrap();
		list(&mut owner, &RelPath::root(), false);
		let a = owner.tree.folder_id(&key("A")).unwrap();
		assert!(owner.tree.folder(a).vanished);
		assert_eq!(owner.tree.totals(FolderId::ROOT).unscanned, 0);
		assert!(owner.tree.is_complete(FolderId::ROOT));

		fs::rename(dir.path().join("moved"), source.join("A")).unwrap();
		list(&mut owner, &RelPath::root(), false);
		assert!(!owner.tree.folder(a).vanished);
		assert_eq!(queued(&owner), [(key("A"), true)]);

		list_queued(&mut owner);
		let b = owner.tree.folder_id(&key("A/B")).unwrap();
		assert!(!owner.tree.folder(b).vanished);
	}

	#[test]
	fn many_rescan_hints_during_a_scan_produce_exactly_one_follow_up_scan() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		fs::create_dir_all(&source).unwrap();
		let (mut owner, updates) = owner(&source, &dir.path().join("archive"));
		owner.scan.active = 1;

		for _ in 0..5 {
			owner.on_hint(Hint::Rescan);
		}
		assert!(queued(&owner).is_empty(), "no scan queued while one runs");

		owner.scan.active = 0;
		owner.after_listing();

		assert_eq!(queued(&owner), [(RelPath::root(), true)]);
		let lost = updates
			.try_iter()
			.filter(|update| matches!(update, Update::Log(entry) if entry.message.contains("lost events")))
			.count();
		assert_eq!(lost, 1, "one log line for the burst");
	}

	fn link_folder(target: &Path, link: &Path) -> bool {
		#[cfg(unix)]
		return std::os::unix::fs::symlink(target, link).is_ok();

		#[cfg(windows)]
		return std::process::Command::new("cmd")
			.arg("/C")
			.arg("mklink")
			.arg("/J")
			.arg(link)
			.arg(target)
			.output()
			.is_ok_and(|out| out.status.success());
	}

	fn copied(path: &str) -> Output {
		Output {
			how: How::Copied(CopyReason::NotMedia),
			path: key(path),
			size: 1,
			hash: None,
		}
	}

	fn states(owner: &Owner, files: &[FileId]) -> Vec<(FileState, bool)> {
		files
			.iter()
			.map(|&file| {
				(
					owner.tree.file(file).record.state.clone(),
					owner.tree.file(file).removed,
				)
			})
			.collect()
	}

	fn nothing_to_archive(owner: &Owner) -> bool {
		owner.pipeline.observed.is_empty() && owner.pipeline.queues.values().all(crate::schedule::Queue::is_empty)
	}

	#[test]
	fn a_linked_folder_whose_target_is_offline_leaves_everything_below_it_as_it_was() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		let disk = dir.path().join("disk");
		fs::create_dir_all(&source).unwrap();
		fs::create_dir_all(&disk).unwrap();
		for name in ["a.txt", "b.txt", "c.txt"] {
			fs::write(disk.join(name), b"x").unwrap();
		}
		if !link_folder(&disk, &source.join("Linked")) {
			eprintln!("skipped: can't create a folder link here");
			return;
		}
		let (mut owner, _updates) = owner(&source, &dir.path().join("archive"));
		list(&mut owner, &RelPath::root(), true);
		list_queued(&mut owner);
		let files: Vec<FileId> = ["Linked/a.txt", "Linked/b.txt", "Linked/c.txt"]
			.iter()
			.map(|path| owner.tree.file_id(&key(path)).unwrap())
			.collect();
		owner.set_state(files[0], FileState::Done(copied("Linked/a.txt")));
		owner.set_state(files[1], FileState::DeletePending(copied("Linked/b.txt")));
		owner.set_state(files[2], FileState::Excluded);
		owner.pipeline.observed.clear();
		let before = states(&owner, &files);

		fs::rename(&disk, dir.path().join("offline")).unwrap();
		list(&mut owner, &RelPath::root(), false);
		list(&mut owner, &key("Linked"), true);

		let linked = owner.tree.folder_id(&key("Linked")).unwrap();
		assert_eq!(states(&owner, &files), before);
		assert!(
			owner.tree.file_id(&key("Linked")).is_none(),
			"no file issue for the folder"
		);
		assert!(owner.issue_files.is_empty());
		assert!(owner.scan.unreadable.contains_key(&linked));
		assert!(!owner.tree.folder(linked).vanished);
		assert!(!owner.tree.is_complete(FolderId::ROOT));
		let issues = owner.issues_view();
		assert_eq!(issues.len(), 1);
		assert_eq!(
			(issues[0].problem, issues[0].file, issues[0].folder),
			(Problem::SourceUnreadable, None, Some(linked))
		);

		fs::rename(dir.path().join("offline"), &disk).unwrap();
		list(&mut owner, &RelPath::root(), false);
		list_queued(&mut owner);

		assert_eq!(states(&owner, &files), before);
		assert!(owner.scan.unreadable.is_empty());
		assert!(owner.tree.folder(linked).record.scanned);
		assert!(nothing_to_archive(&owner), "nothing is archived again");
	}

	#[test]
	fn a_folder_that_cant_be_read_is_an_issue_until_a_listing_succeeds() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		fs::create_dir_all(source.join("Private")).unwrap();
		fs::write(source.join("Private").join("a.txt"), b"a").unwrap();
		let (mut owner, _updates) = owner(&source, &dir.path().join("archive"));
		list(&mut owner, &RelPath::root(), true);
		list_queued(&mut owner);
		let private = owner.tree.folder_id(&key("Private")).unwrap();
		let file = owner.tree.file_id(&key("Private/a.txt")).unwrap();

		owner.scan.active += 1;
		owner.on_listed(
			&Listing {
				folder: key("Private"),
				error: Some("Access is denied. (os error 5)".into()),
				..Listing::default()
			},
			false,
		);

		assert!(!owner.tree.folder(private).record.scanned);
		assert!(owner.tree.stats(FolderId::ROOT).progress < 1.0);
		assert!(!owner.tree.file(file).removed, "nothing below it changes");
		assert_eq!(owner.overall().open_issues, 1);
		let issues = owner.issues_view();
		assert_eq!(issues.len(), 1);
		assert_eq!(issues[0].location, "Private");
		assert!(issues[0].reason.ends_with("Access is denied. (os error 5)"));
		assert_eq!(issues[0].verbs.retry, Some("Retry"));

		list(&mut owner, &key("Private"), false);

		assert!(owner.scan.unreadable.is_empty());
		assert!(owner.tree.folder(private).record.scanned);
		assert!(owner.issues_view().is_empty());
	}

	#[test]
	fn a_missing_source_root_is_an_issue_and_forgets_nothing() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		fs::create_dir_all(&source).unwrap();
		fs::write(source.join("a.txt"), b"a").unwrap();
		let (mut owner, _updates) = owner(&source, &dir.path().join("archive"));
		list(&mut owner, &RelPath::root(), true);
		let file = owner.tree.file_id(&key("a.txt")).unwrap();
		owner.set_state(file, FileState::Done(copied("a.txt")));

		fs::rename(&source, dir.path().join("unplugged")).unwrap();
		list(&mut owner, &RelPath::root(), true);

		assert_eq!(owner.tree.file(file).record.state, FileState::Done(copied("a.txt")));
		assert!(owner.scan.unreadable.contains_key(&FolderId::ROOT));
		assert_eq!(owner.issues_view()[0].location, "the source folder");
		assert!(!owner.tree.is_complete(FolderId::ROOT));
	}

	#[test]
	fn a_deleted_folder_vanishes_through_its_parent_without_an_issue() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		fs::create_dir_all(source.join("Old")).unwrap();
		fs::write(source.join("Old").join("a.txt"), b"a").unwrap();
		let (mut owner, updates) = owner(&source, &dir.path().join("archive"));
		list(&mut owner, &RelPath::root(), true);
		list_queued(&mut owner);
		let file = owner.tree.file_id(&key("Old/a.txt")).unwrap();

		fs::remove_dir_all(source.join("Old")).unwrap();
		list(&mut owner, &key("Old"), false);

		assert!(owner.scan.unreadable.is_empty());
		assert_eq!(queued(&owner), [(RelPath::root(), false)]);
		list_queued(&mut owner);
		let old = owner.tree.folder_id(&key("Old")).unwrap();
		assert!(owner.tree.folder(old).vanished);
		assert!(owner.tree.file(file).removed);
		let unreadable = updates.try_iter().any(|update| match update {
			Update::Log(entry) => entry.message.contains("can't be read"),
			Update::Notice(notice) => notice.text.contains("can't be read"),
			_ => false,
		});
		assert!(!unreadable, "a deleted folder isn't reported as unreadable");
	}

	#[test]
	fn a_queued_folder_deleted_with_the_folder_above_it_vanishes_through_the_nearest_folder_still_there() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		fs::create_dir_all(source.join("A").join("B")).unwrap();
		fs::write(source.join("A").join("a.txt"), b"a").unwrap();
		fs::write(source.join("A").join("B").join("b.txt"), b"b").unwrap();
		let (mut owner, updates) = owner(&source, &dir.path().join("archive"));
		list(&mut owner, &RelPath::root(), true);
		list_queued(&mut owner);
		let files: Vec<FileId> = ["A/a.txt", "A/B/b.txt"]
			.iter()
			.map(|path| owner.tree.file_id(&key(path)).unwrap())
			.collect();

		fs::remove_dir_all(source.join("A")).unwrap();
		list(&mut owner, &key("A/B"), false);

		assert!(owner.scan.unreadable.is_empty());
		assert!(owner.issues_view().is_empty());
		assert_eq!(queued(&owner), [(RelPath::root(), false)]);
		list_queued(&mut owner);
		for folder in ["A", "A/B"] {
			assert!(
				owner.tree.folder(owner.tree.folder_id(&key(folder)).unwrap()).vanished,
				"{folder}"
			);
		}
		assert!(files.iter().all(|&file| owner.tree.file(file).removed));
		assert!(owner.issues_view().is_empty());
		let unreadable = updates.try_iter().any(|update| match update {
			Update::Log(entry) => entry.message.contains("can't be read"),
			Update::Notice(notice) => notice.text.contains("can't be read"),
			_ => false,
		});
		assert!(!unreadable, "a deleted folder isn't reported as unreadable");
	}

	#[test]
	fn the_volumes_own_folders_at_the_source_root_are_never_listed_and_logged_once() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		for folder in [
			"System Volume Information",
			"$RECYCLE.BIN",
			"lost+found",
			"Photos/lost+found",
		] {
			fs::create_dir_all(source.join(folder)).unwrap();
			fs::write(source.join(folder).join("x.txt"), b"x").unwrap();
		}
		let (mut owner, updates) = owner(&source, &dir.path().join("archive"));

		for _ in 0..2 {
			list(&mut owner, &RelPath::root(), true);
			list_queued(&mut owner);
		}

		for folder in ["System Volume Information", "$RECYCLE.BIN", "lost+found"] {
			assert!(owner.tree.folder_id(&key(folder)).is_none(), "{folder}");
		}
		assert!(owner.tree.file_id(&key("Photos/lost+found/x.txt")).is_some());
		assert_eq!(owner.tree.totals(FolderId::ROOT).files, 1);
		let logged: Vec<String> = updates
			.try_iter()
			.filter_map(|update| match update {
				Update::Log(entry) if entry.message.contains("the volume's own folder") => Some(entry.message),
				_ => None,
			})
			.collect();
		assert_eq!(logged.len(), 3, "{logged:?}");
		assert!(
			logged
				.iter()
				.any(|message| message.starts_with("System Volume Information "))
		);
	}

	fn unreadable(owner: &mut Owner, folder: &str) {
		owner.scan.active += 1;
		owner.on_listed(
			&Listing {
				folder: key(folder),
				error: Some("Access is denied. (os error 5)".into()),
				..Listing::default()
			},
			false,
		);
	}

	#[test]
	fn an_acknowledged_unreadable_folder_is_settled_until_a_listing_of_it_succeeds() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		fs::create_dir_all(source.join("Private")).unwrap();
		let (mut owner, _updates) = owner(&source, &dir.path().join("archive"));
		list(&mut owner, &RelPath::root(), false);
		let private = owner.tree.folder_id(&key("Private")).unwrap();
		unreadable(&mut owner, "Private");
		unreadable(&mut owner, "");

		let issues = owner.issues_view();
		let (root, folder) = (&issues[0], &issues[1]);
		assert_eq!(root.location, "the source folder");
		assert_eq!(root.verbs.accept, None, "the source folder can't be left out");
		assert_eq!(folder.verbs.accept, Some("Acknowledge"));
		owner.resolve(root.id, Verb::Accept);
		owner.resolve(folder.id, Verb::Accept);
		list(&mut owner, &RelPath::root(), false);

		assert!(owner.tree.folder(private).record.unreadable_acknowledged);
		assert!(owner.issues_view().is_empty());
		assert_eq!(owner.overall().open_issues, 0);
		assert!(owner.tree.is_complete(FolderId::ROOT));

		unreadable(&mut owner, "Private");
		assert!(owner.issues_view().is_empty(), "acknowledged once is enough");

		fs::write(source.join("Private").join("a.txt"), b"a").unwrap();
		list(&mut owner, &key("Private"), false);

		assert!(!owner.tree.folder(private).record.unreadable_acknowledged);
		assert!(owner.scan.unreadable.is_empty());
		assert!(owner.tree.file_id(&key("Private/a.txt")).is_some());
	}

	#[test]
	fn a_burst_of_a_thousand_hints_lists_each_folder_once() {
		const RUNAWAY: usize = 5_000;

		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		fs::create_dir_all(&source).unwrap();
		let (mut owner, _updates) = owner(&source, &dir.path().join("archive"));
		list(&mut owner, &RelPath::root(), true);
		for index in 0..1000 {
			fs::create_dir_all(source.join("Big").join(format!("sub{index}"))).unwrap();
		}

		for index in 0..1000 {
			owner.relist(key(&format!("Big/sub{index}")));
		}
		owner.relist(key("Big"));
		assert_eq!(queued(&owner), [(RelPath::root(), false)]);

		let mut listings = 0;
		while let Some((folder, recursive)) = owner.scan.pop() {
			list(&mut owner, &folder, recursive);
			listings += 1;
			if listings > RUNAWAY {
				break;
			}
		}

		assert_eq!(listings, 1 + 1 + 1000);
	}

	#[test]
	fn a_folder_asked_for_again_keeps_its_place_and_becomes_recursive() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		fs::create_dir_all(&source).unwrap();
		let (mut owner, _updates) = owner(&source, &dir.path().join("archive"));

		owner.scan.push(key("A"), false);
		owner.scan.push(key("B"), false);
		owner.scan.push(key("A"), true);
		owner.scan.push(key("B"), false);

		assert_eq!(queued(&owner), [(key("A"), true), (key("B"), false)]);
	}
}
