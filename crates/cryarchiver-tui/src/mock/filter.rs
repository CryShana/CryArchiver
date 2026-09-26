use std::collections::BTreeMap;

use super::MockSource;
use crate::format::find_ignore_case;
use crate::view::{FilterEntry, FilterResult, FilterRow, NodeId};

struct Matches {
	kept: Vec<bool>,
	files: BTreeMap<NodeId, Vec<usize>>,
}

impl MockSource {
	pub(super) fn set_filter(&mut self, query: String) {
		self.filter = (!query.is_empty()).then(|| self.compute_filter(query));
		self.rows_version += 1;
	}

	/// New files can add matches, so the result is rebuilt and the version bumped only after an insert. Otherwise
	/// the statuses are patched into the rows in place, as `ArchiveSource::rows_version` promises.
	pub(super) fn refresh_filter(&mut self, inserted: bool) {
		let Some(query) = self.filter.as_ref().map(|filter| filter.query.clone()) else {
			return;
		};

		if inserted {
			self.filter = Some(self.compute_filter(query));
			self.rows_version += 1;
			return;
		}

		let files = &self.files;
		for row in self.filter.iter_mut().flat_map(|filter| filter.rows.iter_mut()) {
			if let FilterEntry::File { row, .. } = &mut row.entry {
				row.status.clone_from(&files.get(row.id).status);
			}
		}
	}

	fn compute_filter(&self, query: String) -> FilterResult {
		let matches = self.matches(&query);
		let matching_files = matches.files.values().map(Vec::len).sum();

		let mut rows = Vec::new();
		self.filtered_rows(self.view.library.root(), 0, &matches, &mut rows);
		FilterResult {
			query,
			rows,
			matching_files,
		}
	}

	fn matches(&self, query: &str) -> Matches {
		let library = &self.view.library;
		let mut files: BTreeMap<NodeId, Vec<usize>> = BTreeMap::new();
		for folder in library.ids() {
			let hits: Vec<usize> = self
				.files
				.in_folder(folder)
				.iter()
				.enumerate()
				.filter(|(_, row)| find_ignore_case(&row.name, query).is_some())
				.map(|(index, _)| index)
				.collect();
			if !hits.is_empty() {
				files.insert(folder, hits);
			}
		}

		let mut kept = vec![false; library.len()];
		for folder in library.ids().rev() {
			let node = library.node(folder);
			kept[folder.index()] |= files.contains_key(&folder) || find_ignore_case(&node.name, query).is_some();
			if kept[folder.index()]
				&& let Some(parent) = node.parent
			{
				kept[parent.index()] = true;
			}
		}

		Matches { kept, files }
	}

	fn filtered_rows(&self, folder: NodeId, depth: u16, matches: &Matches, rows: &mut Vec<FilterRow>) {
		if !matches.kept[folder.index()] {
			return;
		}

		rows.push(FilterRow {
			depth,
			entry: FilterEntry::Folder(folder),
		});

		for &child in &self.view.library.node(folder).children {
			self.filtered_rows(child, depth + 1, matches, rows);
		}

		let listed = self.files.in_folder(folder);
		for &index in matches.files.get(&folder).into_iter().flatten() {
			rows.push(FilterRow {
				depth: depth + 1,
				entry: FilterEntry::File {
					folder,
					row: listed[index].clone(),
				},
			});
		}
	}
}
