//! Deleting originals, the dangerous mode (project layer spec §9). An original is deleted only after its archived
//! file was read back from disk and matched its BLAKE3 (for a placeholder, the canonical copy's archived file too),
//! the source is unchanged since the job, and the file has no open issue. `DeletePending` is written durably before
//! the permanent delete. A file becomes `OriginalDeleted` only from `DeletePending`: by the delete's own result, by a
//! listing that finds the source missing, or at open by `recover_delete` when the source is provably gone.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;
use std::time::Instant;

use cryarchiver_engine::outcome::{ErrorKind, JobError};
use cryarchiver_engine::text::display_path;

use super::duplicates::recorded_output;
use super::jobs::{DeleteCheck, PlaceholderCheck, Refusal, Task};
use super::owner::Owner;
use crate::record::{CopyReason, FileState, How, IssueKind, Output};
use crate::run::Deletion;
use crate::schedule::{Queue, Work, retry_delay};
use crate::tree::FileId;
use crate::view::{LogKind, RowState};

#[derive(Debug, Default)]
pub(crate) struct DangerState {
	/// What each running delete job checked on disk; its intent is refused once the records say something else.
	checked: HashMap<FileId, Checked>,
	/// Placeholders released while their delete was under way; they are settled once it ends.
	released: HashSet<FileId>,
	/// Refused for a reason no retry changes: tried again only when deleting starts again.
	refused: HashSet<FileId>,
	retries: HashMap<FileId, u32>,
	retry_at: BTreeSet<(Instant, FileId)>,
}

#[derive(Debug, Clone, PartialEq)]
struct Checked {
	output: Output,
	/// A placeholder's canonical copy, and its archived output.
	canonical: Option<(FileId, Output)>,
}

impl Owner {
	/// Deleting needs the setting and this run's confirmation: the typed `DELETE`, or the front end's at Start.
	pub(super) fn deleting(&self) -> bool {
		self.settings.delete_originals && self.delete_confirmed
	}

	/// Queues every archived file whose original may go; used when deletion starts, also for files archived earlier.
	pub(super) fn queue_all_deletes(&mut self) {
		self.danger.refused.clear();
		let files: Vec<FileId> = self.tree.file_ids().collect();
		for file in files {
			self.queue_delete(file);
		}
	}

	pub(super) fn queue_delete(&mut self, file: FileId) {
		if !self.deleting() || self.danger.refused.contains(&file) || !self.deletable(file) {
			return;
		}

		if let Some(queue) = self.pipeline.queues.get_mut(&Work::Delete) {
			queue.push(file);
		}
	}

	/// A canonical copy archived again holds its content once more: its placeholders are checked again, also those
	/// refused meanwhile.
	pub(super) fn queue_placeholder_deletes(&mut self, canonical: FileId) {
		for placeholder in self.placeholders_of(canonical) {
			self.danger.refused.remove(&placeholder);
			self.queue_delete(placeholder);
		}
	}

	pub(super) fn cancel_deletes(&mut self) {
		if let Some(queue) = self.pipeline.queues.get_mut(&Work::Delete) {
			*queue = Queue::default();
		}
		self.danger.retry_at.clear();
		self.danger.retries.clear();
		self.pipeline.requeued.retain(|(_, work)| *work != Work::Delete);

		for job in self.pipeline.jobs.values().filter(|job| job.work == Work::Delete) {
			job.cancel.cancel();
		}
	}

	/// Archived with an output that can be checked: encoded, copied as it is, or a placeholder whose canonical copy
	/// holds the content. Never a rewritten reference file, which differs from its source by design, never an output
	/// accepted as it is, and never one a take-over is about to move.
	pub(super) fn deletable(&self, file: FileId) -> bool {
		let entry = self.tree.file(file);
		let FileState::Done(output) = &entry.record.state else {
			return false;
		};

		let checkable = match &output.how {
			How::Copied(CopyReason::Rewritten) => false,
			How::Placeholder { .. } => self.verified_canonical(file).is_some(),
			How::Encoded | How::Copied(_) => true,
		};

		checkable
			&& !entry.removed
			&& output.hash.is_some()
			&& !self.issue_files.contains(&file)
			&& !self.gives_output_to_a_take_over(file)
	}

	/// For a placeholder that isn't lost: its canonical copy, while that holds a content output with a hash and the
	/// same source content. A placeholder matched as the same file has no hash: both records must describe the file
	/// alike, and its job checks the identities and the canonical copy's source.
	fn verified_canonical(&self, file: FileId) -> Option<(FileId, Output)> {
		let record = &self.tree.file(file).record;
		let Some(Output {
			how: How::Placeholder { canonical, lost: false },
			..
		}) = record.state.output()
		else {
			return None;
		};

		let canonical = self
			.tree
			.file_id(canonical)
			.filter(|&canonical| canonical != file && !self.tree.file(canonical).removed)
			.filter(|canonical| !self.issue_files.contains(canonical))?;
		let output = self
			.canonical_output(canonical)
			.filter(|output| output.hash.is_some())?;

		let theirs = &self.tree.file(canonical).record;
		let same_content = match record.full_hash {
			Some(ours) => theirs.full_hash == Some(ours),
			None => (record.size, record.mtime_ns) == (theirs.size, theirs.mtime_ns),
		};
		same_content.then(|| (canonical, output.clone()))
	}

	pub(super) fn delete_task(&mut self, file: FileId) -> Option<Task> {
		if !self.deletable(file) {
			return None;
		}

		let Some(identity) = self.tree.file(file).record.identity else {
			self.note(
				file,
				"original kept: its identity wasn't read when it was archived".into(),
			);
			self.danger.refused.insert(file);
			return None;
		};

		let entry = self.tree.file(file);
		let output = entry.record.state.output()?.clone();
		let canonical = self.verified_canonical(file);
		let placeholder = match &canonical {
			Some((canonical, canonical_output)) => Some(PlaceholderCheck {
				link: canonical_output
					.path
					.relative_from(&self.destination_folder(entry.folder)),
				output: self.destination_path(&canonical_output.path),
				size: canonical_output.size,
				hash: canonical_output.hash?,
				same_file: entry.record.full_hash.is_none().then(|| self.same_file(*canonical)),
			}),
			None => None,
		};

		let check = DeleteCheck {
			output: self.destination_path(&output.path),
			size: output.size,
			hash: output.hash?,
			identity,
			placeholder,
			source_root: self.settings.source.clone(),
		};
		self.danger.checked.insert(file, Checked { output, canonical });
		Some(Task::Delete(check))
	}

	/// The checks passed: `DeletePending` is written durably, then the job may delete. Refused when the records no
	/// longer say what the job checked: a placeholder re-pointed or adopted meanwhile is checked again.
	pub(super) fn delete_intent(&mut self, file: FileId) -> bool {
		let live = self
			.pipeline
			.jobs
			.get(&file)
			.is_some_and(|job| job.work == Work::Delete && !job.cancel.is_cancelled());
		let as_checked = self.danger.checked.get(&file).is_some_and(|checked| {
			self.tree.file(file).record.state.output() == Some(&checked.output)
				&& self.verified_canonical(file) == checked.canonical
		});
		if !live || self.stopping || !self.deleting() || !self.deletable(file) || !as_checked {
			return false;
		}

		let entry = self.tree.file(file);
		let mut record = entry.record.clone();
		let FileState::Done(output) = record.state else {
			return false;
		};
		record.state = FileState::DeletePending(output);
		if let Err(err) = self.store.write_intent(&entry.key, &record) {
			self.fail(err);
			return false;
		}
		self.after_flush();

		self.tree.set_record(file, record);
		self.note_placeholder(file);
		self.touch_file(file);
		!self.crash_point(super::Fault::AfterDeleteIntent)
	}

	/// The delete's own result, applied to the record as it is now: an heir may have adopted another output meanwhile.
	pub(super) fn deleted(&mut self, file: FileId, deletion: &Deletion) {
		let source = self.source_path(&self.tree.file(file).key);
		let output = match self.tree.file(file).record.state.clone() {
			FileState::DeletePending(output) => output,
			FileState::OriginalDeleted(_) if *deletion == Deletion::Deleted => {
				self.log(
					LogKind::Row(RowState::OriginalDeleted),
					format!("deleted {}", display_path(&source)),
				);
				return;
			}
			_ => return,
		};

		match deletion {
			Deletion::Deleted | Deletion::AlreadyGone => {
				self.danger.retries.remove(&file);
				self.set_state(file, FileState::OriginalDeleted(output));
				self.log(
					LogKind::Row(RowState::OriginalDeleted),
					format!("deleted {}", display_path(&source)),
				);
			}
			Deletion::Busy => {
				self.set_state(file, FileState::Done(output));
				if !self.danger.released.contains(&file) {
					self.retry_delete(file, "another program has the original open");
				}
			}
			Deletion::Changed | Deletion::Refused(_) => self.set_state(file, FileState::Done(output)),
		}
	}

	/// Nothing was deleted. An archived file that no longer matches its hash is an issue; a source that changed is
	/// archived again; anything else keeps its original until deleting starts again.
	pub(super) fn delete_refused(&mut self, file: FileId, refusal: Refusal) {
		let record = self.tree.file(file).record.clone();
		let output = match record.state {
			FileState::Done(output) => output,
			FileState::DeletePending(output) => {
				self.set_state(file, FileState::Done(output.clone()));
				output
			}
			_ => return,
		};

		match refusal {
			Refusal::OutputDamaged => {
				let reason = "the archived file doesn't match its recorded BLAKE3";
				self.note(file, format!("original kept: {reason}"));
				self.set_state(
					file,
					FileState::Issue {
						kind: IssueKind::OutputMissing,
						attempt: 0,
						reason: reason.into(),
						output: Some(output),
					},
				);
			}
			Refusal::SourceChanged(now) => {
				self.note(file, "original kept: the source changed since it was archived".into());
				match now {
					Some((size, mtime_ns)) if (size, mtime_ns) == (record.size, record.mtime_ns) => {
						self.identity_changed(file, size, mtime_ns);
					}
					Some((size, mtime_ns)) => self.source_changed(file, size, mtime_ns),
					None => {
						self.danger.refused.insert(file);
					}
				}
			}
			Refusal::Kept(reason) => {
				self.note(file, format!("original kept: {reason}"));
				self.danger.refused.insert(file);
			}
		}
	}

	/// A delete job ended. Without a result the original counts as still there; a placeholder released meanwhile is
	/// settled now that the result is known.
	pub(super) fn delete_finished(&mut self, file: FileId, requeue: bool, result: &Result<(), JobError>) {
		self.danger.checked.remove(&file);
		let cancelled = requeue || result.as_ref().is_err_and(|err| err.kind == ErrorKind::Cancelled);

		if let FileState::DeletePending(output) = self.tree.file(file).record.state.clone() {
			self.set_state(file, FileState::Done(output));
		}

		let released = self.danger.released.remove(&file);
		if released {
			let points_at_its_copy = recorded_output(&self.tree.file(file).record.state)
				.is_some_and(|output| matches!(output.how, How::Placeholder { lost: false, .. }));
			if points_at_its_copy {
				self.release(file);
			}
		} else if let Err(err) = result
			&& !cancelled
		{
			self.retry_delete(file, &err.kind.to_string());
		} else if cancelled {
			self.pipeline.requeued.push((file, Work::Delete));
		}

		if matches!(self.tree.file(file).record.state, FileState::Done(_)) {
			self.member_done(file);
		}
	}

	/// Checked again after a growing delay, as often as automatic retries go; then the original stays.
	fn retry_delete(&mut self, file: FileId, reason: &str) {
		let retries = self.danger.retries.get(&file).copied().unwrap_or(0);
		let Some(delay) = retry_delay(self.tuning.retry_base, retries) else {
			self.danger.retries.remove(&file);
			self.note(file, format!("original kept: {reason}"));
			return;
		};

		self.danger.retries.insert(file, retries + 1);
		self.note(
			file,
			format!(
				"original kept for now: {reason}; checking again in {} s",
				delay.as_secs()
			),
		);
		self.danger.retry_at.insert((Instant::now() + delay, file));
	}

	pub(super) fn retry_deletes(&mut self, now: Instant) {
		while let Some(&(at, file)) = self.danger.retry_at.first() {
			if at > now {
				break;
			}

			self.danger.retry_at.remove(&(at, file));
			self.queue_delete(file);
		}
	}

	pub(super) fn delete_retries_pending(&self) -> bool {
		!self.danger.retry_at.is_empty()
	}

	/// A placeholder released while its delete is under way waits for the delete's result: the original may be gone
	/// by then. Returns whether it waits. A pending delete with no job left is settled as after a crash.
	pub(super) fn release_after_delete(&mut self, file: FileId) -> bool {
		if !matches!(self.tree.file(file).record.state, FileState::DeletePending(_)) {
			return false;
		}

		if let Some(job) = self.pipeline.jobs.get(&file).filter(|job| job.work == Work::Delete) {
			job.cancel.cancel();
			self.danger.released.insert(file);
			return true;
		}

		self.recover_delete(file);
		false
	}

	/// After a crash between `DeletePending` and the delete: an original that is provably gone, its folder there and
	/// the file not, was deleted; anything else is Done again, and checked again before it is deleted.
	pub(super) fn recover_delete(&mut self, file: FileId) {
		let FileState::DeletePending(output) = self.tree.file(file).record.state.clone() else {
			return;
		};

		let source = self.source_path(&self.tree.file(file).key);
		let missing = fs::symlink_metadata(&source).is_err_and(|err| err.kind() == io::ErrorKind::NotFound);
		let gone = missing && source.parent().is_some_and(Path::is_dir);
		if gone {
			self.set_state(file, FileState::OriginalDeleted(output));
		} else {
			self.set_state(file, FileState::Done(output));
		}
	}
}

#[cfg(test)]
mod tests {
	use std::ffi::OsStr;
	use std::path::PathBuf;
	use std::sync::mpsc::Receiver;
	use std::time::Duration;

	use cryarchiver_engine::commit::file_id;
	use cryarchiver_engine::outcome::IoSide;
	use tokio::runtime::Runtime;
	use tokio::sync::mpsc::UnboundedReceiver;
	use tokio::sync::oneshot;

	use super::*;
	use crate::dedup::MIN_DEDUP_SIZE;
	use crate::issues::{IssueKey, Verb};
	use crate::project::jobs::JobMessage;
	use crate::project::owner::Message;
	use crate::project::pipeline::tests::running_job;
	use crate::project::scanning::tests::{key, owner};
	use crate::record::{FileRecord, Hash, Identity, MediaClass, mtime_ns};
	use crate::run::placeholder_text;
	use crate::tree::FolderId;
	use crate::view::{NoticeLevel, Update};

	struct Fixture {
		_dir: tempfile::TempDir,
		owner: Owner,
		updates: Receiver<Update>,
		inbox: UnboundedReceiver<Message>,
		runtime: Runtime,
	}

	impl Fixture {
		fn new() -> Self {
			let dir = tempfile::tempdir().unwrap();
			let source = dir.path().join("source");
			fs::create_dir_all(&source).unwrap();
			let (mut owner, updates) = owner(&source, &dir.path().join("archive"));
			owner.settings.delete_originals = true;
			let inbox = owner.inbox.take().unwrap();

			Self {
				_dir: dir,
				owner,
				updates,
				inbox,
				runtime: Runtime::new().unwrap(),
			}
		}

		fn source(&self, name: &str) -> PathBuf {
			self.owner.settings.source.join(name)
		}

		fn destination(&self, name: &str) -> PathBuf {
			self.owner.settings.destination.join(name)
		}

		/// A source file and its archived output, recorded as the project records them once the output is placed.
		fn archived(&mut self, name: &str, source: &[u8], output: Output) -> FileId {
			fs::write(self.source(name), source).unwrap();
			let archived = self.owner.destination_path(&output.path);
			if !archived.exists() {
				fs::write(&archived, source).unwrap();
			}

			let path = output.path.clone();
			let meta = fs::metadata(self.source(name)).unwrap();
			let record = FileRecord {
				identity: file_id(&self.source(name)).ok().map(Identity::from),
				class: MediaClass::Other,
				full_hash: Some(hash(source)),
				state: FileState::Done(output),
				..FileRecord::discovered(meta.len(), mtime_ns(meta.modified().unwrap()))
			};
			let file = self.owner.tree.add_file(FolderId::ROOT, OsStr::new(name), record);
			self.owner.claim(file, &path);
			self.owner.note_placeholder(file);
			file
		}

		fn copy(&mut self, name: &str, content: &[u8]) -> FileId {
			self.archived(name, content, copied(name, content))
		}

		/// `name` is a duplicate of `canonical`, archived as a placeholder whose text links to its output.
		fn placeholder(&mut self, name: &str, content: &[u8], canonical: &str) -> FileId {
			let text = placeholder_text(canonical, name, content.len() as u64, Some(hash(content)));
			let path = key(&format!("placeholder_{name}.txt"));
			fs::write(self.owner.destination_path(&path), &text).unwrap();
			let output = Output {
				how: How::Placeholder {
					canonical: key(canonical),
					lost: false,
				},
				path,
				size: text.len() as u64,
				hash: Some(hash(text.as_bytes())),
			};
			self.archived(name, content, output)
		}

		/// `name` is the same file as `canonical`, a hard link to it, archived as a placeholder matched by identity
		/// alone: no hash.
		fn same_file_placeholder(&mut self, name: &str, canonical: &str) -> FileId {
			fs::hard_link(self.source(canonical), self.source(name)).unwrap();
			let meta = fs::metadata(self.source(name)).unwrap();
			let text = placeholder_text(canonical, name, meta.len(), None);
			let path = key(&format!("placeholder_{name}.txt"));
			fs::write(self.owner.destination_path(&path), &text).unwrap();
			let output = Output {
				how: How::Placeholder {
					canonical: key(canonical),
					lost: false,
				},
				path: path.clone(),
				size: text.len() as u64,
				hash: Some(hash(text.as_bytes())),
			};

			let record = FileRecord {
				identity: file_id(&self.source(name)).ok().map(Identity::from),
				class: MediaClass::Other,
				state: FileState::Done(output),
				..FileRecord::discovered(meta.len(), mtime_ns(meta.modified().unwrap()))
			};
			let file = self.owner.tree.add_file(FolderId::ROOT, OsStr::new(name), record);
			self.owner.claim(file, &path);
			self.owner.note_placeholder(file);
			file
		}

		fn state(&self, file: FileId) -> FileState {
			self.owner.tree.file(file).record.state.clone()
		}

		fn queued(&self) -> Vec<FileId> {
			self.owner.pipeline.queues[&Work::Delete].iter().collect()
		}

		/// Starts the queued deletes and handles what their jobs send until every job has ended.
		fn run_deletes(&mut self) {
			let _entered = self.runtime.enter();
			self.owner.start_jobs(Instant::now());
			while !self.owner.pipeline.jobs.is_empty() {
				if let Message::Job(file, message) = self.next_message() {
					self.owner.on_job(file, message);
				}
			}
		}

		/// A delete job for `file` that passed its checks and was granted its durable intent.
		fn past_its_intent(&mut self, file: FileId) {
			self.owner.pipeline.queues.get_mut(&Work::Delete).unwrap().remove(file);
			assert!(self.owner.delete_task(file).is_some());
			self.owner.pipeline.jobs.insert(file, running_job(Work::Delete));
			assert!(self.ask_intent(file));
		}

		fn ask_intent(&mut self, file: FileId) -> bool {
			let (reply, answer) = oneshot::channel();
			self.job(file, JobMessage::DeleteIntent { reply });
			answer.blocking_recv().unwrap()
		}

		fn job(&mut self, file: FileId, message: JobMessage) {
			self.owner.on_job(file, message);
		}

		/// The copied file `canonical`'s archived file goes missing, and the user redoes it.
		fn redo_with_its_output_missing(&mut self, canonical: FileId, content: &[u8]) {
			fs::remove_file(self.destination("a.bin")).unwrap();
			self.owner.set_state(
				canonical,
				FileState::Issue {
					kind: IssueKind::OutputMissing,
					attempt: 0,
					reason: "the archived file is missing".into(),
					output: Some(copied("a.bin", content)),
				},
			);
			let issue = self.owner.issue_ids.id(&IssueKey::File(canonical));
			self.owner.resolve(issue, Verb::Retry);
		}

		fn next_message(&mut self) -> Message {
			self.runtime
				.block_on(tokio::time::timeout(Duration::from_secs(20), self.inbox.recv()))
				.unwrap()
				.unwrap()
		}

		/// The log lines and warnings sent so far.
		fn said(&self) -> Vec<String> {
			self.updates
				.try_iter()
				.filter_map(|update| match update {
					Update::Log(entry) => Some(entry.message),
					Update::Notice(notice) if notice.level == NoticeLevel::Warning => Some(notice.text),
					_ => None,
				})
				.collect()
		}
	}

	fn hash(content: &[u8]) -> Hash {
		Hash(*blake3::hash(content).as_bytes())
	}

	fn copied(name: &str, content: &[u8]) -> Output {
		Output {
			how: How::Copied(CopyReason::NotMedia),
			path: key(name),
			size: content.len() as u64,
			hash: Some(hash(content)),
		}
	}

	fn lost(output: &Output) -> Output {
		let How::Placeholder { canonical, .. } = &output.how else {
			panic!("not a placeholder: {output:?}");
		};

		Output {
			how: How::Placeholder {
				canonical: canonical.clone(),
				lost: true,
			},
			..output.clone()
		}
	}

	/// Puts a new file with `content` in the source's place, with the source's modification time.
	fn replace_keeping_the_time(path: &Path, content: &[u8]) {
		let modified = fs::metadata(path).unwrap().modified().unwrap();
		let replacement = path.with_extension("new");
		fs::write(&replacement, content).unwrap();
		fs::File::options()
			.write(true)
			.open(&replacement)
			.unwrap()
			.set_modified(modified)
			.unwrap();
		fs::rename(&replacement, path).unwrap();
	}

	/// Writes `content` into the same file, then sets its modification time back.
	fn rewrite_keeping_the_time(path: &Path, content: &[u8]) {
		let modified = fs::metadata(path).unwrap().modified().unwrap();
		fs::write(path, content).unwrap();
		fs::File::options()
			.write(true)
			.open(path)
			.unwrap()
			.set_modified(modified)
			.unwrap();
	}

	#[test]
	fn a_run_started_without_confirmation_never_deletes_even_when_the_setting_turns_on() {
		for confirmed_at_start in [false, true] {
			let mut fixture = Fixture::new();
			fixture.owner.settings.delete_originals = false;
			let notes = fixture.copy("notes.txt", b"notes");
			fixture.owner.start(confirmed_at_start);

			fixture.owner.settings.delete_originals = true;
			fixture.owner.queue_all_deletes();
			fixture.owner.queue_delete(notes);
			assert!(fixture.queued().is_empty(), "confirmed at start: {confirmed_at_start}");

			fixture.owner.pipeline.jobs.insert(notes, running_job(Work::Delete));
			assert!(!fixture.ask_intent(notes));
			assert!(matches!(fixture.state(notes), FileState::Done(_)));
			assert!(fixture.source("notes.txt").exists());
		}
	}

	#[test]
	fn an_original_whose_size_changed_under_the_same_time_is_never_deleted() {
		let mut fixture = Fixture::new();
		let notes = fixture.copy("notes.txt", b"notes");
		fixture.owner.start(true);
		assert_eq!(fixture.queued(), [notes]);
		let identity = file_id(&fixture.source("notes.txt")).unwrap();
		rewrite_keeping_the_time(&fixture.source("notes.txt"), b"notes, and more");
		assert_eq!(file_id(&fixture.source("notes.txt")).unwrap(), identity);

		fixture.run_deletes();

		assert_eq!(fs::read(fixture.source("notes.txt")).unwrap(), b"notes, and more");
		assert_eq!(fs::read(fixture.destination("notes.txt")).unwrap(), b"notes");
		assert_eq!(fixture.state(notes), FileState::Discovered);
		assert_eq!(fixture.owner.tree.file(notes).record.size, 15);
		assert!(
			fixture
				.said()
				.contains(&"notes.txt: original kept: the source changed since it was archived".to_owned())
		);
	}

	#[test]
	fn an_original_replaced_by_another_file_of_the_same_size_and_time_is_never_deleted() {
		let mut fixture = Fixture::new();
		let notes = fixture.copy("notes.txt", b"notes");
		fixture.owner.start(true);
		replace_keeping_the_time(&fixture.source("notes.txt"), b"NOTES");

		fixture.run_deletes();

		assert_eq!(fs::read(fixture.source("notes.txt")).unwrap(), b"NOTES");
		assert_eq!(fixture.state(notes), FileState::Discovered);
		assert_eq!(fixture.owner.tree.file(notes).record.identity, None);
	}

	#[test]
	fn a_file_archived_without_its_identity_keeps_its_original() {
		let mut fixture = Fixture::new();
		let notes = fixture.copy("notes.txt", b"notes");
		let mut record = fixture.owner.tree.file(notes).record.clone();
		record.identity = None;
		fixture.owner.tree.set_record(notes, record);
		fixture.owner.start(true);

		fixture.run_deletes();

		assert!(fixture.source("notes.txt").exists());
		assert!(matches!(fixture.state(notes), FileState::Done(_)));
		fixture.owner.queue_delete(notes);
		assert!(fixture.queued().is_empty(), "not retried on its own");
	}

	#[test]
	fn a_verified_archived_copy_lets_its_original_go() {
		let mut fixture = Fixture::new();
		let notes = fixture.copy("notes.txt", b"notes");
		fixture.owner.start(true);

		fixture.run_deletes();

		assert!(!fixture.source("notes.txt").exists());
		assert_eq!(fs::read(fixture.destination("notes.txt")).unwrap(), b"notes");
		assert_eq!(
			fixture.state(notes),
			FileState::OriginalDeleted(copied("notes.txt", b"notes"))
		);
	}

	#[test]
	fn an_archived_copy_replaced_by_a_link_to_its_original_never_lets_the_original_go() {
		let mut fixture = Fixture::new();
		let notes = fixture.copy("notes.txt", b"notes");
		fs::remove_file(fixture.destination("notes.txt")).unwrap();
		#[cfg(unix)]
		let linked = std::os::unix::fs::symlink(fixture.source("notes.txt"), fixture.destination("notes.txt")).is_ok();
		#[cfg(windows)]
		let linked =
			std::os::windows::fs::symlink_file(fixture.source("notes.txt"), fixture.destination("notes.txt")).is_ok();
		if !linked {
			eprintln!("skipped: no file link can be made here");
			return;
		}
		fixture.owner.start(true);

		fixture.run_deletes();

		assert_eq!(fs::read(fixture.source("notes.txt")).unwrap(), b"notes");
		assert!(matches!(fixture.state(notes), FileState::Done(_)));
		let said = fixture.said();
		assert!(
			said.contains(
				&"notes.txt: original kept: the archived copy is a link, a junction or not a regular file".into()
			),
			"{said:?}"
		);
	}

	#[test]
	fn an_archived_copy_that_is_a_hard_link_to_its_original_never_lets_the_original_go() {
		let mut fixture = Fixture::new();
		let notes = fixture.copy("notes.txt", b"notes");
		fs::remove_file(fixture.destination("notes.txt")).unwrap();
		fs::hard_link(fixture.source("notes.txt"), fixture.destination("notes.txt")).unwrap();
		fixture.owner.start(true);

		fixture.run_deletes();

		assert_eq!(fs::read(fixture.source("notes.txt")).unwrap(), b"notes");
		assert!(matches!(fixture.state(notes), FileState::Done(_)));
		let said = fixture.said();
		assert!(
			said.contains(
				&"notes.txt: original kept: the archived copy is the original itself, or a hard link to it".into()
			),
			"{said:?}"
		);
	}

	#[test]
	fn a_placeholders_original_is_kept_while_its_canonical_copys_archived_file_is_that_original() {
		let content = vec![7; 1000];
		let mut fixture = Fixture::new();
		let canonical = fixture.copy("a.bin", &content);
		let duplicate = fixture.placeholder("b.bin", &content, "a.bin");
		fs::remove_file(fixture.destination("a.bin")).unwrap();
		fs::hard_link(fixture.source("b.bin"), fixture.destination("a.bin")).unwrap();
		fixture.owner.start(true);
		fixture
			.owner
			.pipeline
			.queues
			.get_mut(&Work::Delete)
			.unwrap()
			.remove(canonical);

		fixture.run_deletes();

		assert_eq!(fs::read(fixture.source("b.bin")).unwrap(), content);
		assert!(matches!(fixture.state(duplicate), FileState::Done(_)));
		let said = fixture.said();
		assert!(
			said.iter().any(|line| line.ends_with(
				"original kept: the archived copy it points to is the original itself, or a hard link to it"
			)),
			"{said:?}"
		);
	}

	#[test]
	fn a_placeholder_is_deleted_only_while_its_canonical_copy_holds_the_content() {
		let content = vec![7; 1000];
		let mut fixture = Fixture::new();
		let canonical = fixture.copy("a.bin", &content);
		let duplicate = fixture.placeholder("b.bin", &content, "a.bin");
		fixture.owner.start(true);
		assert!(fixture.owner.deletable(duplicate));

		let mut damaged = content.clone();
		damaged[0] = 8;
		fs::write(fixture.destination("a.bin"), &damaged).unwrap();
		fixture
			.owner
			.pipeline
			.queues
			.get_mut(&Work::Delete)
			.unwrap()
			.remove(canonical);
		fixture.run_deletes();

		assert!(fixture.source("b.bin").exists());
		assert!(matches!(fixture.state(duplicate), FileState::Done(_)));
		assert!(
			fixture
				.said()
				.iter()
				.any(|line| line.ends_with("the archived copy it points to doesn't match its BLAKE3"))
		);
	}

	#[test]
	fn a_lost_placeholder_or_one_whose_canonical_copy_isnt_verifiable_never_gets_a_delete_job() {
		let content = vec![7; 1000];
		let mut fixture = Fixture::new();
		let canonical = fixture.copy("a.bin", &content);
		let duplicate = fixture.placeholder("b.bin", &content, "a.bin");
		fixture.owner.start(true);
		let placeholder = fixture.state(duplicate).output().unwrap().clone();
		let canonical_record = fixture.owner.tree.file(canonical).record.clone();

		fixture.owner.set_state(duplicate, FileState::Done(lost(&placeholder)));
		assert!(!fixture.owner.deletable(duplicate), "a lost placeholder");
		fixture.owner.set_state(duplicate, FileState::Done(placeholder));

		let accepted = Output {
			hash: None,
			..copied("a.bin", &content)
		};
		fixture.owner.set_state(canonical, FileState::Done(accepted));
		assert!(!fixture.owner.deletable(duplicate), "an output accepted as it is");

		let other = FileRecord {
			full_hash: Some(Hash([9; 32])),
			..canonical_record.clone()
		};
		fixture.owner.tree.set_record(canonical, other);
		assert!(!fixture.owner.deletable(duplicate), "another source content");

		fixture.owner.tree.set_record(canonical, canonical_record);
		assert!(fixture.owner.deletable(duplicate));
	}

	/// `b.bin` is a placeholder of `a.bin` whose delete is past its intent when `a.bin`'s archived file goes missing
	/// and the user redoes it.
	fn a_placeholder_released_during_its_delete() -> (Fixture, FileId) {
		let content = vec![7; 1000];
		let mut fixture = Fixture::new();
		let canonical = fixture.copy("a.bin", &content);
		let duplicate = fixture.placeholder("b.bin", &content, "a.bin");
		fixture.owner.start(true);
		fixture.past_its_intent(duplicate);
		assert!(matches!(fixture.state(duplicate), FileState::DeletePending(_)));

		fixture.redo_with_its_output_missing(canonical, &content);

		assert!(matches!(fixture.state(duplicate), FileState::DeletePending(_)));
		assert!(fixture.owner.pipeline.jobs[&duplicate].cancel.is_cancelled());
		(fixture, duplicate)
	}

	#[test]
	fn a_placeholder_released_before_its_original_went_is_archived_again_from_its_source() {
		let (mut fixture, duplicate) = a_placeholder_released_during_its_delete();

		fixture.job(duplicate, JobMessage::Deleted(Deletion::Busy));
		fixture.job(duplicate, JobMessage::Finished(Ok(())));

		let record = &fixture.owner.tree.file(duplicate).record;
		assert!(matches!(record.state, FileState::Ready | FileState::Queued));
		assert_eq!(record.replaces, Some(key("placeholder_b.bin.txt")));
		assert!(!fixture.queued().contains(&duplicate));
		assert!(!fixture.owner.delete_retries_pending());
	}

	#[test]
	fn a_placeholder_released_once_its_original_went_is_marked_lost() {
		let (mut fixture, duplicate) = a_placeholder_released_during_its_delete();
		let placeholder = fixture.state(duplicate).output().unwrap().clone();

		fixture.job(duplicate, JobMessage::Deleted(Deletion::Deleted));
		fixture.job(duplicate, JobMessage::Finished(Ok(())));

		assert_eq!(fixture.state(duplicate), FileState::OriginalDeleted(lost(&placeholder)));
		assert!(
			fixture
				.said()
				.contains(&"the only archived copy of b.bin is missing".to_owned())
		);
	}

	#[test]
	fn a_placeholder_released_right_after_its_intent_is_never_deleted() {
		let content = vec![7; 1000];
		let mut fixture = Fixture::new();
		let canonical = fixture.copy("a.bin", &content);
		let duplicate = fixture.placeholder("b.bin", &content, "a.bin");
		fixture.owner.start(true);
		fixture
			.owner
			.pipeline
			.queues
			.get_mut(&Work::Delete)
			.unwrap()
			.remove(canonical);
		let _entered = fixture.runtime.enter();
		fixture.owner.start_jobs(Instant::now());

		while !fixture.owner.pipeline.jobs.is_empty() {
			let Message::Job(file, message) = fixture.next_message() else {
				continue;
			};

			match message {
				JobMessage::DeleteIntent { reply } => {
					let go_on = fixture.owner.delete_intent(file);
					assert!(go_on);
					fixture.redo_with_its_output_missing(canonical, &content);
					let _ = reply.send(go_on);
				}
				message => fixture.owner.on_job(file, message),
			}
		}

		assert!(fixture.source("b.bin").exists());
		let record = &fixture.owner.tree.file(duplicate).record;
		assert!(matches!(record.state, FileState::Ready | FileState::Queued));
		assert_eq!(record.replaces, Some(key("placeholder_b.bin.txt")));
	}

	#[test]
	fn an_original_that_cant_be_reached_is_checked_again_and_never_called_changed() {
		let mut fixture = Fixture::new();
		let notes = fixture.copy("notes.txt", b"notes");
		fixture.owner.start(true);
		fs::remove_dir_all(&fixture.owner.settings.source).unwrap();

		fixture.run_deletes();

		assert_eq!(fixture.state(notes), FileState::Done(copied("notes.txt", b"notes")));
		assert!(!fixture.owner.danger.refused.contains(&notes));
		assert!(fixture.owner.delete_retries_pending());
		let said = fixture.said();
		assert!(
			said.iter()
				.any(|line| line.starts_with("notes.txt: original kept for now: ")),
			"{said:?}"
		);
		assert!(!said.iter().any(|line| line.contains("changed since")), "{said:?}");
	}

	#[test]
	fn an_heir_and_a_returned_source_are_queued_once_they_can_be_deleted() {
		let content = vec![7; 1000];
		let mut fixture = Fixture::new();
		let canonical = fixture.copy("a.bin", &content);
		let duplicate = fixture.placeholder("b.bin", &content, "a.bin");
		let notes = fixture.copy("notes.txt", b"notes");
		fixture
			.owner
			.set_state(notes, FileState::SourceGone(copied("notes.txt", b"notes")));
		fixture.owner.start(true);
		*fixture.owner.pipeline.queues.get_mut(&Work::Delete).unwrap() = Queue::default();

		fs::write(fixture.source("a.bin"), b"changed").unwrap();
		fixture.owner.source_changed(canonical, 7, 1);
		assert_eq!(fixture.state(duplicate), FileState::Done(copied("a.bin", &content)));
		assert_eq!(
			fixture.queued(),
			[duplicate],
			"an heir that adopted its canonical copy's output"
		);

		fixture.owner.source_returned(notes);
		assert_eq!(fixture.queued(), [duplicate, notes], "a source that returned");
	}

	#[test]
	fn a_refused_placeholder_is_queued_again_once_its_canonical_copy_is_archived_again() {
		let content = vec![7; 1000];
		let mut fixture = Fixture::new();
		let canonical = fixture.copy("a.bin", &content);
		let duplicate = fixture.placeholder("b.bin", &content, "a.bin");
		fixture.owner.start(true);
		*fixture.owner.pipeline.queues.get_mut(&Work::Delete).unwrap() = Queue::default();
		fixture.owner.danger.refused.insert(duplicate);

		fixture.owner.pipeline.governor.started(Work::Copy, Instant::now());
		fixture.owner.pipeline.jobs.insert(canonical, running_job(Work::Copy));
		fixture.job(canonical, JobMessage::Finished(Ok(())));

		assert_eq!(fixture.queued(), [canonical, duplicate]);
		assert!(!fixture.owner.danger.refused.contains(&duplicate));
	}

	#[test]
	fn a_delete_records_the_output_its_file_holds_when_it_ends() {
		let content = vec![7; 1000];
		let mut fixture = Fixture::new();
		let canonical = fixture.copy("a.bin", &content);
		let duplicate = fixture.placeholder("b.bin", &content, "a.bin");
		fixture.owner.start(true);
		fixture.past_its_intent(duplicate);

		fixture.owner.source_changed(canonical, 5, 1);
		fixture.job(duplicate, JobMessage::Deleted(Deletion::Deleted));
		fixture.job(duplicate, JobMessage::Finished(Ok(())));

		assert_eq!(
			fixture.state(duplicate),
			FileState::OriginalDeleted(copied("a.bin", &content))
		);
	}

	#[test]
	fn an_intent_is_refused_once_the_records_no_longer_say_what_was_checked() {
		let content = vec![7; 1000];
		let mut fixture = Fixture::new();
		fixture.copy("a.bin", &content);
		let duplicate = fixture.placeholder("b.bin", &content, "a.bin");
		fixture.owner.start(true);
		assert!(fixture.owner.delete_task(duplicate).is_some());
		fixture.owner.pipeline.jobs.insert(duplicate, running_job(Work::Delete));

		let mut placeholder = fixture.state(duplicate).output().unwrap().clone();
		placeholder.hash = Some(Hash([3; 32]));
		fixture.owner.set_state(duplicate, FileState::Done(placeholder));

		assert!(!fixture.ask_intent(duplicate));
		assert!(matches!(fixture.state(duplicate), FileState::Done(_)));
	}

	#[test]
	fn a_pending_delete_whose_result_never_came_is_done_again() {
		let mut fixture = Fixture::new();
		let notes = fixture.copy("notes.txt", b"notes");
		fixture.owner.start(true);
		fixture.past_its_intent(notes);

		let source = fixture.source("notes.txt");
		let unreachable = JobError::io(IoSide::Source(&source), &io::Error::other("the drive is gone"));
		fixture.job(notes, JobMessage::Finished(Err(unreachable)));

		assert_eq!(fixture.state(notes), FileState::Done(copied("notes.txt", b"notes")));
		assert!(fixture.owner.delete_retries_pending());
	}

	#[test]
	fn deletes_take_turns_with_an_endless_stream_of_hashes() {
		let mut fixture = Fixture::new();
		let notes = fixture.copy("notes.txt", b"notes");
		for name in ["w.bin", "x.bin", "y.bin", "z.bin"] {
			let record = FileRecord {
				state: FileState::Ready,
				..FileRecord::discovered(MIN_DEDUP_SIZE, 1)
			};
			let file = fixture.owner.tree.add_file(FolderId::ROOT, OsStr::new(name), record);
			assert!(fixture.owner.gate_duplicates(file));
		}
		fixture.owner.plan_dirty_groups();
		fixture.owner.start(true);
		assert_eq!(fixture.queued(), [notes]);
		let _entered = fixture.runtime.enter();

		fixture.owner.start_jobs(Instant::now());
		assert!(
			!fixture.owner.pipeline.jobs.contains_key(&notes),
			"the hashes came first"
		);
		fixture.owner.pipeline.governor.finished(Work::Hash);
		fixture.owner.start_jobs(Instant::now());

		assert!(fixture.owner.pipeline.jobs.contains_key(&notes));
	}

	#[test]
	fn a_same_file_placeholder_is_deleted_only_while_both_records_describe_the_file_alike() {
		let content = vec![7; 1000];
		let mut fixture = Fixture::new();
		let canonical = fixture.copy("a.bin", &content);
		let duplicate = fixture.same_file_placeholder("b.bin", "a.bin");
		fixture.owner.start(true);
		assert!(fixture.owner.deletable(duplicate));

		let seen_through_the_duplicate = FileRecord {
			mtime_ns: fixture.owner.tree.file(canonical).record.mtime_ns + 1,
			..fixture.owner.tree.file(duplicate).record.clone()
		};
		fixture.owner.tree.set_record(duplicate, seen_through_the_duplicate);
		assert!(
			!fixture.owner.deletable(duplicate),
			"the canonical copy's archived content may be older than the file"
		);
		assert!(fixture.owner.delete_task(duplicate).is_none());
	}

	#[test]
	fn a_same_file_placeholder_whose_canonical_copy_changed_has_it_archived_again() {
		let content = vec![7; 1000];
		let mut fixture = Fixture::new();
		let canonical = fixture.copy("a.bin", &content);
		fixture.same_file_placeholder("b.bin", "a.bin");
		let record = fixture.owner.tree.file(canonical).record.clone();

		fixture
			.owner
			.archived_changed(&key("a.bin"), Some((record.size, record.mtime_ns)));
		assert!(
			matches!(fixture.state(canonical), FileState::Done(_)),
			"nothing changed"
		);

		fixture
			.owner
			.archived_changed(&key("a.bin"), Some((record.size, record.mtime_ns + 1)));
		let now = &fixture.owner.tree.file(canonical).record;
		assert_eq!(now.state, FileState::Discovered);
		assert_eq!((now.size, now.mtime_ns), (record.size, record.mtime_ns + 1));
	}
}
