use std::collections::HashSet;

use crate::format::find_ignore_case;
use crate::view::{Library, NodeId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
	pub id: NodeId,
	pub depth: u16,
	pub open: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RowsKey {
	structure_version: u64,
	expansion: u64,
	filter: String,
}

/// The Library's visible rows. They are rebuilt only when the tree's structure, the expansion or the filter
/// changes, never per frame; values inside the rows are read from the view when drawing.
#[derive(Debug)]
pub struct TreeState {
	expanded: HashSet<NodeId>,
	expansion: u64,
	key: Option<RowsKey>,
	rows: Vec<Row>,
	matching_files: usize,
	selected: Option<NodeId>,
	selected_index: usize,
	scroll: usize,
	page: usize,
}

impl TreeState {
	pub fn new(expanded: impl IntoIterator<Item = NodeId>, selected: Option<NodeId>) -> Self {
		Self {
			expanded: expanded.into_iter().collect(),
			expansion: 0,
			key: None,
			rows: Vec::new(),
			matching_files: 0,
			selected,
			selected_index: 0,
			scroll: 0,
			page: 10,
		}
	}

	pub fn rows(&self) -> &[Row] {
		&self.rows
	}

	pub fn matching_files(&self) -> usize {
		self.matching_files
	}

	pub fn selected(&self) -> Option<NodeId> {
		self.selected.filter(|_| !self.rows.is_empty())
	}

	pub fn selected_index(&self) -> Option<usize> {
		self.selected().map(|_| self.selected_index)
	}

	pub fn scroll(&self) -> usize {
		self.scroll
	}

	pub fn is_expanded(&self, id: NodeId) -> bool {
		self.expanded.contains(&id)
	}

	pub fn sync(&mut self, library: &Library, filter: &str) {
		let key = RowsKey {
			structure_version: library.structure_version(),
			expansion: self.expansion,
			filter: filter.to_owned(),
		};
		if self.key.as_ref() == Some(&key) {
			return;
		}

		self.rebuild(library, filter);
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
		self.selected = Some(self.rows[self.selected_index].id);
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

	pub fn select_file_step(&mut self, library: &Library, forward: bool) {
		let candidates: Box<dyn Iterator<Item = usize>> = if forward {
			Box::new(self.selected_index + 1..self.rows.len())
		} else {
			Box::new((0..self.selected_index).rev())
		};

		let found = candidates
			.into_iter()
			.find(|&index| !library.is_folder(self.rows[index].id));
		if let Some(index) = found {
			self.select_index(index);
		}
	}

	pub fn toggle(&mut self, id: NodeId) {
		if !self.expanded.remove(&id) {
			self.expanded.insert(id);
		}
		self.expansion += 1;
	}

	pub fn set_expanded(&mut self, id: NodeId, open: bool) {
		let changed = if open {
			self.expanded.insert(id)
		} else {
			self.expanded.remove(&id)
		};
		if changed {
			self.expansion += 1;
		}
	}

	pub fn set_expanded_below(&mut self, library: &Library, folder: NodeId, open: bool) {
		let mut stack = vec![folder];
		while let Some(id) = stack.pop() {
			if !library.is_folder(id) {
				continue;
			}

			if open {
				self.expanded.insert(id);
			} else {
				self.expanded.remove(&id);
			}
			stack.extend(&library.node(id).children);
		}
		self.expansion += 1;
	}

	/// Opens every ancestor of `id` and selects it. The caller clears any filter that would hide it.
	pub fn reveal(&mut self, library: &Library, id: NodeId, filter: &str) {
		let mut parent = library.node(id).parent;
		while let Some(folder) = parent {
			self.expanded.insert(folder);
			parent = library.node(folder).parent;
		}

		self.expansion += 1;
		self.selected = Some(id);
		self.sync(library, filter);
		self.ensure_visible();
	}

	fn rebuild(&mut self, library: &Library, filter: &str) {
		let kept = if filter.is_empty() {
			None
		} else {
			Some(self.matches(library, filter))
		};

		self.rows.clear();
		let mut stack = vec![(library.root(), 0u16)];
		while let Some((id, depth)) = stack.pop() {
			if kept.as_ref().is_some_and(|kept| !kept[id.index()]) {
				continue;
			}

			let node = library.node(id);
			let open = library.is_folder(id) && (kept.is_some() || self.expanded.contains(&id));
			self.rows.push(Row { id, depth, open });

			if open {
				stack.extend(node.children.iter().rev().map(|&child| (child, depth + 1)));
			}
		}
	}

	fn matches(&mut self, library: &Library, filter: &str) -> Vec<bool> {
		let mut kept = vec![false; library.len()];
		self.matching_files = 0;

		for id in library.ids().rev() {
			let node = library.node(id);
			if find_ignore_case(&node.name, filter).is_some() {
				kept[id.index()] = true;
				if !library.is_folder(id) {
					self.matching_files += 1;
				}
			}

			if kept[id.index()]
				&& let Some(parent) = node.parent
			{
				kept[parent.index()] = true;
			}
		}
		kept
	}

	fn reselect(&mut self) {
		let found = self
			.selected
			.and_then(|selected| self.rows.iter().position(|row| row.id == selected));
		match found {
			Some(index) => self.selected_index = index,
			None => self.select_index(0),
		}
		self.clamp_scroll();
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
