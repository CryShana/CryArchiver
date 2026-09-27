//! Dedup in the run (project layer spec §7): hashing gates a ready file in a shared size group, the canonical copy is
//! the shallowest, duplicates wait for it and become placeholders, and a later shallower copy takes the archived
//! output over by rename. That swap is two ordinary commits: the canonical copy's move, then the placeholder.
//!
//! A canonical copy's output is never replaced while a placeholder's content lives only there: a placeholder whose
//! original is still there is archived again first, and one whose original is gone adopts the output.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;
use std::time::Instant;

use cryarchiver_engine::commit::file_id;

use super::jobs::{Made, Task};
use super::owner::{Message, Owner};
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
	/// A placeholder archived again, to the canonical copy whose earlier output still holds its content: that copy
	/// doesn't replace the output until the placeholder is archived or adopts it.
	released: HashMap<FileId, (FileId, Output)>,
	/// Canonical copies held back by `released`.
	held: HashSet<FileId>,
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
		self.dedup.held.remove(&file);
		self.settle_release(file);

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

				let adopted = holds && self.release_dependents(duplicate, (!moved).then(|| output.clone()));
				let mut record = self.tree.file(duplicate).record.clone();
				record.replaces = (!adopted).then(|| output.path.clone());
				self.tree.set_record(duplicate, record);
			}
			FileState::Ready | FileState::Queued => {}
			_ => return,
		}

		self.settle_release(duplicate);
		self.set_state(duplicate, FileState::Queued);
		self.unqueue(duplicate);

		if self.canonical_output(canonical).is_some() {
			self.enqueue(duplicate);
		} else {
			self.pipeline.waits.wait(duplicate, [canonical]);
		}
	}

	/// Done with an output that holds the content itself, encoded or copied, that no placeholder of a gone original
	/// points at: such an output must stay where it is.
	fn holds_content(&self, file: FileId) -> bool {
		let content = match &self.tree.file(file).record.state {
			FileState::Done(output) => !matches!(output.how, How::Placeholder { .. }),
			_ => false,
		};

		content
			&& !self
				.placeholders_of(file)
				.iter()
				.any(|&dependent| is_gone(&self.tree.file(dependent).record.state))
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

	/// Whether the canonical copy still holds its content at `target`, the output a placeholder's text links to.
	pub(super) fn still_points_at(&self, canonical: &RelPath, target: &RelPath) -> bool {
		self.tree
			.file_id(canonical)
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
						})
				})
		});

		if taken {
			self.dedup.dirty.insert(size);
		}
		taken
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
		if let Some(How::Placeholder { canonical }) =
			self.tree.file(file).record.state.output().map(|output| &output.how)
		{
			self.dedup
				.placeholders
				.entry(canonical.clone())
				.or_default()
				.insert(file);
		}
	}

	/// The files whose recorded output is a placeholder pointing at `canonical`, shallowest first.
	fn placeholders_of(&self, canonical: FileId) -> Vec<FileId> {
		let key = &self.tree.file(canonical).key;
		let points_here = |file: FileId| {
			let entry = self.tree.file(file);
			let pointed = match entry.record.state.output().map(|output| &output.how) {
				Some(How::Placeholder { canonical }) => Some(canonical),
				_ => None,
			};
			!entry.removed && pointed == Some(key)
		};

		let mut found: Vec<FileId> = self
			.dedup
			.placeholders
			.get(key)
			.into_iter()
			.flatten()
			.copied()
			.filter(|&file| file != canonical && points_here(file))
			.collect();
		found.sort_by(|a, b| dedup::canonical_order(&self.tree.file(*a).key, &self.tree.file(*b).key));
		found
	}

	/// `canonical`'s output, `earlier`, is about to be replaced or removed; `None` when it was moved away. The
	/// placeholders pointing at it whose originals are still there are archived again, and while one of them still
	/// needs `earlier`, the canonical copy doesn't replace it. When an original is gone, the shallowest such
	/// placeholder adopts `earlier` instead, and the others point at it. Returns whether `earlier` was adopted: the
	/// canonical copy then leaves it in place.
	pub(super) fn release_dependents(&mut self, canonical: FileId, earlier: Option<Output>) -> bool {
		let dependents = self.placeholders_of(canonical);
		if dependents.is_empty() {
			return false;
		}

		let key = self.tree.file(canonical).key.clone();
		self.dedup.placeholders.remove(&key);
		let earlier = earlier.filter(|output| !matches!(output.how, How::Placeholder { .. }));
		let (gone, present): (Vec<FileId>, Vec<FileId>) = dependents
			.into_iter()
			.partition(|&file| is_gone(&self.tree.file(file).record.state));

		let adopted = if let (Some((&heir, others)), Some(output)) = (gone.split_first(), &earlier) {
			self.adopt(heir, canonical, output);
			for &other in others {
				self.point_at(other, heir);
			}
			true
		} else {
			for file in gone {
				self.note_placeholder(file);
			}
			false
		};

		let replacing = !adopted && !is_gone(&self.tree.file(canonical).record.state);
		for file in present {
			if replacing && let Some(output) = &earlier {
				self.dedup.released.insert(file, (canonical, output.clone()));
			}

			if matches!(self.tree.file(file).record.state, FileState::Done(_)) {
				self.archive_again(file);
			} else {
				self.note_placeholder(file);
			}
		}

		adopted
	}

	/// A placeholder whose canonical copy lets its output go is archived again from the hashes it has, replacing the
	/// placeholder.
	fn archive_again(&mut self, file: FileId) {
		let mut record = self.tree.file(file).record.clone();
		record.replaces = record.state.output().map(|output| output.path.clone());
		self.tree.set_record(file, record);
		self.dedup.canonical_of.remove(&file);

		self.set_state(file, FileState::Ready);
		self.gate(file);
	}

	/// `heir`, whose original is gone, takes `canonical`'s earlier output over as its own record: nothing moves on
	/// disk. Its own placeholder file goes once that is durable.
	fn adopt(&mut self, heir: FileId, canonical: FileId, output: &Output) {
		let entry = self.tree.file(heir);
		let placeholder = match &entry.record.state {
			state if is_gone(state) => state.output().map(|output| output.path.clone()),
			_ => entry.record.replaces.clone(),
		};
		let state = match &entry.record.state {
			FileState::DeletePending(_) => FileState::DeletePending(output.clone()),
			FileState::OriginalDeleted(_) => FileState::OriginalDeleted(output.clone()),
			_ => FileState::SourceGone(output.clone()),
		};

		let mut record = entry.record.clone();
		record.replaces = None;
		self.tree.set_record(heir, record);
		self.set_state(heir, state);
		self.release_claim(canonical, &output.path);
		self.claim(heir, &output.path);

		let mut record = self.tree.file(canonical).record.clone();
		if record.replaces.as_ref() == Some(&output.path) {
			record.replaces = None;
			self.tree.set_record(canonical, record);
			self.save_file(canonical);
		}

		self.dedup.released.retain(|_, (of, _)| *of != canonical);
		self.unhold(canonical);

		if let Err(err) = self.store.flush() {
			self.fail(err);
			return;
		}

		if let Some(placeholder) = placeholder.filter(|placeholder| *placeholder != output.path) {
			self.remove_replaced_output(&placeholder, &output.path);
			self.release_claim(heir, &placeholder);
		}

		let name = self.tree.file(heir).key.display();
		self.log(
			LogKind::Action,
			format!(
				"{name}: its original is gone, so it keeps the archived copy {}",
				output.path.display()
			),
		);
	}

	/// A gone original's placeholder now points at `canonical`; its text still links to the same archived file.
	fn point_at(&mut self, file: FileId, canonical: FileId) {
		let key = self.tree.file(canonical).key.clone();
		let mut state = self.tree.file(file).record.state.clone();
		if let FileState::SourceGone(output) | FileState::DeletePending(output) | FileState::OriginalDeleted(output) =
			&mut state
		{
			output.how = How::Placeholder { canonical: key };
		}
		self.set_state(file, state);
	}

	/// A placeholder archived again whose original vanished before it was archived keeps its content: it adopts the
	/// canonical copy's earlier output, which was held back for it. Returns whether it did.
	pub(super) fn keep_released(&mut self, file: FileId) -> bool {
		let Some((canonical, output)) = self.dedup.released.remove(&file) else {
			return false;
		};

		self.withdraw(file);
		self.adopt(file, canonical, &output);
		self.join_dedup(file);
		true
	}

	/// The placeholder no longer needs its canonical copy's earlier output.
	fn settle_release(&mut self, file: FileId) {
		if let Some((canonical, _)) = self.dedup.released.remove(&file) {
			self.unhold(canonical);
		}
	}

	fn unhold(&mut self, canonical: FileId) {
		let needed = self.dedup.released.values().any(|(of, _)| *of == canonical);
		if !needed && self.dedup.held.remove(&canonical) {
			self.released(canonical);
		}
	}

	/// Whether the file must wait: a placeholder archived again still needs the output it would replace.
	pub(super) fn hold_for_released(&mut self, file: FileId) -> bool {
		let needed = self.dedup.released.values().any(|(of, _)| *of == file);
		if needed {
			self.dedup.held.insert(file);
		}
		needed
	}

	pub(super) fn held_back(&self, file: FileId) -> bool {
		self.dedup.held.contains(&file)
	}

	/// A placeholder placed after its canonical copy let its output go is archived again at once. Returns whether the
	/// file stays Done.
	pub(super) fn dedup_placed(&mut self, file: FileId) -> bool {
		let pointed = match self.tree.file(file).record.state.output().map(|output| &output.how) {
			Some(How::Placeholder { canonical }) => self.tree.file_id(canonical),
			_ => None,
		};
		if let Some(canonical) = pointed
			&& self.canonical_output(canonical).is_none()
		{
			self.archive_again(file);
			return false;
		}

		self.settle_release(file);
		true
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
	use std::sync::mpsc::Receiver;
	use std::time::Duration;

	use super::*;
	use crate::project::scanning::tests::{key, list, owner};
	use crate::record::{CopyReason, FileRecord};
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
}
