use std::collections::{BTreeMap, BTreeSet};

use super::MockSource;
use crate::format::find_ignore_case;
use crate::view::{FileId, FileRow, FileState, Filter, FilterEntry, FilterResult, FilterRow, NodeId, StateFilter};

struct Matches {
	kept: Vec<bool>,
	files: BTreeMap<NodeId, Vec<usize>>,
	/// Under an issues filter, the issues that belong to a folder rather than a file, each counted like the Overall
	/// panel counts it.
	folder_issues: usize,
}

impl MockSource {
	pub(super) fn set_filter(&mut self, filter: Filter) {
		self.filter = (!filter.is_empty()).then(|| self.compute_filter(filter));
		self.rows_version += 1;
	}

	/// New files can add matches, and with a state set so can every status change, so the result is rebuilt; the
	/// version is bumped only when the rows changed. Otherwise the statuses are patched into the rows in place, as
	/// `ArchiveSource::rows_version` promises.
	pub(super) fn refresh_filter(&mut self, inserted: bool) {
		let Some(current) = &self.filter else {
			return;
		};

		if inserted || current.state.is_some() {
			let fresh = self.compute_filter(Filter {
				query: current.query.clone(),
				state: current.state,
			});
			if !same_rows(current, &fresh) {
				self.rows_version += 1;
			}
			self.filter = Some(fresh);
			return;
		}

		let files = &self.files;
		for row in self.filter.iter_mut().flat_map(|filter| filter.rows.iter_mut()) {
			if let FilterEntry::File { row, .. } = &mut row.entry {
				row.status.clone_from(&files.get(row.id).status);
			}
		}
	}

	fn compute_filter(&self, filter: Filter) -> FilterResult {
		let matches = self.matches(&filter);
		let matching_files = matches.files.values().map(Vec::len).sum::<usize>() + matches.folder_issues;

		let mut rows = Vec::new();
		self.filtered_rows(self.view.library.root(), 0, &matches, &mut rows);
		FilterResult {
			query: filter.query,
			state: filter.state,
			rows,
			matching_files,
		}
	}

	fn matches(&self, filter: &Filter) -> Matches {
		let library = &self.view.library;
		let with_issues: BTreeSet<FileId> = self.view.issues.iter().filter_map(|issue| issue.file).collect();
		let passes = |row: &FileRow| {
			let in_state = filter.state.is_none_or(|state| match state {
				StateFilter::Issues => with_issues.contains(&row.id),
				StateFilter::Kept => row.status.state == FileState::KeptOriginal,
				StateFilter::Duplicates => row.status.state == FileState::Duplicate,
			});
			in_state && (filter.query.is_empty() || find_ignore_case(&row.name, &filter.query).is_some())
		};

		let mut files: BTreeMap<NodeId, Vec<usize>> = BTreeMap::new();
		for folder in library.ids() {
			let hits: Vec<usize> = self
				.files
				.in_folder(folder)
				.iter()
				.enumerate()
				.filter(|(_, row)| passes(row))
				.map(|(index, _)| index)
				.collect();
			if !hits.is_empty() {
				files.insert(folder, hits);
			}
		}

		let mut kept = vec![false; library.len()];
		let issue_folders: Vec<NodeId> = self
			.view
			.issues
			.iter()
			.filter(|issue| filter.state == Some(StateFilter::Issues) && issue.file.is_none())
			.filter_map(|issue| issue.folder)
			.filter(|&folder| {
				filter.query.is_empty() || find_ignore_case(&library.node(folder).name, &filter.query).is_some()
			})
			.collect();
		for &folder in &issue_folders {
			kept[folder.index()] = true;
		}

		for folder in library.ids().rev() {
			let node = library.node(folder);
			kept[folder.index()] |=
				files.contains_key(&folder) || find_ignore_case(&node.name, &filter.query).is_some();
			if kept[folder.index()]
				&& let Some(parent) = node.parent
			{
				kept[parent.index()] = true;
			}
		}

		Matches {
			kept,
			files,
			folder_issues: issue_folders.len(),
		}
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

fn same_rows(old: &FilterResult, new: &FilterResult) -> bool {
	old.rows.len() == new.rows.len() && old.rows.iter().zip(&new.rows).all(|(old, new)| same_row(old, new))
}

fn same_row(old: &FilterRow, new: &FilterRow) -> bool {
	old.depth == new.depth
		&& match (&old.entry, &new.entry) {
			(FilterEntry::Folder(old), FilterEntry::Folder(new)) => old == new,
			(FilterEntry::File { row: old, .. }, FilterEntry::File { row: new, .. }) => old.id == new.id,
			_ => false,
		}
}
