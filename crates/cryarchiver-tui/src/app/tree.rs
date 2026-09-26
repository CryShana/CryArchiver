use std::collections::HashMap;

use crate::source::{Action, ArchiveSource};
use crate::view::{FileId, FileRow, FilterEntry, Library, NodeId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expansion {
	/// Subfolders only, as "Expand all" opens them; the file rows are never requested.
	Folders,
	Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fold {
	Closed,
	FoldersOnly,
	Open,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
	Listed(usize),
	Filtered(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
	Folder(NodeId, Fold),
	File { folder: NodeId, file: FileId, slot: Slot },
	Loading(NodeId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
	pub depth: u16,
	pub kind: RowKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
	Folder(NodeId),
	File { file: FileId, folder: NodeId },
}

impl Row {
	fn selection(self) -> Option<Selection> {
		match self.kind {
			RowKind::Folder(folder, _) => Some(Selection::Folder(folder)),
			RowKind::File { folder, file, .. } => Some(Selection::File { file, folder }),
			RowKind::Loading(_) => None,
		}
	}

	fn shows(self, selection: Selection) -> bool {
		self.selection() == Some(selection)
	}
}

/// The file row a tree row stands for, looked up in the source's current lists.
pub fn file_row(source: &dyn ArchiveSource, kind: RowKind) -> Option<&FileRow> {
	match kind {
		RowKind::File {
			folder,
			slot: Slot::Listed(index),
			..
		} => source.files(folder)?.get(index),
		RowKind::File {
			slot: Slot::Filtered(index),
			..
		} => match &source.filter()?.rows.get(index)?.entry {
			FilterEntry::File { row, .. } => Some(row),
			FilterEntry::Folder(_) => None,
		},
		RowKind::Folder(..) | RowKind::Loading(_) => None,
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RowsKey {
	structure_version: u64,
	rows_version: u64,
	expansion: u64,
	filter: String,
}

/// The Library's visible rows. They are rebuilt only when the folder tree, the loaded file lists, the expansion or
/// the filter change, never per frame; values inside the rows are read from the source when drawing.
#[derive(Debug)]
pub struct TreeState {
	expanded: HashMap<NodeId, Expansion>,
	expansion: u64,
	key: Option<RowsKey>,
	rows: Vec<Row>,
	matching_files: Option<usize>,
	selected: Option<Selection>,
	pending: Option<FileId>,
	selected_index: usize,
	scroll: usize,
	page: usize,
	outbox: Vec<Action>,
}

impl TreeState {
	pub fn new() -> Self {
		Self {
			expanded: HashMap::new(),
			expansion: 0,
			key: None,
			rows: Vec::new(),
			matching_files: None,
			selected: None,
			pending: None,
			selected_index: 0,
			scroll: 0,
			page: 10,
			outbox: Vec::new(),
		}
	}

	pub fn rows(&self) -> &[Row] {
		&self.rows
	}

	/// `None` while the source hasn't answered the current filter yet.
	pub fn matching_files(&self) -> Option<usize> {
		self.matching_files
	}

	pub fn selected(&self) -> Option<Selection> {
		self.rows.get(self.selected_index).and_then(|row| row.selection())
	}

	pub fn selected_index(&self) -> Option<usize> {
		(!self.rows.is_empty()).then_some(self.selected_index)
	}

	pub fn scroll(&self) -> usize {
		self.scroll
	}

	pub fn expansion(&self, folder: NodeId) -> Option<Expansion> {
		self.expanded.get(&folder).copied()
	}

	/// The `Expanded` actions the tree has produced since the last call.
	pub fn take_actions(&mut self) -> Vec<Action> {
		std::mem::take(&mut self.outbox)
	}

	pub fn sync(&mut self, source: &dyn ArchiveSource, filter: &str) {
		let library = &source.view().library;
		let key = RowsKey {
			structure_version: library.structure_version(),
			rows_version: source.rows_version(),
			expansion: self.expansion,
			filter: filter.to_owned(),
		};
		if self.key.as_ref() == Some(&key) {
			return;
		}

		self.rebuild(source, filter);
		self.key = Some(key);
		self.reselect();
	}

	/// Records the body height of the frame on screen, so paging and scrolling match what the user sees.
	pub fn set_page(&mut self, body_height: usize) {
		self.page = body_height.max(1);
		self.clamp_scroll();
	}

	pub fn select_index(&mut self, index: usize) {
		let Some(last) = self.rows.len().checked_sub(1) else {
			return;
		};
		self.selected_index = index.min(last);
		self.selected = self.rows[self.selected_index].selection().or(self.selected);
		self.pending = None;
		self.ensure_visible();
	}

	pub fn move_by(&mut self, delta: isize) {
		let target = self.selected_index.saturating_add_signed(delta);
		self.select_index(target);
	}

	pub fn page_by(&mut self, pages: isize) {
		self.move_by(pages * isize::try_from(self.page).unwrap_or(isize::MAX));
	}

	pub fn select_last(&mut self) {
		self.select_index(usize::MAX);
	}

	pub fn scroll_by(&mut self, delta: isize) {
		self.scroll = self.scroll.saturating_add_signed(delta);
		self.clamp_scroll();
	}

	pub fn select_file_step(&mut self, forward: bool) {
		let is_file = |row: &Row| matches!(row.kind, RowKind::File { .. });
		let found = if forward {
			(self.selected_index + 1..self.rows.len()).find(|&index| is_file(&self.rows[index]))
		} else {
			(0..self.selected_index).rev().find(|&index| is_file(&self.rows[index]))
		};

		if let Some(index) = found {
			self.select_index(index);
		}
	}

	/// Closed or folders-only opens fully; fully open closes.
	pub fn toggle(&mut self, folder: NodeId) {
		let open = self.expansion(folder) != Some(Expansion::Full);
		self.set_expanded(folder, open);
	}

	pub fn set_expanded(&mut self, folder: NodeId, open: bool) {
		let was_full = self.expansion(folder) == Some(Expansion::Full);
		if open {
			self.expanded.insert(folder, Expansion::Full);
		} else {
			self.expanded.remove(&folder);
		}

		if open != was_full {
			self.outbox.push(Action::Expanded(folder, open));
		}
		self.expansion += 1;
	}

	pub fn expand_folders_below(&mut self, library: &Library, folder: NodeId) {
		let mut stack = vec![folder];
		while let Some(current) = stack.pop() {
			self.expanded.entry(current).or_insert(Expansion::Folders);
			stack.extend(&library.node(current).children);
		}
		self.expansion += 1;
	}

	pub fn collapse_below(&mut self, library: &Library, folder: NodeId) {
		let mut stack = vec![folder];
		while let Some(current) = stack.pop() {
			if self.expanded.remove(&current) == Some(Expansion::Full) {
				self.outbox.push(Action::Expanded(current, false));
			}
			stack.extend(&library.node(current).children);
		}
		self.expansion += 1;
	}

	pub fn reveal_folder(&mut self, library: &Library, folder: NodeId) {
		self.open_ancestors(library, folder);
		self.selected = Some(Selection::Folder(folder));
		self.pending = None;
	}

	/// Opens the file's folder and selects the file once its rows have loaded.
	pub fn reveal_file(&mut self, library: &Library, folder: NodeId, file: FileId) {
		self.open_ancestors(library, folder);
		if self.expansion(folder) != Some(Expansion::Full) {
			self.set_expanded(folder, true);
		}

		self.selected = Some(Selection::File { file, folder });
		self.pending = Some(file);
		self.key = None;
	}

	fn open_ancestors(&mut self, library: &Library, folder: NodeId) {
		for ancestor in library.ancestors(folder) {
			self.expanded.entry(ancestor).or_insert(Expansion::Folders);
		}
		self.expansion += 1;
	}

	fn rebuild(&mut self, source: &dyn ArchiveSource, filter: &str) {
		self.rows.clear();
		self.matching_files = Some(0);

		if filter.is_empty() {
			self.push_folder(source, source.view().library.root(), 0);
			return;
		}

		let Some(result) = source.filter().filter(|result| result.query == filter) else {
			self.matching_files = None;
			return;
		};

		self.matching_files = Some(result.matching_files);
		self.rows.extend(result.rows.iter().enumerate().map(|(index, row)| Row {
			depth: row.depth,
			kind: match &row.entry {
				FilterEntry::Folder(folder) => RowKind::Folder(*folder, Fold::Open),
				FilterEntry::File { folder, row: file } => RowKind::File {
					folder: *folder,
					file: file.id,
					slot: Slot::Filtered(index),
				},
			},
		}));
	}

	fn push_folder(&mut self, source: &dyn ArchiveSource, folder: NodeId, depth: u16) {
		let expansion = self.expansion(folder);
		let fold = match expansion {
			None => Fold::Closed,
			Some(Expansion::Folders) => Fold::FoldersOnly,
			Some(Expansion::Full) => Fold::Open,
		};
		self.rows.push(Row {
			depth,
			kind: RowKind::Folder(folder, fold),
		});

		if expansion.is_none() {
			return;
		}

		for &child in &source.view().library.node(folder).children {
			self.push_folder(source, child, depth + 1);
		}

		if expansion == Some(Expansion::Full) {
			self.push_files(source, folder, depth + 1);
		}
	}

	fn push_files(&mut self, source: &dyn ArchiveSource, folder: NodeId, depth: u16) {
		let Some(files) = source.files(folder) else {
			self.rows.push(Row {
				depth,
				kind: RowKind::Loading(folder),
			});
			return;
		};

		self.rows.extend(files.iter().enumerate().map(|(index, row)| Row {
			depth,
			kind: RowKind::File {
				folder,
				file: row.id,
				slot: Slot::Listed(index),
			},
		}));
	}

	fn reselect(&mut self) {
		if let Some(file) = self.pending
			&& let Some(index) = self.position(|row| matches!(row.kind, RowKind::File { file: id, .. } if id == file))
		{
			self.selected_index = index;
			self.pending = None;
			self.ensure_visible();
			return;
		}

		let target = self.selected;
		let found = target.and_then(|selection| self.position(|row| row.shows(selection)));
		match (found, target) {
			(Some(index), _) => self.selected_index = index,
			(None, Some(Selection::File { folder, .. })) if self.pending.is_some() => {
				if let Some(index) = self.position(|row| row.shows(Selection::Folder(folder))) {
					self.selected_index = index;
				}
			}
			_ => {
				self.selected_index = 0;
				self.selected = self.rows.first().and_then(|row| row.selection());
			}
		}
		self.ensure_visible();
	}

	fn position(&self, wanted: impl Fn(&Row) -> bool) -> Option<usize> {
		self.rows.iter().position(wanted)
	}

	fn ensure_visible(&mut self) {
		if self.selected_index < self.scroll {
			self.scroll = self.selected_index;
		} else if self.selected_index >= self.scroll + self.page {
			self.scroll = self.selected_index + 1 - self.page;
		}
		self.clamp_scroll();
	}

	fn clamp_scroll(&mut self) {
		self.scroll = self.scroll.min(self.rows.len().saturating_sub(self.page));
	}
}
