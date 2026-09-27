//! The user's resolutions of issues: Retry, Accept or Exclude, as each kind allows (project layer spec §10.1).

use std::fs;

use super::owner::Owner;
use crate::dedup::full_hash;
use crate::issues::{IssueId, IssueKey, Problem, Verb};
use crate::record::{FileState, IssueKind, Output};
use crate::tree::{FileId, FolderId};
use crate::view::{LogKind, NoticeLevel};

impl Owner {
	pub(super) fn resolve(&mut self, issue: IssueId, verb: Verb) {
		match self.issue_ids.key(issue).cloned() {
			Some(IssueKey::File(file)) => self.resolve_file(file, verb),
			Some(IssueKey::Cycle(folder)) if verb == Verb::Accept => self.acknowledge_cycle(folder),
			Some(IssueKey::Unknown(path)) if verb == Verb::Accept => self.acknowledge_unknown(&path),
			Some(IssueKey::Unknown(_) | IssueKey::Cycle(_)) | None => {
				self.notice(
					NoticeLevel::Warning,
					"that resolution doesn't apply to this issue".into(),
				);
			}
		}
	}

	pub(super) fn retry_failed_in(&mut self, folder: FolderId) {
		let root = self.tree.folder(folder).key.clone();
		let failed: Vec<FileId> = self
			.issue_files
			.iter()
			.copied()
			.filter(|file| self.tree.file(*file).key.is_within(&root))
			.filter(|file| self.problem(*file).is_some_and(|problem| problem.allows(Verb::Retry)))
			.collect();

		for file in failed {
			self.resolve_file(file, Verb::Retry);
		}
	}

	fn problem(&self, file: FileId) -> Option<Problem> {
		match &self.tree.file(file).record.state {
			FileState::Issue { kind, .. } => Some(Problem::of(*kind)),
			_ => None,
		}
	}

	fn resolve_file(&mut self, file: FileId, verb: Verb) {
		let FileState::Issue { kind, output, .. } = self.tree.file(file).record.state.clone() else {
			return;
		};
		if !Problem::of(kind).allows(verb) {
			self.notice(
				NoticeLevel::Warning,
				"that resolution doesn't apply to this issue".into(),
			);
			return;
		}

		let name = self.tree.file(file).key.display();
		match (kind, verb) {
			(IssueKind::Unrecoverable { .. }, Verb::Retry) => self.enqueue(file),
			(IssueKind::Unrecoverable { .. }, Verb::Accept) => {
				self.pipeline.chosen_copies.insert(file);
				self.enqueue(file);
			}
			(IssueKind::SourceUnreadable, Verb::Retry) => {
				self.set_state(file, FileState::Discovered);
				self.observe(file);
			}
			(IssueKind::OutputMissing, Verb::Retry) => {
				let adopted = self.give_up_output(file);
				let mut record = self.tree.file(file).record.clone();
				record.replaces = output.filter(|_| !adopted).map(|output| output.path);
				self.tree.set_record(file, record);
				self.set_state(file, FileState::Discovered);
				self.observe(file);
			}
			(IssueKind::OutputMissing, Verb::Accept) => self.accept_output(file, output.as_ref()),
			(_, Verb::Exclude) => {
				self.give_up_output(file);
				self.set_state(file, FileState::Excluded);
				self.leave_dedup(file);
				self.file_settled(file);
			}
			_ => return,
		}

		self.log(LogKind::Action, format!("{name}: {} chosen", verb_label(verb)));
	}

	/// "Accept as is": the output as it now is on disk becomes the archive. Nothing can be verified against it, so it
	/// carries no hash; a missing output leaves the file excluded. The placeholders that point at it are archived again
	/// from their own sources, unless the output is still as it was archived.
	fn accept_output(&mut self, file: FileId, output: Option<&Output>) {
		let current = output.and_then(|output| {
			let size = fs::metadata(self.destination_path(&output.path)).ok()?.len();
			Some(Output {
				size,
				hash: None,
				..output.clone()
			})
		});

		if current.is_none() || !self.verified_for_dependents(file, output) {
			self.transfer_dependents(file, output, false);
		}

		if let Some(current) = current {
			self.set_state(file, FileState::Done(current));
		} else {
			self.set_state(file, FileState::Excluded);
		}
		self.file_settled(file);
	}

	/// Whether the recorded output is on disk as it was archived: only then may a placeholder adopt it. Blocking: reads
	/// the file when its size matches.
	pub(super) fn output_intact(&self, output: &Output) -> bool {
		let path = self.destination_path(&output.path);
		let sized = fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file() && meta.len() == output.size);

		sized
			&& output
				.hash
				.is_some_and(|hash| full_hash(&path).is_ok_and(|found| found == hash))
	}

	fn acknowledge_cycle(&mut self, folder: FolderId) {
		if let Some(cycle) = &mut self.tree.folder_mut(folder).record.cycle {
			cycle.acknowledged = true;
		}
		self.save_folder(folder);
		self.view.issues_dirty = true;
	}
}

fn verb_label(verb: Verb) -> &'static str {
	match verb {
		Verb::Retry => "retry",
		Verb::Accept => "accept",
		Verb::Exclude => "exclude",
	}
}
