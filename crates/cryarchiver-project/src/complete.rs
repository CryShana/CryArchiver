//! The completion check (project layer spec §10.3): every source file should end up as exactly one destination file.

use std::collections::HashSet;
use std::hash::BuildHasher;

use crate::keys::RelPath;
use crate::record::FileState;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParityReport {
	pub source_files: u64,
	pub archive_files: u64,
	pub excluded: u64,
	/// Source files with no destination file.
	pub missing: Vec<RelPath>,
}

impl ParityReport {
	pub fn is_clean(&self) -> bool {
		self.missing.is_empty() && self.source_files == self.archive_files + self.excluded
	}

	/// "N archived", or "N archived + K excluded".
	pub fn summary(&self) -> String {
		if self.excluded == 0 {
			format!("{} archived", self.archive_files)
		} else {
			format!("{} archived + {} excluded", self.archive_files, self.excluded)
		}
	}
}

/// Compares the project's files with the files the destination walk found. Files whose source was deleted by the
/// user no longer count as source files; their outputs stay in the archive.
pub fn check<'a, S: BuildHasher>(
	files: impl IntoIterator<Item = (&'a RelPath, &'a FileState)>,
	destination: &HashSet<RelPath, S>,
) -> ParityReport {
	let mut report = ParityReport::default();
	for (key, state) in files {
		match state {
			FileState::SourceGone(_) => {}
			FileState::Excluded => {
				report.source_files += 1;
				report.excluded += 1;
			}
			state => {
				report.source_files += 1;
				match state.output() {
					Some(output) if destination.contains(&output.path) => report.archive_files += 1,
					_ => report.missing.push(key.clone()),
				}
			}
		}
	}

	report.missing.sort();
	report
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::record::{CopyReason, How, IssueKind, Output};

	fn path(text: &str) -> RelPath {
		RelPath::from_bytes(text.as_bytes().to_vec())
	}

	fn done(output: &str) -> FileState {
		FileState::Done(Output {
			how: How::Copied(CopyReason::NotMedia),
			path: path(output),
			size: 1,
			hash: None,
		})
	}

	#[test]
	fn every_source_file_has_one_destination_file() {
		let destination = HashSet::from([path("a.avif"), path("b.txt")]);
		let files = [(path("a.jpg"), done("a.avif")), (path("b.txt"), done("b.txt"))];

		let report = check(files.iter().map(|(key, state)| (key, state)), &destination);

		assert!(report.is_clean());
		assert_eq!(report.summary(), "2 archived");
	}

	#[test]
	fn excluded_files_are_counted_apart_and_gaps_are_listed() {
		let destination = HashSet::from([path("a.avif")]);
		let files = [
			(path("a.jpg"), done("a.avif")),
			(path("b.jpg"), FileState::Excluded),
			(path("c.jpg"), done("c.avif")),
			(
				path("d.jpg"),
				FileState::Issue {
					kind: IssueKind::Unrecoverable { damaged: true },
					attempt: 1,
					reason: String::new(),
					output: None,
				},
			),
		];

		let report = check(files.iter().map(|(key, state)| (key, state)), &destination);

		assert_eq!(report.summary(), "1 archived + 1 excluded");
		assert_eq!(report.missing, [path("c.jpg"), path("d.jpg")]);
		assert!(!report.is_clean());
	}

	#[test]
	fn deleted_sources_no_longer_count() {
		let destination = HashSet::from([path("a.avif")]);
		let gone = FileState::SourceGone(Output {
			how: How::Encoded,
			path: path("a.avif"),
			size: 1,
			hash: None,
		});

		let report = check([(&path("a.jpg"), &gone)], &destination);

		assert_eq!(report.source_files, 0);
		assert!(report.is_clean());
	}
}
