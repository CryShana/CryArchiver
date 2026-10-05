use std::cmp::Ordering;
use std::collections::HashMap;

use crate::source::{Action, ArchiveSource};
use crate::view::{FileId, FileRow, FileState, FilterEntry, Library, NodeId};

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

/// The Library's order by the Progress column. Siblings sort among themselves, folders above files, and ties keep
/// the source's order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProgressSort {
	#[default]
	Off,
	Descending,
	Ascending,
}

impl ProgressSort {
	pub fn next(self) -> Self {
		match self {
			Self::Off => Self::Descending,
			Self::Descending => Self::Ascending,
			Self::Ascending => Self::Off,
		}
	}
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

/// The row itself, then every folder above it, nearest first: the fallbacks for keeping it at the top.
fn anchor_chain(library: &Library, row: Row) -> Vec<Selection> {
	let (own, folder) = match row.kind {
		RowKind::Folder(folder, _) => (Some(Selection::Folder(folder)), library.node(folder).parent),
		RowKind::File { folder, file, .. } => (Some(Selection::File { file, folder }), Some(folder)),
		RowKind::Loading(folder) => (None, Some(folder)),
	};

	let folders = folder
		.into_iter()
		.flat_map(|folder| std::iter::once(folder).chain(library.ancestors(folder)));
	own.into_iter().chain(folders.map(Selection::Folder)).collect()
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

/// The fraction the row's Progress column shows; `None` when it shows no gauge.
pub fn shown_progress(source: &dyn ArchiveSource, kind: RowKind) -> Option<f64> {
	match kind {
		RowKind::Folder(folder, _) => {
			let stats = source.view().library.stats(folder);
			Some(if stats.complete { 1.0 } else { stats.progress })
		}
		RowKind::File { .. } => {
			let status = &file_row(source, kind)?.status;
			match (status.state, status.progress) {
				(state, Some(fraction)) if state.is_active() => Some(fraction),
				(FileState::Done, _) => Some(1.0),
				_ => None,
			}
		}
		RowKind::Loading(_) => None,
	}
}

/// Sorts each folder's rows among themselves and keeps every folder's rows directly under it, at every depth.
fn sort_levels(rows: &[Row], order: ProgressSort, progress: &impl Fn(Row) -> Option<f64>) -> Vec<Row> {
	let mut blocks = Vec::new();
	let mut start = 0;
	while start < rows.len() {
		let depth = rows[start].depth;
		let end = rows[start + 1..]
			.iter()
			.position(|row| row.depth <= depth)
			.map_or(rows.len(), |offset| start + 1 + offset);
		blocks.push(&rows[start..end]);
		start = end;
	}

	let rank = |row: Row| match row.kind {
		RowKind::Folder(..) => 0,
		RowKind::File { .. } => 1,
		RowKind::Loading(_) => 2,
	};
	let compare = |a: Row, b: Row| {
		let (a_value, b_value) = (progress(a).unwrap_or(-1.0), progress(b).unwrap_or(-1.0));
		let by_progress = match order {
			ProgressSort::Off => Ordering::Equal,
			ProgressSort::Descending => b_value.total_cmp(&a_value),
			ProgressSort::Ascending => a_value.total_cmp(&b_value),
		};
		rank(a).cmp(&rank(b)).then(by_progress)
	};
	blocks.sort_by(|a, b| compare(a[0], b[0]));

	let mut sorted = Vec::with_capacity(rows.len());
	for block in blocks {
		sorted.push(block[0]);
		sorted.extend(sort_levels(&block[1..], order, progress));
	}
	sorted
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RowsKey {
	structure_version: u64,
	rows_version: u64,
	expansion: u64,
	filter: String,
	sort: ProgressSort,
	values: u64,
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
	/// Set by reveals: the next rebuild scrolls to the selection instead of keeping the viewport where it was.
	follow: bool,
	selected_index: usize,
	scroll: usize,
	page: usize,
	outbox: Vec<Action>,
	sort: ProgressSort,
	/// Bumped while sorted whenever the source's values may have moved, so the order follows the progress.
	values: u64,
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
			follow: false,
			selected_index: 0,
			scroll: 0,
			page: 10,
			outbox: Vec::new(),
			sort: ProgressSort::Off,
			values: 0,
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

	/// The selection as asked for, including a file whose row hasn't loaded yet.
	pub fn target(&self) -> Option<Selection> {
		self.selected
	}

	pub fn selected_index(&self) -> Option<usize> {
		(!self.rows.is_empty()).then_some(self.selected_index)
	}

	pub fn scroll(&self) -> usize {
		self.scroll
	}

	pub fn sort(&self) -> ProgressSort {
		self.sort
	}

	/// Moves to the next sort mode. The selected row stays selected and in view wherever the new order puts it.
	pub fn cycle_sort(&mut self) {
		self.sort = self.sort.next();
		self.follow = true;
	}

	/// The source's statuses changed, which moves sorted rows.
	pub fn values_changed(&mut self) {
		if self.sort != ProgressSort::Off {
			self.values += 1;
		}
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
			sort: self.sort,
			values: self.values,
		};
		if self.key.as_ref() == Some(&key) {
			return;
		}

		let filter_changed = self.key.as_ref().is_some_and(|old| old.filter != key.filter);
		let anchors = self
			.rows
			.get(self.scroll)
			.map(|&row| anchor_chain(library, row))
			.unwrap_or_default();

		self.rebuild(source, filter);
		self.key = Some(key);
		let moved = self.reselect();
		self.place_viewport(&anchors, moved || filter_changed);
	}

	/// Keeps the row that was at the top of the viewport at the top, so a rebuild never scrolls under the user. When
	/// that row is gone, its nearest surviving ancestor takes its place. Only a landed reveal, a lost selection or a
	/// new filter scrolls to the selection instead.
	fn place_viewport(&mut self, anchors: &[Selection], follow_selection: bool) {
		if std::mem::take(&mut self.follow) || follow_selection {
			self.ensure_visible();
			return;
		}

		if let Some(index) = anchors
			.iter()
			.find_map(|&anchor| self.position(|row| row.shows(anchor)))
		{
			self.scroll = index;
		}
		self.clamp_scroll();
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
		self.follow = true;
		self.key = None;
	}

	/// Opens the file's folder and selects the file once its rows have loaded.
	pub fn reveal_file(&mut self, library: &Library, folder: NodeId, file: FileId) {
		self.open_ancestors(library, folder);
		if self.expansion(folder) != Some(Expansion::Full) {
			self.set_expanded(folder, true);
		}

		self.selected = Some(Selection::File { file, folder });
		self.pending = Some(file);
		self.follow = true;
		self.key = None;
	}

	fn open_ancestors(&mut self, library: &Library, folder: NodeId) {
		for ancestor in library.ancestors(folder) {
			self.expanded.entry(ancestor).or_insert(Expansion::Folders);
		}
		self.expansion += 1;
	}

	fn rebuild(&mut self, source: &dyn ArchiveSource, filter: &str) {
		self.list(source, filter);

		if self.sort != ProgressSort::Off {
			self.rows = sort_levels(&self.rows, self.sort, &|row: Row| shown_progress(source, row.kind));
		}
	}

	fn list(&mut self, source: &dyn ArchiveSource, filter: &str) {
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

	/// Returns whether the selection moved on its own: a pending reveal landed, or the selected row disappeared.
	fn reselect(&mut self) -> bool {
		if let Some(file) = self.pending
			&& let Some(index) = self.position(|row| matches!(row.kind, RowKind::File { file: id, .. } if id == file))
		{
			self.selected_index = index;
			self.pending = None;
			return true;
		}

		let target = self.selected;
		let found = target.and_then(|selection| self.position(|row| row.shows(selection)));
		match (found, target) {
			(Some(index), _) => {
				self.selected_index = index;
				false
			}
			(None, Some(Selection::File { folder, .. })) if self.pending.is_some() => {
				if let Some(index) = self.position(|row| row.shows(Selection::Folder(folder))) {
					self.selected_index = index;
				}
				false
			}
			_ => {
				self.selected_index = 0;
				self.selected = self.rows.first().and_then(|row| row.selection());
				true
			}
		}
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

#[cfg(test)]
mod tests {
	use super::*;

	struct Nested {
		rows: Vec<Row>,
		progress: Vec<(Selection, f64)>,
	}

	fn folder(id: NodeId, depth: u16) -> Row {
		Row {
			depth,
			kind: RowKind::Folder(id, Fold::Open),
		}
	}

	fn file(folder: NodeId, id: u64, depth: u16) -> Row {
		Row {
			depth,
			kind: RowKind::File {
				folder,
				file: FileId(id),
				slot: Slot::Listed(0),
			},
		}
	}

	/// `root` holds `low` (40 %) with `deep` (90 %) and files 1 (20 %) and 2 (80 %), then `high` (70 %), then files 3
	/// (50 %), 4 (no gauge) and 5 (50 %).
	fn nested() -> Nested {
		let mut library = Library::new("root");
		let root = library.root();
		let low = library.add_folder(root, "low");
		let deep = library.add_folder(low, "deep");
		let high = library.add_folder(root, "high");

		let rows = vec![
			folder(root, 0),
			folder(low, 1),
			folder(deep, 2),
			file(low, 1, 2),
			file(low, 2, 2),
			folder(high, 1),
			file(root, 3, 1),
			file(root, 4, 1),
			file(root, 5, 1),
		];
		let progress = vec![
			(Selection::Folder(root), 0.5),
			(Selection::Folder(low), 0.4),
			(Selection::Folder(deep), 0.9),
			(Selection::Folder(high), 0.7),
			(
				Selection::File {
					file: FileId(1),
					folder: low,
				},
				0.2,
			),
			(
				Selection::File {
					file: FileId(2),
					folder: low,
				},
				0.8,
			),
			(
				Selection::File {
					file: FileId(3),
					folder: root,
				},
				0.5,
			),
			(
				Selection::File {
					file: FileId(5),
					folder: root,
				},
				0.5,
			),
		];
		Nested { rows, progress }
	}

	fn order(nested: &Nested, sort: ProgressSort) -> Vec<String> {
		let progress = |row: Row| {
			let selection = row.selection()?;
			nested
				.progress
				.iter()
				.find(|(id, _)| *id == selection)
				.map(|&(_, value)| value)
		};
		let names = ["root", "low", "deep", "high"];
		sort_levels(&nested.rows, sort, &progress)
			.into_iter()
			.map(|row| match row.kind {
				RowKind::Folder(id, _) => format!("{}{}", "  ".repeat(usize::from(row.depth)), names[id.index()]),
				RowKind::File { file, .. } => format!("{}file {}", "  ".repeat(usize::from(row.depth)), file.0),
				RowKind::Loading(_) => "loading".to_owned(),
			})
			.collect()
	}

	#[test]
	fn descending_sorts_every_level_and_keeps_children_under_their_folder() {
		let nested = nested();

		assert_eq!(
			order(&nested, ProgressSort::Descending),
			[
				"root",
				"  high",
				"  low",
				"    deep",
				"    file 2",
				"    file 1",
				"  file 3",
				"  file 5",
				"  file 4",
			]
		);
	}

	#[test]
	fn ascending_puts_the_least_progress_first_and_ties_keep_their_order() {
		let nested = nested();

		assert_eq!(
			order(&nested, ProgressSort::Ascending),
			[
				"root",
				"  low",
				"    deep",
				"    file 1",
				"    file 2",
				"  high",
				"  file 4",
				"  file 3",
				"  file 5",
			]
		);
	}

	#[test]
	fn off_keeps_the_original_order() {
		let nested = nested();

		assert_eq!(
			order(&nested, ProgressSort::Off),
			[
				"root",
				"  low",
				"    deep",
				"    file 1",
				"    file 2",
				"  high",
				"  file 3",
				"  file 4",
				"  file 5",
			]
		);
	}

	#[test]
	fn the_sort_cycles_off_descending_ascending() {
		assert_eq!(ProgressSort::Off.next(), ProgressSort::Descending);
		assert_eq!(ProgressSort::Descending.next(), ProgressSort::Ascending);
		assert_eq!(ProgressSort::Ascending.next(), ProgressSort::Off);
	}
}
