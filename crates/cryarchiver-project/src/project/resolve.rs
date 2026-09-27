//! The user's resolutions of issues: Retry, Accept or Exclude, as each kind allows (project layer spec §10.1).

use std::collections::HashMap;
use std::fs;
use std::io;

use super::owner::{Message, Owner};
use crate::dedup::full_hash;
use crate::issues::{IssueId, IssueKey, Problem, Verb};
use crate::record::{FileState, IssueKind, Output};
use crate::tree::{FileId, FolderId};
use crate::view::{LogKind, NoticeLevel};

/// Output issues whose archived file is being hashed off the project task, and what waits for the answer.
#[derive(Debug, Default)]
pub(crate) struct VerifyState {
	waiting: HashMap<FileId, Vec<Deferred>>,
	/// The answer, while what waited for it runs: the file, the output as hashed, and whether it matched.
	answer: Option<(FileId, Output, bool)>,
}

/// What an output issue's file waits to do until its archived file is verified.
#[derive(Debug)]
pub(crate) enum Deferred {
	Changed { size: u64, mtime_ns: i64 },
	IdentityChanged { size: u64, mtime_ns: i64 },
	Missing,
	Broken(String),
	Resolve(Verb),
}

impl Owner {
	pub(super) fn resolve(&mut self, issue: IssueId, verb: Verb) {
		match self.issue_ids.key(issue).cloned() {
			Some(IssueKey::File(file)) => self.resolve_file(file, verb),
			Some(IssueKey::Cycle(folder)) if verb == Verb::Accept => self.acknowledge_cycle(folder),
			Some(IssueKey::Unknown(path)) if verb == Verb::Accept => self.acknowledge_unknown(&path),
			Some(IssueKey::Unreadable(folder)) if verb == Verb::Retry => {
				self.queue_listing(self.tree.folder(folder).key.clone(), true);
			}
			Some(IssueKey::Unreadable(folder)) if verb == Verb::Accept && folder != FolderId::ROOT => {
				self.acknowledge_unreadable(folder);
			}
			Some(IssueKey::Unknown(_) | IssueKey::Cycle(_) | IssueKey::Unreadable(_)) | None => {
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
		if self.waits_for_verification(file, Deferred::Resolve(verb)) {
			return;
		}

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

		if self.verify_first(file, Deferred::Resolve(verb)) {
			return;
		}

		if verb == Verb::Retry {
			self.forget_identity_changes(file);
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
				self.leave_dedup(file);
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

	/// "Accept as is": an output still as it was archived stays as it is, hash and placeholders included. Otherwise the
	/// output as it now is on disk becomes the archive with no hash, so it can never be a canonical copy, and the
	/// placeholders that pointed at it are archived again from their own sources; a missing output leaves the file
	/// excluded.
	fn accept_output(&mut self, file: FileId, output: Option<&Output>) {
		if let Some(output) = output
			&& self.output_intact(file, output)
		{
			self.set_state(file, FileState::Done(output.clone()));
			self.file_settled(file);
			self.queue_delete(file);
			return;
		}

		let current = output.and_then(|output| {
			let size = fs::metadata(self.destination_path(&output.path)).ok()?.len();
			Some(Output {
				size,
				hash: None,
				..output.clone()
			})
		});
		self.transfer_dependents(file, None);

		if let Some(current) = current {
			self.set_state(file, FileState::Done(current));
		} else {
			self.set_state(file, FileState::Excluded);
		}
		self.leave_dedup(file);
		self.file_settled(file);
	}

	/// Whether the output an issue of `file` is about is on disk as it was archived: only then may a placeholder adopt
	/// it, or Accept keep it as it was. Only an answer from `verify_first` says so.
	pub(super) fn output_intact(&self, file: FileId, output: &Output) -> bool {
		self.verify
			.answer
			.as_ref()
			.is_some_and(|(verified, hashed, intact)| *verified == file && hashed == output && *intact)
	}

	/// Settling an output issue's dependents, or accepting its output as it was, needs the archived file hashed while
	/// its size still matches. That runs off the project task, and `then` waits for the answer, after whatever already
	/// waits for it. Returns whether it waits.
	pub(super) fn verify_first(&mut self, file: FileId, then: Deferred) -> bool {
		if self.verify.waiting.contains_key(&file) {
			return self.waits_for_verification(file, then);
		}

		let Some(output) = self.output_to_verify(file, &then) else {
			return false;
		};
		let (path, hash) = (self.destination_path(&output.path), output.hash);
		let messages = self.messages.clone();
		tokio::task::spawn_blocking(move || {
			let intact = hash.is_some_and(|hash| full_hash(&path).is_ok_and(|found| found == hash));
			let _ = messages.send(Message::OutputVerified(file, output, intact));
		});
		self.verify.waiting.insert(file, vec![then]);
		true
	}

	/// Whatever comes for a file while its issue's output is being verified runs after what already waits, as it would
	/// have. Returns whether it waits.
	fn waits_for_verification(&mut self, file: FileId, then: Deferred) -> bool {
		let Some(waiting) = self.verify.waiting.get_mut(&file) else {
			return false;
		};

		waiting.push(then);
		true
	}

	/// The issue's output, when `then` needs to know whether it is intact, no answer is at hand, and only its hash can
	/// tell: a missing output, or one of another size, isn't intact.
	fn output_to_verify(&self, file: FileId, then: &Deferred) -> Option<Output> {
		let entry = self.tree.file(file);
		let FileState::Issue {
			output: Some(output), ..
		} = &entry.record.state
		else {
			return None;
		};

		let needed = matches!(then, Deferred::Resolve(Verb::Accept)) || self.has_dependents(file);
		let answered = self
			.verify
			.answer
			.as_ref()
			.is_some_and(|(verified, hashed, _)| *verified == file && hashed == output);
		let sized = fs::symlink_metadata(self.destination_path(&output.path))
			.is_ok_and(|meta| meta.is_file() && meta.len() == output.size);

		(!entry.removed && needed && !answered && output.hash.is_some() && sized).then(|| output.clone())
	}

	/// The answer came back: what waited for it runs now, in order, with the answer at hand.
	pub(super) fn on_output_verified(&mut self, file: FileId, output: Output, intact: bool) {
		let Some(waiting) = self.verify.waiting.remove(&file) else {
			return;
		};

		self.verify.answer = Some((file, output, intact));
		for then in waiting {
			self.run_deferred(file, then);
		}
		self.verify.answer = None;
	}

	fn run_deferred(&mut self, file: FileId, then: Deferred) {
		let entry = self.tree.file(file);
		let (folder, name) = (entry.folder, entry.name.clone());
		match then {
			Deferred::Changed { size, mtime_ns } => self.source_changed(file, size, mtime_ns),
			Deferred::IdentityChanged { size, mtime_ns } => self.identity_changed(file, size, mtime_ns),
			Deferred::Missing => {
				let missing = fs::symlink_metadata(self.source_path(&entry.key))
					.is_err_and(|err| err.kind() == io::ErrorKind::NotFound);
				if missing {
					self.source_missing(file);
				} else {
					self.queue_listing(self.tree.folder(folder).key.clone(), false);
				}
			}
			Deferred::Broken(reason) => self.source_broken(folder, &name, &reason),
			Deferred::Resolve(verb) => self.resolve_file(file, verb),
		}
	}

	pub(super) fn verifying(&self) -> bool {
		!self.verify.waiting.is_empty()
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
