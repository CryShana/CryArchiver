//! Applying source listings (project layer spec §6.1 and §6.2): new, changed, vanished and unreadable files, new
//! folders, and folder links that lead back into their own tree.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};

use cryarchiver_engine::text::display_name;

use super::owner::{Message, Owner};
use crate::keys::RelPath;
use crate::record::{CycleRecord, FileRecord, FileState, FolderRecord, Identity, IssueKind, MediaClass};
use crate::scan::{self, EntryKind, Listing};
use crate::tree::{FileId, FolderId};
use crate::view::{LogKind, NoticeLevel, Update};

#[derive(Debug, Default)]
pub(crate) struct ScanState {
	queue: VecDeque<(RelPath, bool)>,
	active: usize,
	pub(super) complete: bool,
}

impl Owner {
	/// The full reconcile of the source: every folder is listed again.
	pub(super) fn start_scan(&mut self) {
		self.scan.complete = false;
		self.scan.queue.push_back((RelPath::root(), true));
		self.view.status_dirty = true;
		self.pump_listings();
	}

	pub(super) fn pump_listings(&mut self) {
		while self.scan.active < self.tuning.listings {
			let Some((folder, recursive)) = self.scan.queue.pop_front() else {
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

	pub(super) fn on_listed(&mut self, listing: &Listing, recursive: bool) {
		self.scan.active -= 1;

		let Some(folder) = self.tree.folder_id(&listing.folder) else {
			self.after_listing();
			return;
		};

		if let Some(error) = &listing.error {
			self.notice(
				NoticeLevel::Warning,
				format!("{} can't be read: {error}", self.folder_label(folder)),
			);
		} else if self.leads_back(folder, listing.identity) {
			self.found_cycle(folder);
		} else {
			self.apply_listing(folder, listing, recursive);
		}

		self.tree.set_scanned(folder, true);
		self.save_folder(folder);
		self.touch_folder(folder);
		self.after_listing();
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

		for &index in &diff.broken {
			if let EntryKind::Broken(reason) = &listing.entries[index].kind {
				self.source_broken(folder, &listing.entries[index].name, reason);
			}
		}

		for name in &diff.missing_files {
			if let Some(file) = self.tree.file_id(&self.tree.folder(folder).key.join(name)) {
				self.source_missing(file);
			}
		}

		let mut present = HashSet::new();
		for &index in &diff.folders {
			let name = &listing.entries[index].name;
			present.insert(name.clone());
			let (child, new) = self.add_folder(folder, name);
			let returned = std::mem::take(&mut self.tree.folder_mut(child).vanished);
			if recursive || new || returned || !self.tree.folder(child).record.scanned {
				self.scan.queue.push_back((self.tree.folder(child).key.clone(), true));
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
		if !known {
			self.file_added(file);
		}
	}

	/// The source changed after the project saw it: it is archived again, and the new output replaces the old one.
	pub(super) fn source_changed(&mut self, file: FileId, size: u64, mtime_ns: i64) {
		let old = &self.tree.file(file).record;
		let replaces = old
			.state
			.output()
			.map(|output| output.path.clone())
			.or(old.replaces.clone());
		let record = FileRecord {
			size,
			mtime_ns,
			identity: None,
			class: MediaClass::Unknown,
			window_hash: None,
			full_hash: None,
			replaces,
			state: FileState::Discovered,
		};
		self.tree.set_record(file, record);
		self.set_state(file, FileState::Discovered);
	}

	/// The source file is gone. An archived file keeps its output; anything else leaves the project.
	pub(super) fn source_missing(&mut self, file: FileId) {
		let state = self.tree.file(file).record.state.clone();
		match state {
			FileState::Done(output) => self.set_state(file, FileState::SourceGone(output)),
			FileState::DeletePending(output) => self.set_state(file, FileState::OriginalDeleted(output)),
			FileState::SourceGone(_) | FileState::OriginalDeleted(_) => {}
			_ => self.forget_file(file),
		}
	}

	/// An archived file listed again unchanged, after its source was gone or its original deleted, is Done as it was.
	fn source_returned(&mut self, file: FileId) {
		if let FileState::SourceGone(output) | FileState::OriginalDeleted(output) = &self.tree.file(file).record.state {
			let output = output.clone();
			self.set_state(file, FileState::Done(output));
		}
	}

	pub(super) fn forget_file(&mut self, file: FileId) {
		let folder = self.tree.file(file).folder;
		self.tree.remove_file(file);
		self.track_issue(file);
		self.save_file(file);
		self.touch_folder(folder);
		if self.view.watched.contains(&folder) {
			self.send(Update::FileRemoved { folder, file });
		}
	}

	/// An unreadable entry is an issue, unless the file is archived already: then its output stays and a warning says
	/// what happened.
	fn source_broken(&mut self, folder: FolderId, name: &OsStr, reason: &str) {
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
			},
		);
	}

	/// The folder is gone, with everything below it, including any cycle it was.
	fn subtree_missing(&mut self, folder: FolderId) {
		self.tree.folder_mut(folder).vanished = true;
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
mod tests {
	use std::fs;
	use std::path::Path;
	use std::sync::Arc;
	use std::sync::mpsc::{Receiver, channel};

	use cryarchiver_engine::engine::{Encode, MemoryGate};

	use super::*;
	use crate::project::{Backend, ProjectOptions, SystemSample, Tuning, create};
	use crate::store::Store;

	struct NoBackend;

	impl Backend for NoBackend {
		fn encoder(&self, _image_threads: usize, _memory: Arc<dyn MemoryGate>) -> Arc<dyn Encode> {
			unreachable!("scanning never encodes")
		}

		fn toolchain(&self) -> String {
			String::new()
		}

		fn sample(&self) -> SystemSample {
			SystemSample::default()
		}
	}

	/// An owner whose listings run only when the test lists a folder: nothing is spawned.
	fn owner(source: &Path, destination: &Path) -> (Owner, Receiver<Update>) {
		let project = create(source, destination, OsStr::new("archive"), |_| {}).unwrap();
		let (store, meta, loaded) = Store::open(&project).unwrap();
		let (updates, received) = channel();
		let options = ProjectOptions {
			backend: Arc::new(NoBackend),
			tuning: Tuning {
				listings: 0,
				..Tuning::default()
			},
		};

		(
			Owner::open(store, meta.settings, loaded, options, updates).unwrap(),
			received,
		)
	}

	fn list(owner: &mut Owner, folder: &RelPath, recursive: bool) {
		let listing = scan::list_folder(&owner.settings.source, folder);
		owner.scan.active += 1;
		owner.on_listed(&listing, recursive);
	}

	fn list_queued(owner: &mut Owner) {
		while let Some((folder, recursive)) = owner.scan.queue.pop_front() {
			list(owner, &folder, recursive);
		}
	}

	fn key(path: &str) -> RelPath {
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
		assert_eq!(Vec::from(owner.scan.queue.clone()), [(key("A"), true)]);

		list_queued(&mut owner);
		let b = owner.tree.folder_id(&key("A/B")).unwrap();
		assert!(!owner.tree.folder(b).vanished);
	}
}
