//! Dedup in the run (project layer spec §7): hashing gates a ready file in a shared size group, the canonical copy is
//! the shallowest, duplicates wait for it and become placeholders, and a later shallower copy takes the archived
//! output over by rename. That swap is two ordinary commits: the canonical copy's move, then the placeholder.
//!
//! A placeholder's content lives in its canonical copy's output, so that output is never replaced or removed while a
//! record depends on it: before the canonical copy gives it up, a placeholder adopts it.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;
use std::time::Instant;

use cryarchiver_engine::commit::file_id;

use super::jobs::{Made, Task};
use super::owner::{Message, Owner};
use super::pipeline::RunningJob;
use crate::dedup::{self, MIN_DEDUP_SIZE, Member};
use crate::keys::RelPath;
use crate::record::{FileState, Hash, How, Identity, IssueKind, Output, mtime_ns};
use crate::references;
use crate::run::{self, placeholder_text};
use crate::schedule::Work;
use crate::tree::FileId;
use crate::view::LogKind;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HashOutcome {
	Hashed {
		identity: Option<Identity>,
		hash: Hash,
	},
	/// The source no longer has the size and time the job started from.
	Changed {
		size: u64,
		mtime_ns: i64,
	},
	Gone,
	Unreadable(String),
}

#[derive(Debug, Default)]
pub(crate) struct DedupState {
	by_size: HashMap<u64, BTreeSet<FileId>>,
	/// Size groups to plan on the next tick.
	dirty: BTreeSet<u64>,
	pending: HashMap<FileId, HashKind>,
	/// Files whose identity was read in this session; a stored identity may belong to another file by now.
	fresh_identity: HashSet<FileId>,
	/// Duplicate to canonical copy.
	canonical_of: HashMap<FileId, FileId>,
	/// Canonical copy to the member whose archived output it takes over by rename.
	moves: HashMap<FileId, FileId>,
	/// A canonical copy's key to the files whose recorded output was a placeholder pointing at it; checked on use.
	placeholders: HashMap<RelPath, HashSet<FileId>>,
	/// Heirs' own placeholder files, removed once the batch that records their adoption is durable.
	remove_after_durable: Vec<(FileId, RelPath)>,
}

impl Owner {
	/// Takes a ready file into dedup when it is large enough and not a reference file; its group then decides when
	/// it is queued.
	pub(super) fn gate_duplicates(&mut self, file: FileId) -> bool {
		let entry = self.tree.file(file);
		let size = entry.record.size;
		if size < MIN_DEDUP_SIZE || references::kind_of(&entry.name).is_some() {
			return false;
		}

		self.dedup.by_size.entry(size).or_default().insert(file);
		self.dedup.dirty.insert(size);
		true
	}

	/// Drops a file from dedup, when it is excluded, changed or gone, and lets its group decide again.
	pub(super) fn leave_dedup(&mut self, file: FileId) {
		self.dedup.canonical_of.remove(&file);
		self.dedup.moves.remove(&file);
		self.dedup.pending.remove(&file);
		self.dedup.fresh_identity.remove(&file);

		let size = self.tree.file(file).record.size;
		if self
			.dedup
			.by_size
			.get_mut(&size)
			.is_some_and(|members| members.remove(&file))
		{
			self.dedup.dirty.insert(size);
		}
	}

	/// A file the last run archived joins its size group, so a copy found later is recognised.
	pub(super) fn join_dedup(&mut self, file: FileId) {
		let entry = self.tree.file(file);
		let size = entry.record.size;
		if size >= MIN_DEDUP_SIZE && references::kind_of(&entry.name).is_none() {
			self.dedup.by_size.entry(size).or_default().insert(file);
		}
	}

	/// On open, every group with company decides again: a swap a crash cut short between its two commits gets its
	/// placeholder.
	pub(super) fn plan_dedup_groups(&mut self) {
		let sizes = self
			.dedup
			.by_size
			.iter()
			.filter(|(_, members)| members.len() > 1)
			.map(|(size, _)| *size);
		self.dedup.dirty.extend(sizes);
	}

	/// Plans each group that changed since the last tick, once.
	pub(super) fn plan_dirty_groups(&mut self) {
		for size in std::mem::take(&mut self.dedup.dirty) {
			self.plan_size_group(size);
		}
	}

	pub(super) fn groups_planned(&self) -> bool {
		self.dedup.dirty.is_empty()
	}

	/// A member became Done: the group may now release its duplicates or need a swap.
	pub(super) fn member_done(&mut self, file: FileId) {
		if let Some(holder) = self.dedup.moves.remove(&file) {
			self.route_duplicate(holder, file, true);
		}

		let size = self.tree.file(file).record.size;
		if self
			.dedup
			.by_size
			.get(&size)
			.is_some_and(|members| members.contains(&file))
		{
			self.dedup.dirty.insert(size);
		}
	}

	fn members(&self, size: u64) -> Vec<Member> {
		let Some(ids) = self.dedup.by_size.get(&size) else {
			return Vec::new();
		};

		ids.iter()
			.copied()
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
				let identity = file
					.record
					.identity
					.filter(|_| !is_gone(&file.record.state) && self.dedup.fresh_identity.contains(&id));
				Member {
					id,
					path: file.key.clone(),
					identity,
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

		self.unqueue(file);
		self.enqueue(file);
	}

	/// Out of every queue and wait, with no hash pending.
	fn unqueue(&mut self, file: FileId) {
		for queue in self.pipeline.queues.values_mut() {
			queue.remove(file);
		}
		self.dedup.pending.remove(&file);
		self.pipeline.waits.forget(file);
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
			let outcome = hash_unchanged(&path, request);
			let _ = messages.send(Message::Hashed(file, request, outcome));
		});
	}

	/// A window hash comes with the file's identity, read in the same visit, so a hard link or a link to a file
	/// already in the tree needs no full hash. A result for a file that changed since its job started is dropped:
	/// the request its change made is still pending. A change the job itself saw is a change like any other.
	pub(super) fn on_hashed(&mut self, file: FileId, request: HashRequest, outcome: HashOutcome) {
		self.pipeline.governor.finished(Work::Hash);
		let record = &self.tree.file(file).record;
		let current = (record.size, record.mtime_ns) == (request.size, request.mtime_ns);
		if !current || self.dedup.pending.get(&file) != Some(&request.kind) {
			return;
		}

		self.dedup.pending.remove(&file);
		let size = record.size;

		match outcome {
			HashOutcome::Hashed { identity, hash } => {
				let mut record = self.tree.file(file).record.clone();
				match request.kind {
					HashKind::Window => {
						record.window_hash = Some(hash);
						record.identity = identity;
						if identity.is_some() {
							self.dedup.fresh_identity.insert(file);
						} else {
							self.dedup.fresh_identity.remove(&file);
						}
					}
					HashKind::Full => record.full_hash = Some(hash),
				}
				self.tree.set_record(file, record);
				self.save_file(file);
				self.touch_file(file);
			}
			HashOutcome::Changed { size, mtime_ns } => {
				self.source_changed(file, size, mtime_ns);
				return;
			}
			HashOutcome::Gone => {
				self.source_missing(file);
				return;
			}
			HashOutcome::Unreadable(reason) if self.tree.file(file).record.state == FileState::Ready => {
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
			HashOutcome::Unreadable(_) => {
				if let Some(members) = self.dedup.by_size.get_mut(&size) {
					members.remove(&file);
				}
			}
		}

		self.dedup.dirty.insert(size);
	}

	/// The first member is the canonical copy; the others become its placeholders. A member that was itself a
	/// canonical copy waiting for a take-over no longer is.
	fn apply_set(&mut self, set: &[FileId]) {
		let canonical = set[0];
		self.dedup.canonical_of.remove(&canonical);
		for &duplicate in &set[1..] {
			self.dedup.canonical_of.insert(duplicate, canonical);
			if !self.pipeline.jobs.contains_key(&duplicate) {
				self.dedup.moves.remove(&duplicate);
			}
		}

		let unstarted = matches!(
			self.tree.file(canonical).record.state,
			FileState::Ready | FileState::Queued
		) && !self.pipeline.jobs.contains_key(&canonical);
		if unstarted {
			let holder = set[1..].iter().copied().find(|&member| {
				self.holds_content(member)
					&& !self.pipeline.jobs.contains_key(&member)
					&& !self
						.dedup
						.moves
						.iter()
						.any(|(taker, held)| *held == member && *taker != canonical)
			});
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
			self.route_duplicate(duplicate, canonical, false);
		}
	}

	/// `moved`: the duplicate's archived output was just taken over by the canonical copy.
	fn route_duplicate(&mut self, duplicate: FileId, canonical: FileId, moved: bool) {
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
				let holds = !matches!(output.how, How::Placeholder { .. });
				if holds && !moved && self.canonical_output(canonical).is_none() {
					return;
				}

				let adopted = holds && !moved && self.transfer_dependents(duplicate, output);
				let mut record = self.tree.file(duplicate).record.clone();
				record.replaces = (!adopted).then(|| output.path.clone());
				self.tree.set_record(duplicate, record);
			}
			FileState::Ready | FileState::Queued => {}
			_ => return,
		}

		self.set_state(duplicate, FileState::Queued);
		self.unqueue(duplicate);

		if self.canonical_output(canonical).is_some() {
			self.enqueue(duplicate);
		} else {
			self.pipeline.waits.wait(duplicate, [canonical]);
		}
	}

	/// Done with an output that holds the content itself, encoded or copied, that no placeholder points at: such an
	/// output must stay where it is.
	fn holds_content(&self, file: FileId) -> bool {
		let content = match &self.tree.file(file).record.state {
			FileState::Done(output) => !matches!(output.how, How::Placeholder { .. }),
			_ => false,
		};

		content && !self.has_dependents(file)
	}

	/// Whether the member whose output a canonical copy's move takes over still holds its content at `from`. When it
	/// no longer does, the move is dropped and the group decides again.
	pub(super) fn take_over_holds(&mut self, file: FileId, from: &Path) -> bool {
		let holds = self.dedup.moves.get(&file).is_some_and(|&holder| {
			self.holds_content(holder)
				&& self
					.tree
					.file(holder)
					.record
					.state
					.output()
					.is_some_and(|output| self.destination_path(&output.path) == from)
		});

		if !holds {
			self.dedup.moves.remove(&file);
			self.dedup.dirty.insert(self.tree.file(file).record.size);
		}
		holds
	}

	/// The archived content a placeholder may point at, also after the canonical copy's source is gone or deleted.
	pub(super) fn canonical_output(&self, canonical: FileId) -> Option<&Output> {
		match &self.tree.file(canonical).record.state {
			FileState::Done(output)
			| FileState::SourceGone(output)
			| FileState::DeletePending(output)
			| FileState::OriginalDeleted(output) => (!matches!(output.how, How::Placeholder { .. })).then_some(output),
			_ => None,
		}
	}

	/// Whether the canonical copy still holds its content at `target`, the output a placeholder's text links to, and
	/// no take-over is moving it away.
	pub(super) fn still_points_at(&self, canonical: &RelPath, target: &RelPath) -> bool {
		self.tree
			.file_id(canonical)
			.filter(|canonical| !self.dedup.moves.values().any(|holder| holder == canonical))
			.and_then(|canonical| self.canonical_output(canonical))
			.is_some_and(|output| output.path == *target)
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
		let canonical_key = self.tree.file(canonical).key.clone();
		let same_file = entry
			.record
			.full_hash
			.is_none()
			.then(|| self.source_path(&canonical_key));
		Some(Task::Write {
			content: text.into_bytes(),
			made: Made::Placeholder {
				canonical: canonical_key,
				target: output.path,
			},
			same_file,
		})
	}

	/// The duplicate and its canonical copy were matched as the same file, and no longer are: the duplicate goes
	/// through dedup again, by its hashes.
	pub(super) fn not_the_same_file(&mut self, file: FileId) {
		let mut record = self.tree.file(file).record.clone();
		record.identity = None;
		self.tree.set_record(file, record);
		self.leave_dedup(file);

		self.set_state(file, FileState::Ready);
		self.gate(file);
	}

	/// Whether the file's archived output went, or is going, to a shallower copy by a take-over: a canonical copy's
	/// move is planned or running, or a crash cut the swap short after the move, and a copy ahead of it in canonical
	/// order holds that content now. The group then decides again, and the file becomes its placeholder.
	pub(super) fn output_taken_over(&mut self, file: FileId, output: &Output) -> bool {
		if self.dedup.moves.values().any(|holder| *holder == file) {
			return true;
		}

		let Some(hash) = output.hash else {
			return false;
		};
		let entry = self.tree.file(file);
		let size = entry.record.size;
		let taken = self.dedup.by_size.get(&size).is_some_and(|members| {
			members.contains(&file)
				&& members.iter().any(|&member| {
					member != file
						&& dedup::canonical_order(&self.tree.file(member).key, &entry.key).is_lt()
						&& self.canonical_output(member).is_some_and(|held| {
							held.hash == Some(hash) && held.size == output.size && held.path != output.path
						}) && self.sources_match(file, member)
				})
		});

		if taken {
			self.dedup.dirty.insert(size);
		}
		taken
	}

	/// Whether two files' own sources are provably the same content, not merely their archived outputs: their full
	/// hash when both are known, else their window hash. Neither known is not proof, so it is never a match: a
	/// same-hash, same-size output at another path could still belong to a different source.
	fn sources_match(&self, file: FileId, other: FileId) -> bool {
		let a = &self.tree.file(file).record;
		let b = &self.tree.file(other).record;
		match (a.full_hash, b.full_hash) {
			(Some(left), Some(right)) => left == right,
			(None, None) => false,
			_ => a.window_hash.is_some() && a.window_hash == b.window_hash && a.size == b.size,
		}
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

	/// Remembers a file whose recorded output is a placeholder, under the canonical copy it points at.
	pub(super) fn note_placeholder(&mut self, file: FileId) {
		if let Some(canonical) = placeholder_of(&self.tree.file(file).record.state) {
			self.dedup
				.placeholders
				.entry(canonical.clone())
				.or_default()
				.insert(file);
		}
	}

	fn points_at(&self, file: FileId, canonical: &RelPath) -> bool {
		let entry = self.tree.file(file);
		!entry.removed && placeholder_of(&entry.record.state) == Some(canonical)
	}

	/// The files whose recorded output is a placeholder pointing at `canonical`, shallowest first.
	fn placeholders_of(&self, canonical: FileId) -> Vec<FileId> {
		let key = &self.tree.file(canonical).key;
		let mut found: Vec<FileId> = self
			.dedup
			.placeholders
			.get(key)
			.into_iter()
			.flatten()
			.copied()
			.filter(|&file| file != canonical && self.points_at(file, key))
			.collect();
		found.sort_by(|a, b| dedup::canonical_order(&self.tree.file(*a).key, &self.tree.file(*b).key));
		found
	}

	/// Whether a record depends on the file's content output: some record's output, in any state, is a placeholder
	/// pointing at it.
	pub(super) fn has_dependents(&self, file: FileId) -> bool {
		let key = &self.tree.file(file).key;
		self.dedup.placeholders.get(key).is_some_and(|files| {
			files
				.iter()
				.any(|&dependent| dependent != file && self.points_at(dependent, key))
		})
	}

	/// An output file that any record depends on is never replaced or removed. `canonical` is about to give up
	/// `earlier`, its content output, so its dependents take it over, in the same batch as that change: the first in
	/// canonical order adopts it as its own record (nothing moves on disk), and the others point at that heir. Returns
	/// whether there was an heir; the canonical copy's next output then takes a name of its own.
	pub(super) fn transfer_dependents(&mut self, canonical: FileId, earlier: &Output) -> bool {
		if matches!(earlier.how, How::Placeholder { .. }) {
			return false;
		}

		let dependents = self.placeholders_of(canonical);
		let Some((&heir, others)) = dependents.split_first() else {
			return false;
		};

		let key = self.tree.file(canonical).key.clone();
		self.dedup.placeholders.remove(&key);
		for &other in others {
			self.point_at(other, heir);
		}
		self.adopt(heir, canonical, earlier);

		let name = self.tree.file(heir).key.display();
		self.log(
			LogKind::Action,
			format!("{name}: keeps the archived copy {}", earlier.path.display()),
		);
		self.crash_point(super::Fault::AfterAdoption);
		true
	}

	/// `heir` takes `canonical`'s earlier output over as its own record: a gone original keeps its gone state, anything
	/// else is Done with it. Its placed placeholder, and the earlier output its own commit would replace, go once that
	/// is durable; a placeholder still being committed is left to `placed`.
	fn adopt(&mut self, heir: FileId, canonical: FileId, output: &Output) {
		let entry = self.tree.file(heir);
		let placed = match &entry.record.state {
			FileState::Done(placeholder)
			| FileState::SourceGone(placeholder)
			| FileState::DeletePending(placeholder)
			| FileState::OriginalDeleted(placeholder) => Some(placeholder.path.clone()),
			_ => None,
		};
		let mut own: Vec<RelPath> = placed
			.into_iter()
			.chain(entry.record.replaces.clone())
			.filter(|path| *path != output.path)
			.collect();
		own.dedup();
		let state = match &entry.record.state {
			FileState::SourceGone(_) => FileState::SourceGone(output.clone()),
			FileState::DeletePending(_) => FileState::DeletePending(output.clone()),
			FileState::OriginalDeleted(_) => FileState::OriginalDeleted(output.clone()),
			_ => FileState::Done(output.clone()),
		};

		let mut record = entry.record.clone();
		record.replaces = None;
		self.tree.set_record(heir, record);
		self.dedup.canonical_of.remove(&heir);
		self.set_state(heir, state);
		self.release_claim(canonical, &output.path);
		self.claim(heir, &output.path);

		for path in own {
			self.remove_once_durable(heir, path);
		}
	}

	/// A placeholder now points at `canonical`; its text still links to the same archived file.
	fn point_at(&mut self, file: FileId, canonical: FileId) {
		let how = How::Placeholder {
			canonical: self.tree.file(canonical).key.clone(),
		};
		let mut state = self.tree.file(file).record.state.clone();
		if let Some(output) = recorded_output_mut(&mut state) {
			output.how = how.clone();
		}
		self.set_state(file, state);

		if let Some(pending) = self
			.pipeline
			.jobs
			.get_mut(&file)
			.and_then(RunningJob::pending_output_mut)
		{
			pending.how = how;
		}
	}

	/// A file with dependents doesn't replace its earlier output: its new output takes a name of its own.
	pub(super) fn keep_depended_output(&mut self, file: FileId) {
		if self.tree.file(file).record.replaces.is_none() || !self.has_dependents(file) {
			return;
		}

		let mut record = self.tree.file(file).record.clone();
		record.replaces = None;
		self.tree.set_record(file, record);
	}

	/// An heir's own file, removed once the batch that records its adoption is durable.
	pub(super) fn remove_once_durable(&mut self, heir: FileId, path: RelPath) {
		self.dedup.remove_after_durable.push((heir, path));
	}

	/// Once everything batched is durable, removes the heirs' own files, each only while no other file holds its name
	/// and no record of the heir names it.
	pub(super) fn after_flush(&mut self) {
		if self.store.pending_changes() > 0 || self.dedup.remove_after_durable.is_empty() {
			return;
		}

		for (heir, path) in std::mem::take(&mut self.dedup.remove_after_durable) {
			if self.recorded_outputs(heir).contains(&path) || self.claimed_by_other(heir, &path) {
				continue;
			}

			self.remove_own_output(&path);
			self.release_claim(heir, &path);
		}
	}

	/// Files whose removal waits for a flush; still the project's own.
	pub(super) fn placeholders_to_remove(&self) -> impl Iterator<Item = &RelPath> {
		self.dedup.remove_after_durable.iter().map(|(_, path)| path)
	}
}

/// The output a record names: its state's, or the one an output issue is about.
pub(super) fn recorded_output(state: &FileState) -> Option<&Output> {
	match state {
		FileState::Issue { output, .. } => output.as_ref(),
		state => state.output(),
	}
}

fn recorded_output_mut(state: &mut FileState) -> Option<&mut Output> {
	match state {
		FileState::Issue { output, .. } => output.as_mut(),
		FileState::Committing { output, .. }
		| FileState::Done(output)
		| FileState::SourceGone(output)
		| FileState::DeletePending(output)
		| FileState::OriginalDeleted(output) => Some(output),
		FileState::Discovered
		| FileState::Ready
		| FileState::Queued
		| FileState::InProgress { .. }
		| FileState::Excluded => None,
	}
}

/// The canonical copy a recorded placeholder points at, also when an output issue is about the placeholder.
fn placeholder_of(state: &FileState) -> Option<&RelPath> {
	match recorded_output(state).map(|output| &output.how) {
		Some(How::Placeholder { canonical }) => Some(canonical),
		_ => None,
	}
}

fn is_gone(state: &FileState) -> bool {
	matches!(
		state,
		FileState::SourceGone(_) | FileState::DeletePending(_) | FileState::OriginalDeleted(_)
	)
}

/// Hashes the file only while it has the size and time the project recorded, checked before and after.
fn hash_unchanged(path: &Path, request: HashRequest) -> HashOutcome {
	let unchanged = || run::source_unchanged(path, request.size, request.mtime_ns);
	if !unchanged() {
		return changed(path);
	}

	let hashed = match request.kind {
		HashKind::Window => {
			let identity = file_id(path).ok().map(Identity::from);
			dedup::window_hash(path).map(|hash| (identity, hash))
		}
		HashKind::Full => dedup::full_hash(path).map(|hash| (None, hash)),
	};
	if !unchanged() {
		return changed(path);
	}

	match hashed {
		Ok((identity, hash)) => HashOutcome::Hashed { identity, hash },
		Err(err) => HashOutcome::Unreadable(err.to_string()),
	}
}

fn changed(path: &Path) -> HashOutcome {
	match fs::metadata(path) {
		Ok(meta) if meta.is_file() => match meta.modified() {
			Ok(modified) => HashOutcome::Changed {
				size: meta.len(),
				mtime_ns: mtime_ns(modified),
			},
			Err(err) => HashOutcome::Unreadable(err.to_string()),
		},
		Ok(_) => HashOutcome::Unreadable("no longer a file".into()),
		Err(err) if err.kind() == io::ErrorKind::NotFound => HashOutcome::Gone,
		Err(err) => HashOutcome::Unreadable(err.to_string()),
	}
}

#[cfg(test)]
mod tests {
	use std::ffi::OsStr;
	use std::path::PathBuf;
	use std::sync::mpsc::Receiver;
	use std::time::Duration;

	use tokio::sync::oneshot;

	use super::*;
	use crate::project::jobs::{JobMessage, Target};
	use crate::project::pipeline::tests::{committing_job, running_job};
	use crate::project::scanning::tests::{key, list, list_queued, owner, reopen};
	use crate::record::{CopyReason, FileRecord};
	use crate::run::Staged;
	use crate::tree::FolderId;
	use crate::view::Update;

	const SIZE: u64 = 100_000;

	struct Fixture {
		dir: tempfile::TempDir,
		owner: Owner,
		_updates: Receiver<Update>,
	}

	impl Fixture {
		fn new() -> Self {
			let dir = tempfile::tempdir().unwrap();
			let source = dir.path().join("source");
			fs::create_dir_all(&source).unwrap();
			let (owner, updates) = owner(&source, &dir.path().join("archive"));
			Self {
				dir,
				owner,
				_updates: updates,
			}
		}

		/// The same project opened again, as after a crash: whatever wasn't flushed is lost.
		fn reopened(self) -> Self {
			let project = self.owner.project_file.clone();
			drop(self.owner);
			let (owner, updates) = reopen(&project);
			Self {
				dir: self.dir,
				owner,
				_updates: updates,
			}
		}

		fn add(&mut self, name: &str, record: FileRecord) -> FileId {
			self.owner.tree.add_file(FolderId::ROOT, OsStr::new(name), record)
		}

		fn gated(&mut self, name: &str, record: FileRecord) -> FileId {
			let file = self.add(name, record);
			assert!(self.owner.gate_duplicates(file));
			file
		}
	}

	fn ready(identity: Option<Identity>, window: u8) -> FileRecord {
		FileRecord {
			identity,
			window_hash: Some(Hash([window; 32])),
			state: FileState::Ready,
			..FileRecord::discovered(SIZE, 1)
		}
	}

	fn copied(name: &str) -> Output {
		Output {
			how: How::Copied(CopyReason::NotMedia),
			path: key(name),
			size: SIZE,
			hash: Some(Hash([1; 32])),
		}
	}

	#[test]
	fn an_output_a_take_over_is_moving_is_never_reported_missing() {
		let mut fixture = Fixture::new();
		let holder = fixture.add(
			"held.bin",
			FileRecord {
				state: FileState::Done(copied("held.bin")),
				..FileRecord::discovered(SIZE, 1)
			},
		);
		let canonical = fixture.add("a.bin", ready(None, 1));
		fixture.owner.dedup.moves.insert(canonical, holder);

		fixture.owner.on_destination_listed(&[]);

		assert_eq!(
			fixture.owner.tree.file(holder).record.state,
			FileState::Done(copied("held.bin"))
		);
	}

	#[test]
	fn a_real_missing_output_beside_a_different_source_twin_is_still_flagged() {
		let mut fixture = Fixture::new();
		let shared_hash = Some(Hash([7; 32]));
		let twin = fixture.add(
			"a.bin",
			FileRecord {
				full_hash: Some(Hash([1; 32])),
				state: FileState::Done(Output {
					hash: shared_hash,
					..copied("a.bin")
				}),
				..ready(None, 1)
			},
		);
		let lost_output = Output {
			hash: shared_hash,
			..copied("b.bin")
		};
		let lost = fixture.add(
			"b.bin",
			FileRecord {
				full_hash: Some(Hash([2; 32])),
				state: FileState::Done(lost_output.clone()),
				..ready(None, 2)
			},
		);
		fixture
			.owner
			.dedup
			.by_size
			.entry(SIZE)
			.or_default()
			.extend([twin, lost]);

		assert!(!fixture.owner.output_taken_over(lost, &lost_output));
	}

	#[test]
	fn a_gone_members_stored_identity_never_makes_a_new_file_its_duplicate() {
		let mut fixture = Fixture::new();
		let identity = Some(Identity { volume: 1, index: 7 });
		let gone = fixture.add(
			"a.bin",
			FileRecord {
				full_hash: Some(Hash([1; 32])),
				state: FileState::SourceGone(copied("a.bin")),
				..ready(identity, 1)
			},
		);
		fixture.owner.join_dedup(gone);
		fixture.owner.dedup.fresh_identity.insert(gone);
		let new = fixture.gated("b.bin", ready(identity, 2));
		fixture.owner.dedup.fresh_identity.insert(new);

		fixture.owner.plan_dirty_groups();

		assert!(!fixture.owner.is_duplicate(new));
		assert_eq!(fixture.owner.tree.file(new).record.state, FileState::Queued);
	}

	#[test]
	fn a_stored_identity_needs_a_full_hash_until_it_is_read_again() {
		let mut fixture = Fixture::new();
		let identity = Some(Identity { volume: 1, index: 7 });
		let a = fixture.gated("a.bin", ready(identity, 1));
		let b = fixture.gated("b.bin", ready(identity, 1));

		fixture.owner.plan_dirty_groups();

		assert!(!fixture.owner.is_duplicate(b));
		assert_eq!(fixture.owner.dedup.pending.get(&a), Some(&HashKind::Full));
		assert_eq!(fixture.owner.dedup.pending.get(&b), Some(&HashKind::Full));

		fixture.owner.dedup.pending.clear();
		fixture.owner.dedup.fresh_identity.extend([a, b]);
		fixture.owner.dedup.dirty.insert(SIZE);
		fixture.owner.plan_dirty_groups();

		assert_eq!(fixture.owner.canonical_of(b), Some(a));
	}

	fn hash_once(fixture: &mut Fixture, runtime: &tokio::runtime::Runtime, file: FileId) {
		let mut inbox = fixture.owner.inbox.take().unwrap();
		fixture.owner.request_hash(file, HashKind::Window);
		fixture.owner.start_hash(file, Instant::now());

		let message = runtime
			.block_on(tokio::time::timeout(Duration::from_secs(20), inbox.recv()))
			.unwrap()
			.unwrap();
		let Message::Hashed(file, request, outcome) = message else {
			panic!("not a hash result")
		};
		fixture.owner.on_hashed(file, request, outcome);
		fixture.owner.inbox = Some(inbox);
	}

	#[test]
	fn a_source_that_changed_under_its_hash_is_archived_again() {
		let runtime = tokio::runtime::Runtime::new().unwrap();
		let _entered = runtime.enter();
		let mut fixture = Fixture::new();
		let source = fixture.dir.path().join("source");
		fs::write(source.join("a.bin"), vec![1; 100_000]).unwrap();
		fs::write(source.join("b.bin"), vec![2; 100_000]).unwrap();
		list(&mut fixture.owner, &RelPath::root(), true);
		let a = fixture.owner.tree.file_id(&key("a.bin")).unwrap();
		let b = fixture.owner.tree.file_id(&key("b.bin")).unwrap();
		fixture.owner.set_state(a, FileState::Done(copied("a.bin")));
		fixture.owner.join_dedup(a);
		fixture.owner.set_state(b, FileState::Ready);

		fs::write(source.join("a.bin"), vec![3; 100_001]).unwrap();
		hash_once(&mut fixture, &runtime, a);

		let record = &fixture.owner.tree.file(a).record;
		assert_eq!(record.state, FileState::Discovered);
		assert_eq!((record.size, record.window_hash), (100_001, None));
		assert_eq!(record.replaces, Some(key("a.bin")));

		fs::remove_file(source.join("b.bin")).unwrap();
		hash_once(&mut fixture, &runtime, b);

		assert!(fixture.owner.tree.file(b).removed);
		assert!(fixture.owner.issue_files.is_empty());
	}

	#[test]
	fn a_stale_hash_result_is_dropped() {
		let mut fixture = Fixture::new();
		let file = fixture.gated(
			"a.bin",
			FileRecord {
				window_hash: None,
				..ready(None, 0)
			},
		);
		fixture.owner.request_hash(file, HashKind::Window);
		let hashed = || HashOutcome::Hashed {
			identity: Some(Identity { volume: 1, index: 2 }),
			hash: Hash([9; 32]),
		};

		let older = HashRequest {
			kind: HashKind::Window,
			size: SIZE,
			mtime_ns: 0,
		};
		fixture.owner.on_hashed(file, older, hashed());
		let other_kind = HashRequest {
			kind: HashKind::Full,
			size: SIZE,
			mtime_ns: 1,
		};
		fixture.owner.on_hashed(file, other_kind, hashed());

		let record = &fixture.owner.tree.file(file).record;
		assert_eq!(
			(record.window_hash, record.full_hash, record.identity),
			(None, None, None)
		);
		assert_eq!(fixture.owner.dedup.pending.get(&file), Some(&HashKind::Window));
	}

	#[test]
	fn a_large_size_group_is_planned_quickly() {
		let mut fixture = Fixture::new();
		let started = Instant::now();

		let files: Vec<FileId> = (0..5000u32)
			.map(|index| {
				let mut window = [0; 32];
				window[..4].copy_from_slice(&index.to_le_bytes());
				fixture.gated(
					&format!("f{index}.bin"),
					FileRecord {
						window_hash: Some(Hash(window)),
						..ready(None, 0)
					},
				)
			})
			.collect();
		fixture.owner.plan_dirty_groups();

		assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
		assert!(
			files
				.iter()
				.all(|file| fixture.owner.tree.file(*file).record.state == FileState::Queued)
		);
	}

	#[test]
	fn a_canonical_copy_that_becomes_a_duplicate_no_longer_takes_an_output_over() {
		let mut fixture = Fixture::new();
		let hashed = |state| FileRecord {
			full_hash: Some(Hash([4; 32])),
			state,
			..ready(None, 4)
		};
		let shallowest = fixture.gated("a.bin", hashed(FileState::Ready));
		let former = fixture.gated("b.bin", hashed(FileState::Queued));
		let holder = fixture.gated("c.bin", hashed(FileState::Done(copied("c.bin"))));
		fixture.owner.dedup.moves.insert(former, holder);

		fixture.owner.plan_dirty_groups();

		assert_eq!(fixture.owner.dedup.moves.get(&shallowest), Some(&holder));
		assert!(!fixture.owner.dedup.moves.contains_key(&former));
		assert_eq!(fixture.owner.canonical_of(former), Some(shallowest));
	}

	fn take_over(fixture: &mut Fixture, canonical: FileId) -> Option<Target> {
		let (reply, answer) = oneshot::channel();
		let staged = Staged {
			path: fixture.owner.destination_path(&key("held.bin")),
			size: SIZE,
			hash: Hash([1; 32]),
		};
		let made = Made::Moved {
			how: How::Copied(CopyReason::NotMedia),
			hash: Some(Hash([1; 32])),
			extension: "bin".into(),
		};

		fixture
			.owner
			.on_job(canonical, JobMessage::Commit { staged, made, reply });
		answer.blocking_recv().unwrap()
	}

	#[test]
	fn a_take_over_is_refused_once_its_holder_no_longer_holds_the_content() {
		let mut fixture = Fixture::new();
		let holder = fixture.add(
			"held.bin",
			FileRecord {
				state: FileState::Done(copied("held.bin")),
				..FileRecord::discovered(SIZE, 1)
			},
		);
		let canonical = fixture.add("a.bin", ready(None, 1));
		fixture.owner.dedup.moves.insert(canonical, holder);
		fixture
			.owner
			.pipeline
			.jobs
			.insert(canonical, running_job(Work::Placeholder));
		fixture.owner.set_state(canonical, FileState::InProgress { attempt: 1 });

		assert!(take_over(&mut fixture, canonical).is_some());

		let gone = fixture.add(
			"gone.bin",
			FileRecord {
				state: FileState::SourceGone(Output {
					how: How::Placeholder {
						canonical: key("held.bin"),
					},
					path: key("placeholder_gone.bin.txt"),
					size: 10,
					hash: Some(Hash([2; 32])),
				}),
				..FileRecord::discovered(SIZE, 1)
			},
		);
		fixture.owner.note_placeholder(gone);

		assert!(take_over(&mut fixture, canonical).is_none());
		assert!(!fixture.owner.dedup.moves.contains_key(&canonical));
	}

	fn placeholder(name: &str, canonical: &str) -> Output {
		Output {
			how: How::Placeholder {
				canonical: key(canonical),
			},
			path: key(&format!("placeholder_{name}.txt")),
			size: 10,
			hash: Some(Hash([2; 32])),
		}
	}

	#[test]
	fn every_placeholder_of_a_changed_canonical_copy_keeps_its_content_whatever_its_state() {
		let mut fixture = Fixture::new();
		let canonical = fixture.add(
			"a.bin",
			FileRecord {
				state: FileState::Done(copied("a.bin")),
				..ready(None, 1)
			},
		);
		fixture.owner.claim(canonical, &key("a.bin"));
		let missing = FileState::Issue {
			kind: IssueKind::OutputMissing,
			attempt: 0,
			reason: "the archived file is missing".into(),
			output: Some(placeholder("e.bin", "a.bin")),
		};
		let dependents = [
			("b.bin", FileState::OriginalDeleted(placeholder("b.bin", "a.bin"))),
			("c.bin", FileState::DeletePending(placeholder("c.bin", "a.bin"))),
			(
				"d.bin",
				FileState::Committing {
					attempt: 1,
					output: placeholder("d.bin", "a.bin"),
				},
			),
			("e.bin", missing),
		]
		.map(|(name, state)| {
			let file = fixture.add(
				name,
				FileRecord {
					state,
					..FileRecord::discovered(SIZE, 1)
				},
			);
			fixture.owner.note_placeholder(file);
			file
		});
		let [heir, pending, committing, missing] = dependents;
		fixture
			.owner
			.pipeline
			.jobs
			.insert(committing, committing_job(placeholder("d.bin", "a.bin")));

		fixture.owner.source_changed(canonical, SIZE, 2);

		let state = |file: FileId| fixture.owner.tree.file(file).record.state.clone();
		assert_eq!(state(heir), FileState::OriginalDeleted(copied("a.bin")));
		assert_eq!(state(pending), FileState::DeletePending(placeholder("c.bin", "b.bin")));
		assert_eq!(
			state(committing),
			FileState::Committing {
				attempt: 1,
				output: placeholder("d.bin", "b.bin"),
			}
		);
		assert_eq!(
			fixture.owner.pipeline.jobs[&committing].pending_output(),
			Some(&placeholder("d.bin", "b.bin"))
		);
		assert!(matches!(
			state(missing),
			FileState::Issue { output: Some(output), .. } if output == placeholder("e.bin", "b.bin")
		));
		assert_eq!(fixture.owner.tree.file(canonical).record.replaces, None);
		assert!(fixture.owner.claimed_by_other(canonical, &key("a.bin")));
		assert!(!fixture.owner.claimed_by_other(heir, &key("a.bin")));
	}

	#[test]
	fn a_canonical_copy_that_becomes_a_placeholder_leaves_its_output_to_its_own_placeholders() {
		let mut fixture = Fixture::new();
		let done = |output| FileRecord {
			full_hash: Some(Hash([4; 32])),
			state: FileState::Done(output),
			..ready(None, 4)
		};
		let shallowest = fixture.add("a.bin", done(copied("a.bin")));
		let former = fixture.add("b.bin", done(copied("b.bin")));
		let dependent = fixture.add("c.bin", done(placeholder("c.bin", "b.bin")));
		fixture.owner.note_placeholder(dependent);
		fixture.owner.dedup.canonical_of.insert(former, shallowest);

		fixture.owner.route_duplicate(former, shallowest, false);

		assert_eq!(
			fixture.owner.tree.file(dependent).record.state,
			FileState::Done(copied("b.bin"))
		);
		assert_eq!(fixture.owner.tree.file(former).record.replaces, None);
		assert!(fixture.owner.pipeline.queues[&Work::Placeholder].contains(former));
	}

	fn commit(fixture: &mut Fixture, file: FileId, made: Made, content: &[u8]) -> Option<Target> {
		let (reply, answer) = oneshot::channel();
		let staged = Staged {
			path: PathBuf::from("staged"),
			size: content.len() as u64,
			hash: Hash(*blake3::hash(content).as_bytes()),
		};

		fixture.owner.on_job(file, JobMessage::Commit { staged, made, reply });
		answer.blocking_recv().unwrap()
	}

	fn running(fixture: &mut Fixture, file: FileId, work: Work) {
		fixture.owner.pipeline.jobs.insert(file, running_job(work));
		fixture.owner.set_state(file, FileState::InProgress { attempt: 1 });
	}

	fn scan(fixture: &mut Fixture) {
		list(&mut fixture.owner, &RelPath::root(), true);
		list_queued(&mut fixture.owner);
	}

	#[test]
	fn an_output_a_record_depends_on_is_never_replaced_or_removed() {
		let mut fixture = Fixture::new();
		let canonical = fixture.add(
			"a.bin",
			FileRecord {
				replaces: Some(key("a.bin")),
				..ready(None, 1)
			},
		);
		let dependent = fixture.add(
			"b.bin",
			FileRecord {
				state: FileState::Done(placeholder("b.bin", "a.bin")),
				..ready(None, 1)
			},
		);
		fixture.owner.note_placeholder(dependent);
		let archived = fixture.owner.destination_path(&key("a.bin"));
		fs::create_dir_all(archived.parent().unwrap()).unwrap();
		fs::write(&archived, b"old").unwrap();

		running(&mut fixture, canonical, Work::Copy);
		let target = commit(&mut fixture, canonical, Made::Copied(CopyReason::NotMedia), b"new").unwrap();

		assert_eq!(
			(target.path.clone(), target.replaces),
			(fixture.owner.destination_path(&key("a (2).bin")), None)
		);

		let mut record = fixture.owner.tree.file(canonical).record.clone();
		record.replaces = Some(key("a.bin"));
		fixture.owner.tree.set_record(canonical, record);
		fs::write(&target.path, b"new").unwrap();
		fixture.owner.on_job(canonical, JobMessage::Placed);

		assert_eq!(fs::read(&archived).unwrap(), b"old");
	}

	#[test]
	fn an_output_a_placeholder_points_at_is_never_taken_over() {
		let mut fixture = Fixture::new();
		let hashed = |state| FileRecord {
			full_hash: Some(Hash([4; 32])),
			state,
			..ready(None, 4)
		};
		let shallowest = fixture.gated("a.bin", hashed(FileState::Ready));
		let holder = fixture.gated("b.bin", hashed(FileState::Done(copied("b.bin"))));
		let dependent = fixture.add(
			"c.bin",
			FileRecord {
				state: FileState::Done(placeholder("c.bin", "b.bin")),
				..FileRecord::discovered(SIZE, 1)
			},
		);
		fixture.owner.note_placeholder(dependent);

		fixture.owner.plan_dirty_groups();

		assert!(!fixture.owner.dedup.moves.contains_key(&shallowest));
		assert_eq!(fixture.owner.canonical_of(holder), Some(shallowest));
	}

	#[test]
	fn a_placeholder_is_never_committed_against_an_output_a_take_over_is_moving() {
		let mut fixture = Fixture::new();
		let holder = fixture.add(
			"held.bin",
			FileRecord {
				state: FileState::Done(copied("held.bin")),
				..ready(None, 1)
			},
		);
		let taker = fixture.add("a.bin", ready(None, 1));
		let duplicate = fixture.add("b.bin", ready(None, 1));
		fixture.owner.dedup.moves.insert(taker, holder);

		running(&mut fixture, duplicate, Work::Placeholder);
		let made = Made::Placeholder {
			canonical: key("held.bin"),
			target: key("held.bin"),
		};

		assert!(commit(&mut fixture, duplicate, made, b"placeholder").is_none());
	}

	static OLD: [u8; 100_000] = [1; 100_000];
	static NEW: [u8; 100_001] = [2; 100_001];
	const TEXT: &[u8] = b"This file is a duplicate.";

	/// `a.bin` is archived, and the intent of its duplicate `Backup/a.bin`'s placeholder is durable when `a.bin`
	/// changes. Returns the archived output and the placeholder's target.
	fn placeholder_committed_as_its_canonical_copy_changes(fixture: &mut Fixture) -> (Output, Target) {
		let source = fixture.dir.path().join("source");
		fs::create_dir_all(source.join("Backup")).unwrap();
		fs::write(source.join("a.bin"), OLD).unwrap();
		fs::write(source.join("Backup").join("a.bin"), OLD).unwrap();
		scan(fixture);
		let canonical = fixture.owner.tree.file_id(&key("a.bin")).unwrap();
		let duplicate = fixture.owner.tree.file_id(&key("Backup/a.bin")).unwrap();
		let archived = fixture.owner.destination_path(&key("a.bin"));
		fs::create_dir_all(archived.parent().unwrap()).unwrap();
		fs::write(&archived, OLD).unwrap();
		let output = Output {
			hash: Some(Hash(*blake3::hash(&OLD).as_bytes())),
			..copied("a.bin")
		};
		fixture.owner.set_state(canonical, FileState::Done(output.clone()));
		fixture.owner.claim(canonical, &key("a.bin"));

		running(fixture, duplicate, Work::Placeholder);
		let made = Made::Placeholder {
			canonical: key("a.bin"),
			target: key("a.bin"),
		};
		let target = commit(fixture, duplicate, made, TEXT).unwrap();
		fs::write(source.join("a.bin"), NEW).unwrap();
		list(&mut fixture.owner, &RelPath::root(), false);
		fs::create_dir_all(target.path.parent().unwrap()).unwrap();
		fs::write(&target.path, TEXT).unwrap();
		(output, target)
	}

	#[test]
	fn a_placeholder_placed_after_it_adopted_its_canonical_copys_output_goes_once_that_is_durable() {
		let mut fixture = Fixture::new();
		let (output, target) = placeholder_committed_as_its_canonical_copy_changes(&mut fixture);
		let duplicate = fixture.owner.tree.file_id(&key("Backup/a.bin")).unwrap();

		fixture.owner.on_job(duplicate, JobMessage::Placed);

		assert_eq!(fixture.owner.tree.file(duplicate).record.state, FileState::Done(output));
		assert!(target.path.exists());

		fixture.owner.store.flush().unwrap();
		fixture.owner.after_flush();

		assert!(!target.path.exists());
	}

	/// The project crashes before the placeholder's rename is recorded; `flushed`: the change was durable by then.
	fn a_placeholder_committed_as_its_canonical_copy_changes_then_a_crash(flushed: bool) {
		let mut fixture = Fixture::new();
		let (output, _) = placeholder_committed_as_its_canonical_copy_changes(&mut fixture);
		let duplicate = fixture.owner.tree.file_id(&key("Backup/a.bin")).unwrap();
		if flushed {
			fixture.owner.store.flush().unwrap();
			fixture.owner.after_flush();
		}
		fixture.owner.fault = Some(crate::project::Fault::AfterPlaced);
		fixture.owner.on_job(duplicate, JobMessage::Placed);

		let mut fixture = fixture.reopened();
		scan(&mut fixture);
		let canonical = fixture.owner.tree.file_id(&key("a.bin")).unwrap();
		let duplicate = fixture.owner.tree.file_id(&key("Backup/a.bin")).unwrap();

		assert_eq!(fixture.owner.tree.file(duplicate).record.state, FileState::Done(output));

		running(&mut fixture, canonical, Work::Copy);
		let again = commit(&mut fixture, canonical, Made::Copied(CopyReason::NotMedia), &NEW).unwrap();
		assert_eq!(
			(again.path.clone(), again.replaces),
			(fixture.owner.destination_path(&key("a (2).bin")), None)
		);
		fs::write(&again.path, NEW).unwrap();
		fixture.owner.on_job(canonical, JobMessage::Placed);

		let archived = fixture.owner.destination_path(&key("a.bin"));
		assert_eq!(fs::read(archived).unwrap(), OLD);
		assert_eq!(
			fixture.owner.tree.file(canonical).record.state.output().unwrap().path,
			key("a (2).bin")
		);
	}

	#[test]
	fn a_placeholder_committed_as_its_canonical_copy_changes_keeps_the_content_through_a_crash() {
		a_placeholder_committed_as_its_canonical_copy_changes_then_a_crash(false);
	}

	#[test]
	fn a_placeholder_committed_as_its_canonical_copy_changes_keeps_the_content_once_that_is_durable() {
		a_placeholder_committed_as_its_canonical_copy_changes_then_a_crash(true);
	}
}
