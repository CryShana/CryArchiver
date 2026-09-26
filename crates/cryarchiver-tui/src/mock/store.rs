use super::MediaKind;
use crate::view::{FileId, FileRow, FileStatus, NodeId};

/// The mock's files, kept per folder so a loaded folder's rows can be lent out as one slice.
#[derive(Debug, Default)]
pub struct FileStore {
	by_folder: Vec<Vec<FileRow>>,
	location: Vec<(NodeId, usize)>,
	kinds: Vec<MediaKind>,
}

impl FileStore {
	pub fn add(&mut self, folder: NodeId, name: String, status: FileStatus, kind: MediaKind) -> FileId {
		if self.by_folder.len() <= folder.index() {
			self.by_folder.resize_with(folder.index() + 1, Vec::new);
		}

		let id = FileId(self.location.len() as u64);
		let rows = &mut self.by_folder[folder.index()];
		self.location.push((folder, rows.len()));
		self.kinds.push(kind);
		rows.push(FileRow { id, name, status });
		id
	}

	pub fn len(&self) -> usize {
		self.location.len()
	}

	pub fn ids(&self) -> impl Iterator<Item = FileId> + use<> {
		(0..self.location.len() as u64).map(FileId)
	}

	pub fn get(&self, id: FileId) -> &FileRow {
		let (folder, index) = self.location[slot(id)];
		&self.by_folder[folder.index()][index]
	}

	pub fn status_mut(&mut self, id: FileId) -> &mut FileStatus {
		let (folder, index) = self.location[slot(id)];
		&mut self.by_folder[folder.index()][index].status
	}

	pub fn folder(&self, id: FileId) -> NodeId {
		self.location[slot(id)].0
	}

	pub fn kind(&self, id: FileId) -> MediaKind {
		self.kinds[slot(id)]
	}

	pub fn in_folder(&self, folder: NodeId) -> &[FileRow] {
		self.by_folder.get(folder.index()).map_or(&[], Vec::as_slice)
	}
}

fn slot(id: FileId) -> usize {
	usize::try_from(id.0).expect("file ids fit in usize")
}
