//! Dedup in the run (project layer spec §7): hashing gates a ready file in a shared size group, the canonical copy is
//! the shallowest, duplicates wait for it and become placeholders, and a later shallower copy takes the archived
//! output over by rename. That swap is two ordinary commits: the canonical copy's move, then the placeholder.

use std::collections::HashMap;
use std::time::Instant;

use cryarchiver_engine::commit::file_id;

use super::jobs::{Made, Task};
use super::owner::{Message, Owner};
use crate::dedup::{self, MIN_DEDUP_SIZE, Member};
use crate::record::{FileState, Hash, How, Identity, IssueKind, Output};
use crate::run::placeholder_text;
use crate::schedule::Work;
use crate::tree::FileId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HashKind {
	Window,
	Full,
}

/// A hash job: its kind, and the file's size and time when it started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HashRequest {
	kind: HashKind,
	size: u64,
	mtime_ns: i64,
}

#[derive(Debug, Default)]
pub(crate) struct DedupState {
	by_size: HashMap<u64, Vec<FileId>>,
	pending: HashMap<FileId, HashKind>,
	/// Duplicate to canonical copy.
	canonical_of: HashMap<FileId, FileId>,
	/// Canonical copy to the member whose archived output it takes over by rename.
	moves: HashMap<FileId, FileId>,
}

impl Owner {
	/// Takes a ready file into dedup when it is large enough; its group then decides when it is queued.
	pub(super) fn gate_duplicates(&mut self, file: FileId) -> bool {
		let size = self.tree.file(file).record.size;
		if size < MIN_DEDUP_SIZE {
			return false;
		}

		let members = self.dedup.by_size.entry(size).or_default();
		if !members.contains(&file) {
			members.push(file);
		}
		self.plan_size_group(size);
		true
	}

	/// Drops a file from dedup, when it is excluded, changed or gone, and lets its group decide again.
	pub(super) fn leave_dedup(&mut self, file: FileId) {
		self.dedup.canonical_of.remove(&file);
		self.dedup.moves.remove(&file);
		self.dedup.pending.remove(&file);

		let size = self.tree.file(file).record.size;
		let Some(members) = self.dedup.by_size.get_mut(&size) else {
			return;
		};
		let before = members.len();
		members.retain(|member| *member != file);
		if members.len() != before {
			self.plan_size_group(size);
		}
	}

	/// A file the last run archived joins its size group, so a copy found later is recognised.
	pub(super) fn join_dedup(&mut self, file: FileId) {
		let size = self.tree.file(file).record.size;
		if size >= MIN_DEDUP_SIZE {
			self.dedup.by_size.entry(size).or_default().push(file);
		}
	}

	/// On open, every group with company decides again: a swap a crash cut short between its two commits gets its
	/// placeholder.
	pub(super) fn plan_dedup_groups(&mut self) {
		let mut sizes: Vec<u64> = self
			.dedup
			.by_size
			.iter()
			.filter(|(_, members)| members.len() > 1)
			.map(|(size, _)| *size)
			.collect();
		sizes.sort_unstable();

		for size in sizes {
			self.plan_size_group(size);
		}
	}

	/// A member became Done: the group may now release its duplicates or need a swap.
	pub(super) fn member_done(&mut self, file: FileId) {
		if let Some(holder) = self.dedup.moves.remove(&file) {
			self.route_duplicate(holder, file);
		}

		let size = self.tree.file(file).record.size;
		if self
			.dedup
			.by_size
			.get(&size)
			.is_some_and(|members| members.contains(&file))
		{
			self.plan_size_group(size);
		}
	}

	fn members(&self, size: u64) -> Vec<Member> {
		let ids = self.dedup.by_size.get(&size).cloned().unwrap_or_default();
		ids.into_iter()
			.filter(|&id| {
				let file = self.tree.file(id);
				let hashed = file.record.full_hash.is_some();
				!file.removed
					&& match file.record.state {
						FileState::Excluded
						| FileState::Discovered
						| FileState::Issue {
							kind: IssueKind::SourceUnreadable,
							..
						} => false,
						FileState::SourceGone(_) | FileState::DeletePending(_) | FileState::OriginalDeleted(_) => {
							hashed
						}
						_ => true,
					}
			})
			.map(|id| {
				let file = self.tree.file(id);
				Member {
					id,
					path: file.key.clone(),
					identity: file.record.identity,
					window: file.record.window_hash,
					full: file.record.full_hash,
				}
			})
			.collect()
	}

	fn plan_size_group(&mut self, size: u64) {
		let plan = dedup::plan_group(&self.members(size));

		for file in plan.window_needed {
			self.request_hash(file, HashKind::Window);
		}

		for file in plan.full_needed {
			self.request_hash(file, HashKind::Full);
		}

		for file in plan.unique {
			let was_duplicate = self.dedup.canonical_of.remove(&file).is_some();
			let was_moving = self.dedup.moves.remove(&file).is_some();
			if was_duplicate || was_moving || self.tree.file(file).record.state == FileState::Ready {
				self.reroute(file);
			}
		}

		for set in plan.sets {
			self.apply_set(&set);
		}
	}

	/// Puts a file that isn't running back into the right queue for its current role.
	fn reroute(&mut self, file: FileId) {
		if self.pipeline.jobs.contains_key(&file) {
			return;
		}

		if !matches!(self.tree.file(file).record.state, FileState::Ready | FileState::Queued) {
			return;
		}

		for queue in self.pipeline.queues.values_mut() {
			queue.remove(file);
		}
		self.pipeline.waits.forget(file);
		self.enqueue(file);
	}

	fn request_hash(&mut self, file: FileId, kind: HashKind) {
		if self.dedup.pending.contains_key(&file) {
			return;
		}

		self.dedup.pending.insert(file, kind);
		if let Some(queue) = self.pipeline.queues.get_mut(&Work::Hash) {
			queue.push(file);
		}
		self.touch_file(file);
	}

	pub(super) fn hashing(&self, file: FileId) -> bool {
		self.dedup.pending.contains_key(&file)
	}

	pub(super) fn hashes_pending(&self) -> bool {
		!self.dedup.pending.is_empty()
	}

	pub(super) fn start_hash(&mut self, file: FileId, now: Instant) {
		let Some(&kind) = self.dedup.pending.get(&file) else {
			return;
		};

		self.pipeline.governor.started(Work::Hash, now);
		let entry = self.tree.file(file);
		let request = HashRequest {
			kind,
			size: entry.record.size,
			mtime_ns: entry.record.mtime_ns,
		};
		let path = self.source_path(&entry.key);
		let messages = self.messages.clone();
		tokio::task::spawn_blocking(move || {
			let hashed = match kind {
				HashKind::Window => {
					let identity = file_id(&path).ok().map(Identity::from);
					dedup::window_hash(&path).map(|hash| (identity, hash))
				}
				HashKind::Full => dedup::full_hash(&path).map(|hash| (None, hash)),
			};
			let _ = messages.send(Message::Hashed(file, request, hashed.map_err(|err| err.to_string())));
		});
	}

	/// A window hash comes with the file's identity, read in the same visit, so a hard link or a link to a file
	/// already in the tree needs no full hash. A result for a file that changed since its job started is dropped:
	/// the request its change made is still pending.
	pub(super) fn on_hashed(
		&mut self,
		file: FileId,
		request: HashRequest,
		hash: Result<(Option<Identity>, Hash), String>,
	) {
		self.pipeline.governor.finished(Work::Hash);
		let record = &self.tree.file(file).record;
		let current = (record.size, record.mtime_ns) == (request.size, request.mtime_ns);
		if !current || self.dedup.pending.get(&file) != Some(&request.kind) {
			return;
		}

		let kind = request.kind;
		self.dedup.pending.remove(&file);
		let size = record.size;

		match hash {
			Ok((identity, hash)) => {
				let mut record = self.tree.file(file).record.clone();
				match kind {
					HashKind::Window => {
						record.window_hash = Some(hash);
						record.identity = identity;
					}
					HashKind::Full => record.full_hash = Some(hash),
				}
				self.tree.set_record(file, record);
				self.save_file(file);
				self.touch_file(file);
			}
			Err(reason) if self.tree.file(file).record.state == FileState::Ready => {
				self.set_state(
					file,
					FileState::Issue {
						kind: IssueKind::SourceUnreadable,
						attempt: 0,
						reason,
						output: None,
					},
				);
			}
			Err(_) => {
				if let Some(members) = self.dedup.by_size.get_mut(&size) {
					members.retain(|member| *member != file);
				}
			}
		}

		self.plan_size_group(size);
	}

	/// The first member is the canonical copy; the others become its placeholders.
	fn apply_set(&mut self, set: &[FileId]) {
		let canonical = set[0];
		self.dedup.canonical_of.remove(&canonical);
		for &duplicate in &set[1..] {
			self.dedup.canonical_of.insert(duplicate, canonical);
		}

		let unstarted = matches!(
			self.tree.file(canonical).record.state,
			FileState::Ready | FileState::Queued
		) && !self.pipeline.jobs.contains_key(&canonical);
		if unstarted {
			let holder = set[1..].iter().copied().find(|member| self.holds_content(*member));
			match holder {
				Some(holder) => {
					self.dedup.moves.insert(canonical, holder);
				}
				None => {
					self.dedup.moves.remove(&canonical);
				}
			}

			self.reroute(canonical);
		}

		for &duplicate in &set[1..] {
			self.route_duplicate(duplicate, canonical);
		}
	}

	fn route_duplicate(&mut self, duplicate: FileId, canonical: FileId) {
		let state = self.tree.file(duplicate).record.state.clone();
		let placeholder_in_place = matches!(
			state.output().map(|output| &output.how),
			Some(How::Placeholder { canonical: pointed }) if *pointed == self.tree.file(canonical).key
		);
		let giving_its_output = self.dedup.moves.get(&canonical) == Some(&duplicate);
		if placeholder_in_place || giving_its_output || self.pipeline.jobs.contains_key(&duplicate) {
			return;
		}

		match &state {
			FileState::Done(output) => {
				let mut record = self.tree.file(duplicate).record.clone();
				record.replaces = Some(output.path.clone());
				self.tree.set_record(duplicate, record);
			}
			FileState::Ready | FileState::Queued => {}
			_ => return,
		}

		self.set_state(duplicate, FileState::Queued);
		for queue in self.pipeline.queues.values_mut() {
			queue.remove(duplicate);
		}
		self.pipeline.waits.forget(duplicate);

		if self.canonical_output(canonical).is_some() {
			self.enqueue(duplicate);
		} else {
			self.pipeline.waits.wait(duplicate, [canonical]);
		}
	}

	/// Done with an output that holds the content itself: encoded or copied, not a placeholder.
	fn holds_content(&self, file: FileId) -> bool {
		match &self.tree.file(file).record.state {
			FileState::Done(output) => !matches!(output.how, How::Placeholder { .. }),
			_ => false,
		}
	}

	/// The archived content a placeholder may point at. A canonical copy whose source is gone or deleted keeps its
	/// output, and it never becomes Done again, so a duplicate waiting for that would wait forever.
	fn canonical_output(&self, canonical: FileId) -> Option<&Output> {
		match &self.tree.file(canonical).record.state {
			FileState::Done(output)
			| FileState::SourceGone(output)
			| FileState::DeletePending(output)
			| FileState::OriginalDeleted(output) => (!matches!(output.how, How::Placeholder { .. })).then_some(output),
			_ => None,
		}
	}

	/// A duplicate's placeholder, or a canonical copy's move of the archived output; `None` (and a wait) while the
	/// canonical copy isn't archived yet.
	pub(super) fn duplicate_task(&mut self, file: FileId) -> Option<Task> {
		if let Some(&holder) = self.dedup.moves.get(&file) {
			let output = self.tree.file(holder).record.state.output()?.clone();
			let extension = std::path::Path::new(&output.path.name())
				.extension()
				.map(|extension| extension.to_string_lossy().into_owned())
				.unwrap_or_default();
			return Some(Task::Move {
				from: self.destination_path(&output.path),
				made: Made::Moved {
					how: output.how,
					hash: output.hash,
					extension,
				},
			});
		}

		let canonical = *self.dedup.canonical_of.get(&file)?;
		let Some(output) = self.canonical_output(canonical).cloned() else {
			self.pipeline.waits.wait(file, [canonical]);
			return None;
		};

		let entry = self.tree.file(file);
		let link = output.path.relative_from(&self.destination_folder(entry.folder));
		let text = placeholder_text(&link, &entry.key.display(), entry.record.size, entry.record.full_hash);
		Some(Task::Write {
			content: text.into_bytes(),
			made: Made::Placeholder {
				canonical: self.tree.file(canonical).key.clone(),
			},
		})
	}

	pub(super) fn is_duplicate(&self, file: FileId) -> bool {
		self.dedup.canonical_of.contains_key(&file) || self.dedup.moves.contains_key(&file)
	}

	pub(super) fn canonical_of(&self, file: FileId) -> Option<FileId> {
		self.dedup.canonical_of.get(&file).copied()
	}

	pub(super) fn duplicates_of(&self, canonical: FileId) -> Vec<FileId> {
		let mut duplicates: Vec<FileId> = self
			.dedup
			.canonical_of
			.iter()
			.filter(|(_, of)| **of == canonical)
			.map(|(duplicate, _)| *duplicate)
			.collect();
		duplicates.sort();
		duplicates
	}
}
