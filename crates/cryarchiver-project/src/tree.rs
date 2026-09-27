//! The in-memory mirror of the source tree and its progress sums (project layer spec §10.2). Folders and files live
//! in arenas, so their ids stay valid for the whole session. Each folder keeps its subtree's sums; a file change
//! walks up only its own ancestors.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};

use cryarchiver_engine::naming::ArchivalFolder;

use crate::keys::RelPath;
use crate::record::{CopyReason, FileRecord, FileState, FolderRecord, How, Identity};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FolderId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId(pub u64);

impl FolderId {
	pub const ROOT: Self = Self(0);

	fn index(self) -> usize {
		self.0 as usize
	}
}

impl FileId {
	fn index(self) -> usize {
		usize::try_from(self.0).expect("file ids fit in memory")
	}
}

/// A subtree's sums. `fraction` adds up the progress of the files that are running.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Totals {
	pub files: u64,
	pub settled: u64,
	pub fraction: f64,
	pub bytes_in: u64,
	pub bytes_out: u64,
	pub issues: u32,
	pub unscanned: u32,
	/// Source bytes of every counted file, finished or not.
	pub size: u64,
	pub kept: u64,
	pub duplicates: u64,
	pub originals_deleted: u64,
}

/// What a folder row shows: the subtree's counts, progress and sizes.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FolderStats {
	pub files_done: u64,
	pub files_total: u64,
	/// Below 1 until the folder is complete, so a floored percentage never reads 100 % early.
	pub progress: f64,
	pub bytes_in: u64,
	pub bytes_out: u64,
	pub open_issues: u32,
	pub complete: bool,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct Share {
	settled: bool,
	fraction: f64,
	bytes_in: u64,
	bytes_out: u64,
	issue: bool,
	counted: bool,
	size: u64,
	kept: bool,
	duplicate: bool,
	deleted: bool,
}

#[derive(Debug)]
pub struct Folder {
	pub key: RelPath,
	pub name: OsString,
	pub parent: Option<FolderId>,
	pub children: Vec<FolderId>,
	pub files: Vec<FileId>,
	pub record: FolderRecord,
	pub naming: ArchivalFolder,
	pub identity: Option<Identity>,
	totals: Totals,
}

#[derive(Debug)]
pub struct File {
	pub key: RelPath,
	pub name: OsString,
	pub folder: FolderId,
	pub record: FileRecord,
	/// No longer part of the tree: the source vanished before it was archived.
	pub removed: bool,
	progress: f64,
	share: Share,
}

#[derive(Debug)]
pub struct Tree {
	folders: Vec<Folder>,
	files: Vec<File>,
	folder_keys: HashMap<RelPath, FolderId>,
	file_keys: HashMap<RelPath, FileId>,
}

impl Default for Tree {
	fn default() -> Self {
		Self::new()
	}
}

impl Tree {
	pub fn new() -> Self {
		let root = Folder {
			key: RelPath::root(),
			name: OsString::new(),
			parent: None,
			children: Vec::new(),
			files: Vec::new(),
			record: FolderRecord::default(),
			naming: ArchivalFolder::default(),
			identity: None,
			totals: Totals {
				unscanned: 1,
				..Totals::default()
			},
		};

		Self {
			folders: vec![root],
			files: Vec::new(),
			folder_keys: HashMap::from([(RelPath::root(), FolderId::ROOT)]),
			file_keys: HashMap::new(),
		}
	}

	pub fn folder(&self, id: FolderId) -> &Folder {
		&self.folders[id.index()]
	}

	pub fn folder_mut(&mut self, id: FolderId) -> &mut Folder {
		&mut self.folders[id.index()]
	}

	pub fn file(&self, id: FileId) -> &File {
		&self.files[id.index()]
	}

	pub fn has_folder(&self, id: FolderId) -> bool {
		id.index() < self.folders.len()
	}

	pub fn has_file(&self, id: FileId) -> bool {
		usize::try_from(id.0).is_ok_and(|index| index < self.files.len())
	}

	pub fn folder_id(&self, key: &RelPath) -> Option<FolderId> {
		self.folder_keys.get(key).copied()
	}

	pub fn file_id(&self, key: &RelPath) -> Option<FileId> {
		self.file_keys.get(key).copied()
	}

	pub fn folder_count(&self) -> usize {
		self.folders.len()
	}

	pub fn file_count(&self) -> usize {
		self.files.len()
	}

	pub fn file_ids(&self) -> impl DoubleEndedIterator<Item = FileId> + use<> {
		(0..self.files.len() as u64).map(FileId)
	}

	pub fn folder_ids(&self) -> impl DoubleEndedIterator<Item = FolderId> + use<> {
		(0..u32::try_from(self.folders.len()).expect("fewer than 2^32 folders")).map(FolderId)
	}

	pub fn ancestors(&self, id: FolderId) -> impl Iterator<Item = FolderId> + '_ {
		std::iter::successors(Some(id), |&folder| self.folder(folder).parent)
	}

	pub fn totals(&self, id: FolderId) -> Totals {
		self.folder(id).totals
	}

	/// Adds a folder under `parent`, or returns the one already there. An unscanned folder holds its ancestors below
	/// 100 %.
	pub fn add_folder(&mut self, parent: FolderId, name: &OsStr, record: FolderRecord) -> FolderId {
		let key = self.folder(parent).key.join(name);
		if let Some(existing) = self.folder_id(&key) {
			return existing;
		}

		let id = FolderId(u32::try_from(self.folders.len()).expect("fewer than 2^32 folders"));
		let unscanned = u32::from(!record.scanned);
		self.folders.push(Folder {
			key: key.clone(),
			name: name.to_os_string(),
			parent: Some(parent),
			children: Vec::new(),
			files: Vec::new(),
			record,
			naming: ArchivalFolder::default(),
			identity: None,
			totals: Totals::default(),
		});
		self.folders[parent.index()].children.push(id);
		self.folder_keys.insert(key, id);
		self.add_unscanned(id, i64::from(unscanned));
		id
	}

	/// The folder at `key`, creating any missing ancestors as unscanned.
	pub fn ensure_folder(&mut self, key: &RelPath) -> FolderId {
		if let Some(existing) = self.folder_id(key) {
			return existing;
		}

		let parent = key
			.parent()
			.map_or(FolderId::ROOT, |parent| self.ensure_folder(&parent));
		self.add_folder(parent, &key.name(), FolderRecord::default())
	}

	pub fn add_file(&mut self, folder: FolderId, name: &OsStr, record: FileRecord) -> FileId {
		let key = self.folder(folder).key.join(name);
		if let Some(existing) = self.file_id(&key) {
			self.files[existing.index()].removed = false;
			self.set_record(existing, record);
			return existing;
		}

		let id = FileId(self.files.len() as u64);
		self.files.push(File {
			key: key.clone(),
			name: name.to_os_string(),
			folder,
			record,
			removed: false,
			progress: 0.0,
			share: Share::default(),
		});
		self.folders[folder.index()].files.push(id);
		self.folders[folder.index()].naming.add(name);
		self.file_keys.insert(key, id);
		self.refresh_share(id);
		id
	}

	pub fn set_record(&mut self, id: FileId, record: FileRecord) {
		self.files[id.index()].record = record;
		self.refresh_share(id);
	}

	pub fn set_state(&mut self, id: FileId, state: FileState) {
		let file = &mut self.files[id.index()];
		if !matches!(state, FileState::InProgress { .. }) {
			file.progress = 0.0;
		}
		file.record.state = state;
		self.refresh_share(id);
	}

	/// The last progress the engine reported for a running file.
	pub fn set_fraction(&mut self, id: FileId, fraction: f64) {
		if matches!(self.files[id.index()].record.state, FileState::InProgress { .. }) {
			self.files[id.index()].progress = fraction.clamp(0.0, 1.0);
			self.refresh_share(id);
		}
	}

	pub fn remove_file(&mut self, id: FileId) {
		self.files[id.index()].removed = true;
		self.refresh_share(id);
	}

	pub fn set_scanned(&mut self, id: FolderId, scanned: bool) {
		let folder = &mut self.folders[id.index()];
		if folder.record.scanned == scanned {
			return;
		}

		folder.record.scanned = scanned;
		self.add_unscanned(id, if scanned { -1 } else { 1 });
	}

	pub fn stats(&self, id: FolderId) -> FolderStats {
		let totals = self.totals(id);
		let complete = totals.unscanned == 0 && totals.settled == totals.files && totals.issues == 0;

		#[allow(clippy::cast_precision_loss)]
		let progress = if complete {
			1.0
		} else if totals.files == 0 {
			0.0
		} else {
			((totals.settled as f64 + totals.fraction) / totals.files as f64).min(0.999_999)
		};

		FolderStats {
			files_done: totals.settled,
			files_total: totals.files,
			progress,
			bytes_in: totals.bytes_in,
			bytes_out: totals.bytes_out,
			open_issues: totals.issues,
			complete,
		}
	}

	pub fn is_complete(&self, id: FolderId) -> bool {
		self.stats(id).complete
	}

	fn refresh_share(&mut self, id: FileId) {
		let file = &self.files[id.index()];
		let output = file.record.state.output();
		let settled = file.record.state.is_settled();
		let share = if file.removed {
			Share::default()
		} else {
			Share {
				settled,
				fraction: if matches!(file.record.state, FileState::InProgress { .. }) {
					file.progress
				} else {
					0.0
				},
				bytes_in: if output.is_some() { file.record.size } else { 0 },
				bytes_out: output.map_or(0, |output| output.size),
				issue: matches!(file.record.state, FileState::Issue { .. }),
				counted: true,
				size: file.record.size,
				kept: output.is_some_and(|output| output.how == How::Copied(CopyReason::KeptOriginal)),
				duplicate: output.is_some_and(|output| matches!(output.how, How::Placeholder { .. })),
				deleted: matches!(file.record.state, FileState::OriginalDeleted(_)),
			}
		};

		let old = std::mem::replace(&mut self.files[id.index()].share, share);
		if old == share {
			return;
		}

		let folder = self.files[id.index()].folder;
		let ancestors: Vec<FolderId> = self.ancestors(folder).collect();
		for ancestor in ancestors {
			let totals = &mut self.folders[ancestor.index()].totals;
			totals.files = totals.files + u64::from(share.counted) - u64::from(old.counted);
			totals.settled = totals.settled + u64::from(share.settled) - u64::from(old.settled);
			totals.fraction += share.fraction - old.fraction;
			totals.bytes_in = totals.bytes_in + share.bytes_in - old.bytes_in;
			totals.bytes_out = totals.bytes_out + share.bytes_out - old.bytes_out;
			totals.issues = totals.issues + u32::from(share.issue) - u32::from(old.issue);
			totals.size = totals.size + share.size - old.size;
			totals.kept = totals.kept + u64::from(share.kept) - u64::from(old.kept);
			totals.duplicates = totals.duplicates + u64::from(share.duplicate) - u64::from(old.duplicate);
			totals.originals_deleted = totals.originals_deleted + u64::from(share.deleted) - u64::from(old.deleted);
		}
	}

	fn add_unscanned(&mut self, id: FolderId, delta: i64) {
		let ancestors: Vec<FolderId> = self.ancestors(id).collect();
		for ancestor in ancestors {
			let totals = &mut self.folders[ancestor.index()].totals;
			totals.unscanned = u32::try_from(i64::from(totals.unscanned) + delta).unwrap_or(0);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::record::{CopyReason, Hash, How, IssueKind, Output};

	fn done(out: u64) -> FileState {
		FileState::Done(Output {
			how: How::Copied(CopyReason::NotMedia),
			path: RelPath::root(),
			size: out,
			hash: Some(Hash([0; 32])),
		})
	}

	fn scanned() -> FolderRecord {
		FolderRecord {
			scanned: true,
			..FolderRecord::default()
		}
	}

	fn tree_with_two_levels() -> (Tree, FolderId, FileId, FileId, FileId) {
		let mut tree = Tree::new();
		tree.set_scanned(FolderId::ROOT, true);
		let photos = tree.add_folder(FolderId::ROOT, OsStr::new("Photos"), scanned());
		let a = tree.add_file(FolderId::ROOT, OsStr::new("a.txt"), FileRecord::discovered(10, 1));
		let b = tree.add_file(photos, OsStr::new("b.jpg"), FileRecord::discovered(100, 1));
		let c = tree.add_file(photos, OsStr::new("c.jpg"), FileRecord::discovered(100, 1));
		(tree, photos, a, b, c)
	}

	#[test]
	fn every_file_weighs_the_same_however_deep() {
		let (mut tree, photos, a, b, _) = tree_with_two_levels();

		tree.set_state(a, done(10));
		tree.set_state(b, FileState::InProgress { attempt: 1 });
		tree.set_fraction(b, 0.5);

		assert_eq!(tree.stats(photos).files_total, 2);
		assert!((tree.stats(photos).progress - 0.25).abs() < 1e-9);
		assert_eq!(tree.stats(FolderId::ROOT).files_done, 1);
		assert!((tree.stats(FolderId::ROOT).progress - 0.5).abs() < 1e-9);
	}

	#[test]
	fn a_folder_is_complete_only_when_everything_below_is_settled_and_scanned() {
		let (mut tree, photos, a, b, c) = tree_with_two_levels();
		for file in [a, b, c] {
			tree.set_state(file, done(5));
		}
		assert!(tree.stats(FolderId::ROOT).complete);
		assert!((tree.stats(FolderId::ROOT).progress - 1.0).abs() < f64::EPSILON);

		let deeper = tree.add_folder(photos, OsStr::new("2021"), FolderRecord::default());
		assert!(!tree.stats(FolderId::ROOT).complete);
		assert!(tree.stats(FolderId::ROOT).progress < 1.0);

		tree.set_scanned(deeper, true);
		assert!(tree.stats(FolderId::ROOT).complete);
	}

	#[test]
	fn open_issues_hold_the_total_below_100_percent() {
		let (mut tree, photos, a, b, c) = tree_with_two_levels();
		tree.set_state(a, done(5));
		tree.set_state(b, done(50));
		tree.set_state(
			c,
			FileState::Issue {
				kind: IssueKind::Unrecoverable { damaged: true },
				attempt: 1,
				reason: "truncated".into(),
			},
		);

		let root = tree.stats(FolderId::ROOT);
		assert_eq!((root.files_done, root.files_total, root.open_issues), (2, 3, 1));
		assert!(!root.complete);
		assert_eq!(tree.stats(photos).open_issues, 1);

		tree.set_state(c, FileState::Excluded);
		assert!(tree.stats(FolderId::ROOT).complete);
	}

	#[test]
	fn the_percentage_never_rounds_up_to_100() {
		let mut tree = Tree::new();
		tree.set_scanned(FolderId::ROOT, true);
		let files: Vec<FileId> = (0..1000)
			.map(|index| tree.add_file(FolderId::ROOT, OsStr::new(&index.to_string()), FileRecord::default()))
			.collect();
		for &file in &files[..999] {
			tree.set_state(file, done(1));
		}

		assert!((tree.stats(FolderId::ROOT).progress * 100.0).floor() < 100.0);
	}

	#[test]
	fn bytes_count_finished_files_only() {
		let (mut tree, photos, _, b, c) = tree_with_two_levels();
		tree.set_state(b, done(40));
		tree.set_state(c, FileState::Queued);

		let stats = tree.stats(photos);
		assert_eq!((stats.bytes_in, stats.bytes_out), (100, 40));
	}

	#[test]
	fn kept_originals_and_duplicates_are_counted() {
		let (mut tree, _, a, b, c) = tree_with_two_levels();
		let output = |how| Output {
			how,
			path: RelPath::root(),
			size: 1,
			hash: None,
		};

		tree.set_state(a, FileState::Done(output(How::Copied(CopyReason::KeptOriginal))));
		tree.set_state(
			b,
			FileState::Done(output(How::Placeholder {
				canonical: RelPath::root(),
			})),
		);
		tree.set_state(c, FileState::OriginalDeleted(output(How::Encoded)));

		let totals = tree.totals(FolderId::ROOT);
		assert_eq!((totals.kept, totals.duplicates, totals.originals_deleted), (1, 1, 1));
		assert_eq!(totals.size, 210);
	}

	#[test]
	fn removed_files_leave_the_sums() {
		let (mut tree, photos, _, b, _) = tree_with_two_levels();

		tree.remove_file(b);

		assert_eq!(tree.stats(photos).files_total, 1);
		assert_eq!(tree.stats(FolderId::ROOT).files_total, 2);
	}

	#[test]
	fn missing_ancestors_are_created_unscanned() {
		let mut tree = Tree::new();
		let key = RelPath::root().join(OsStr::new("a")).join(OsStr::new("b"));

		let folder = tree.ensure_folder(&key);

		assert_eq!(tree.folder(folder).key, key);
		assert_eq!(tree.totals(FolderId::ROOT).unscanned, 3);
		assert_eq!(tree.ensure_folder(&key), folder);
	}
}
