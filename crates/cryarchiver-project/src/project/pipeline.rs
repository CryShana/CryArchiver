//! A file's way through the run (project layer spec §5 and §8): the ready rule, the queues, the governor, the jobs,
//! the commit intent, retries and issues.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cryarchiver_engine::commit::free_space;
use cryarchiver_engine::decision::Decision;
use cryarchiver_engine::engine::{Encode, Job, JobEvent, Stage};
use cryarchiver_engine::naming::{collision_key, destination_name, placeholder_name};
use cryarchiver_engine::options::JobOptions;
use cryarchiver_engine::outcome::{ErrorClass, ErrorKind, JobError, threads_note};
use cryarchiver_engine::quality::memory::SharedMemory;
use cryarchiver_engine::sink::ArchivalSink;
use cryarchiver_engine::workspace::ARCHIVE_DIR;
use tokio_util::sync::CancellationToken;

use super::jobs::{JobContext, JobMessage, Made, SourceIdentity, Target, Task};
use super::owner::{Message, Owner};
use super::resolve::Deferred;
use crate::keys::RelPath;
use crate::memory::MemoryBudget;
use crate::record::{CopyReason, FailureLog, FileRecord, FileState, How, IssueKind, MediaClass, Output};
use crate::references::{self, RefKind};
use crate::run::{ArchiveFolders, Staged};
use crate::schedule::{DiskGuard, Governor, GovernorConfig, Queue, Waits, Work, retry_delay};
use crate::tree::{FileId, FolderId};
use crate::view::{LogKind, NoticeLevel, size_text};
use crate::watch::{ReadyCheck, check_ready, is_stable};

const READY_CHECKS: usize = 8;
/// The longest a file another program holds open for writing waits between two ready checks.
const BUSY_BACKOFF: Duration = Duration::from_secs(5);
/// How often in a session a file is archived again because its identity changed.
const IDENTITY_CHANGES: u32 = 3;
const IDENTITY_UNSTABLE: &str = "its identity changes every time it is read";
const START_ORDER: [Work; 5] = [Work::Placeholder, Work::Hash, Work::Delete, Work::Copy, Work::Encode];
const DELETES_FIRST: [Work; 5] = [Work::Placeholder, Work::Delete, Work::Hash, Work::Copy, Work::Encode];

pub(crate) struct RunningJob {
	pub(super) work: Work,
	pub(super) attempt: u32,
	pub(super) cancel: CancellationToken,
	pub(super) fraction: f64,
	pub(super) stage: Option<Stage>,
	pub(super) video_waiting: bool,
	pub(super) requeue: bool,
	/// The job found that the duplicate it was writing a placeholder for is no longer the same file.
	pub(super) not_same_file: bool,
	pending: Option<Output>,
	/// A take-over's holder and the output it held there, from the commit intent until the rename is confirmed.
	moved_from: Option<(FileId, RelPath)>,
}

pub(crate) struct Pipeline {
	pub(super) observed: HashMap<FileId, Instant>,
	checking: HashSet<FileId>,
	/// Files a ready check found busy: not checked again before the time, and the wait that led to it.
	busy: HashMap<FileId, (Instant, Duration)>,
	pub(super) queues: HashMap<Work, Queue>,
	pub(super) waits: Waits,
	pub(super) retry_at: BTreeSet<(Instant, FileId)>,
	pub(super) retries: HashMap<FileId, u32>,
	/// The thread budget of a file's next attempt, once a tool crashed in an earlier one.
	pub(super) retry_threads: HashMap<FileId, usize>,
	/// Work whose job ended cancelled, back at the front of its queue from the next tick on, so a job cancelled every
	/// time it starts never restarts back to back.
	pub(super) requeued: Vec<(FileId, Work)>,
	pub(super) jobs: HashMap<FileId, RunningJob>,
	/// Files ready to start again while their earlier job still winds down; they start once it has finished.
	pub(super) deferred: HashSet<FileId>,
	pub(super) governor: Governor,
	/// A hash started while deletes waited: the next free disk slot goes to a delete, so an endless stream of
	/// hashes in watch mode can't hold deletes back for good.
	deletes_first: bool,
	pub(super) encoder: Arc<dyn Encode>,
	pub(super) image_threads: usize,
	pub(super) memory: Arc<MemoryBudget>,
	pub(super) quality: Arc<SharedMemory>,
	pub(super) sink: ArchivalSink,
	pub(super) folders: ArchiveFolders,
	/// The destination folders that refused an output this session, each announced once.
	refused_folders: HashSet<String>,
	pub(super) disk: DiskGuard,
	last_disk_check: Option<Instant>,
	pub(super) chosen_copies: HashSet<FileId>,
	/// Output names by destination folder and collision key.
	pub(super) claims: HashMap<(RelPath, String), FileId>,
	taken: HashSet<(RelPath, String)>,
	pub(super) notes: HashMap<FileId, Vec<String>>,
	pub(super) workers_dirty: bool,
	identity_changes: HashMap<FileId, u32>,
}

impl RunningJob {
	/// The output named in the job's commit intent, until it is placed or taken.
	pub(super) fn pending_output(&self) -> Option<&Output> {
		self.pending.as_ref()
	}

	pub(super) fn pending_output_mut(&mut self) -> Option<&mut Output> {
		self.pending.as_mut()
	}

	pub(super) fn moved_from(&self) -> Option<(FileId, &RelPath)> {
		self.moved_from.as_ref().map(|(holder, path)| (*holder, path))
	}
}

impl Pipeline {
	pub(crate) fn new(
		source: &Path,
		destination: &Path,
		config: GovernorConfig,
		encoder: Arc<dyn Encode>,
		image_threads: usize,
		memory: Arc<MemoryBudget>,
		quality: Arc<SharedMemory>,
	) -> Self {
		let folders = ArchiveFolders::new(source, destination);

		Self {
			observed: HashMap::new(),
			checking: HashSet::new(),
			busy: HashMap::new(),
			queues: START_ORDER.iter().map(|work| (*work, Queue::default())).collect(),
			waits: Waits::default(),
			retry_at: BTreeSet::new(),
			retries: HashMap::new(),
			retry_threads: HashMap::new(),
			requeued: Vec::new(),
			jobs: HashMap::new(),
			deferred: HashSet::new(),
			governor: Governor::new(config),
			deletes_first: false,
			encoder,
			image_threads,
			memory,
			quality,
			sink: ArchivalSink::checked_by(destination, Arc::new(folders.clone())),
			folders,
			refused_folders: HashSet::new(),
			disk: DiskGuard::default(),
			last_disk_check: None,
			chosen_copies: HashSet::new(),
			claims: HashMap::new(),
			taken: HashSet::new(),
			notes: HashMap::new(),
			workers_dirty: false,
			identity_changes: HashMap::new(),
		}
	}
}

impl Owner {
	/// Starts watching a discovered file for the ready rule.
	pub(super) fn observe(&mut self, file: FileId) {
		self.pipeline.observed.insert(file, Instant::now());
		self.pipeline.busy.remove(&file);
	}

	/// Another program holds the file open for writing: it is checked again from the next tick on, and less often
	/// while it stays busy.
	fn still_busy(&mut self, file: FileId) {
		let now = Instant::now();
		let wait = self
			.pipeline
			.busy
			.get(&file)
			.map_or(self.tuning.tick, |&(_, wait)| (wait * 2).min(BUSY_BACKOFF));

		self.pipeline.observed.insert(file, now);
		self.pipeline.busy.insert(file, (now + wait, wait));
	}

	pub(super) fn unobserve(&mut self, file: FileId) {
		self.pipeline.observed.remove(&file);
		self.pipeline.busy.remove(&file);
	}

	pub(super) fn check_discovered(&mut self, now: Instant) {
		let window = self.settings.stability_window;
		let candidates: Vec<FileId> = self
			.pipeline
			.observed
			.iter()
			.filter(|(file, since)| {
				let record = &self.tree.file(**file).record;
				let due = self.pipeline.busy.get(*file).is_none_or(|&(at, _)| at <= now);
				!self.pipeline.checking.contains(file) && due && is_stable(record.mtime_ns, **since, now, window)
			})
			.map(|(file, _)| *file)
			.take(READY_CHECKS.saturating_sub(self.pipeline.checking.len()))
			.collect();

		for file in candidates {
			self.pipeline.checking.insert(file);
			let entry = self.tree.file(file);
			let path = self.source_path(&entry.key);
			let (size, mtime_ns) = (entry.record.size, entry.record.mtime_ns);
			let want_content = matches!(
				references::kind_of(&entry.name),
				Some(RefKind::CueSheet | RefKind::Playlist)
			);
			let messages = self.messages.clone();
			tokio::task::spawn_blocking(move || {
				let check = check_ready(&path, size, mtime_ns, want_content);
				let _ = messages.send(Message::ReadyChecked(file, check));
			});
		}
	}

	pub(super) fn on_ready_checked(&mut self, file: FileId, check: ReadyCheck) {
		self.pipeline.checking.remove(&file);
		let entry = self.tree.file(file);
		if entry.removed || entry.record.state != FileState::Discovered {
			return;
		}

		match check {
			ReadyCheck::Changed { size, mtime_ns } => {
				self.leave_dedup(file);
				let mut record = self.tree.file(file).record.clone();
				record.size = size;
				record.mtime_ns = mtime_ns;
				record.identity = None;
				record.window_hash = None;
				record.full_hash = None;
				self.tree.set_record(file, record);
				self.save_file(file);
				self.touch_file(file);
				self.observe(file);
			}
			ReadyCheck::Busy => self.still_busy(file),
			ReadyCheck::Unreadable(reason) => {
				self.unobserve(file);
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
			ReadyCheck::Ready { class, content } => {
				let mut record = entry.record.clone();
				self.unobserve(file);
				record.class = class;
				self.tree.set_record(file, record);
				if let Some(content) = content {
					self.remember_content(file, content);
				}

				self.set_state(file, FileState::Ready);
				self.gate(file);
			}
		}
	}

	/// Everything a ready file waits for before it is queued: the files a reference names, or the dedup hashes.
	pub(super) fn gate(&mut self, file: FileId) {
		if !self.gate_references(file) && !self.gate_duplicates(file) {
			self.enqueue(file);
		}
	}

	pub(super) fn enqueue(&mut self, file: FileId) {
		if self.tree.file(file).record.state != FileState::Queued {
			self.set_state(file, FileState::Queued);
		}

		let work = self.work_for(file);
		if let Some(queue) = self.pipeline.queues.get_mut(&work) {
			queue.push(file);
		}
	}

	pub(super) fn work_for(&self, file: FileId) -> Work {
		let record = &self.tree.file(file).record;
		if self.is_duplicate(file) {
			Work::Placeholder
		} else if self.pipeline.chosen_copies.contains(&file)
			|| record.class == MediaClass::Other
			|| self.is_reference(file)
		{
			Work::Copy
		} else {
			Work::Encode
		}
	}

	pub(super) fn may_run(&self) -> bool {
		self.started && !self.paused && !self.stopping && !self.pipeline.disk.is_paused()
	}

	pub(super) fn start_jobs(&mut self, now: Instant) {
		if !self.may_run() {
			return;
		}

		let order = if self.pipeline.deletes_first {
			DELETES_FIRST
		} else {
			START_ORDER
		};
		for work in order {
			while self.pipeline.governor.may_start(work, now) {
				let next = self.pipeline.queues.get_mut(&work).and_then(Queue::pop_front);
				let Some(file) = next else {
					break;
				};

				match work {
					Work::Hash => {
						self.start_hash(file, now);
						self.pipeline.deletes_first = self
							.pipeline
							.queues
							.get(&Work::Delete)
							.is_some_and(|queue| !queue.is_empty());
					}
					Work::Delete => {
						self.pipeline.deletes_first = false;
						self.start_job(file, work, now);
					}
					_ => self.start_job(file, work, now),
				}
			}
		}
	}

	fn start_job(&mut self, file: FileId, work: Work, now: Instant) {
		let entry = self.tree.file(file);
		let startable = if work == Work::Delete {
			self.deletable(file) && !self.pipeline.jobs.contains_key(&file)
		} else {
			!entry.removed && entry.record.state == FileState::Queued
		};
		if !startable {
			return;
		}

		if self.pipeline.jobs.contains_key(&file) {
			self.pipeline.deferred.insert(file);
			return;
		}

		let Some(task) = self.task_for(file, work) else {
			return;
		};
		let entry = self.tree.file(file);
		let attempt = self.last_attempts.get(&entry.key).copied().unwrap_or(0) + 1;

		let cancel = CancellationToken::new();
		let context = JobContext {
			file,
			source: self.source_path(&entry.key),
			size: entry.record.size,
			mtime_ns: entry.record.mtime_ns,
			identity: entry.record.identity,
			sink: self.pipeline.sink.clone(),
			folders: self.pipeline.folders.clone(),
			encoder: self.pipeline.encoder.clone(),
			messages: self.messages.clone(),
			cancel: cancel.clone(),
		};

		self.pipeline.governor.started(work, now);
		self.pipeline.jobs.insert(
			file,
			RunningJob {
				work,
				attempt,
				cancel,
				fraction: 0.0,
				stage: None,
				video_waiting: false,
				requeue: false,
				not_same_file: false,
				pending: None,
				moved_from: None,
			},
		);
		self.pipeline.workers_dirty = true;
		if work != Work::Delete {
			self.set_state(file, FileState::InProgress { attempt });

			let key = self.tree.file(file).key.clone();
			if let Err(err) = self.store.remove_decisions(&key) {
				self.fail(err);
			}
		}
		tokio::spawn(context.run(task));
	}

	pub(super) fn task_for(&mut self, file: FileId, work: Work) -> Option<Task> {
		let entry = self.tree.file(file);
		match work {
			Work::Encode => Some(Task::Encode(Job {
				source: self.source_path(&entry.key),
				speed: self.settings.speed.into(),
				keep_metadata: true,
				drop_aux: !self.settings.keep_auxiliary_images,
				min_savings: self.settings.min_savings_fraction(),
				overrides: self.settings.overrides,
				options: JobOptions {
					threads: self.pipeline.retry_threads.get(&file).copied(),
					..JobOptions::default()
				},
			})),
			Work::Copy if self.pipeline.chosen_copies.contains(&file) => Some(Task::Copy(CopyReason::Chosen)),
			Work::Copy => Some(self.reference_task(file).unwrap_or(Task::Copy(CopyReason::NotMedia))),
			Work::Placeholder => self.duplicate_task(file),
			Work::Delete => self.delete_task(file),
			Work::Hash => None,
		}
	}

	pub(super) fn on_job(&mut self, file: FileId, message: JobMessage) {
		match message {
			JobMessage::Event(event) => self.on_job_event(file, event),
			JobMessage::Decided(decision) => self.decided(file, &decision),
			JobMessage::Commit {
				staged,
				made,
				identity,
				reply,
			} => {
				let target = self.commit_intent(file, &staged, &made, identity);
				let _ = reply.send(target);
			}
			JobMessage::Placed => self.placed(file),
			JobMessage::Taken => self.name_taken(file),
			JobMessage::Kept(reason) => self.note(file, kept_note(&reason)),
			JobMessage::NotSameFile => {
				if let Some(job) = self.pipeline.jobs.get_mut(&file) {
					job.not_same_file = true;
				}
			}
			JobMessage::SourceChanged(now) => self.job_source_changed(file, now),
			JobMessage::ArchivedChanged { file: archived, now } => self.archived_changed(&archived, now),
			JobMessage::DeleteIntent { reply } => {
				let go_on = self.delete_intent(file);
				let _ = reply.send(go_on);
			}
			JobMessage::Deleted(deletion) => self.deleted(file, &deletion),
			JobMessage::DeleteRefused(refusal) => self.delete_refused(file, refusal),
			JobMessage::Finished(result) => self.job_finished(file, result),
		}
	}

	/// The job found its source changed since the record was made, with no listing to say so: the file is archived
	/// again as it is now.
	fn job_source_changed(&mut self, file: FileId, now: Option<(u64, i64)>) {
		let entry = self.tree.file(file);
		let ours = matches!(
			entry.record.state,
			FileState::InProgress { .. } | FileState::Committing { .. }
		);
		let Some((size, mtime_ns)) = now.filter(|&now| now != (entry.record.size, entry.record.mtime_ns)) else {
			return;
		};
		if entry.removed || !ours {
			return;
		}

		self.note(
			file,
			"the source changed before it was archived: archiving it again".into(),
		);
		self.source_changed(file, size, mtime_ns);
	}

	fn on_job_event(&mut self, file: FileId, event: JobEvent) {
		let Some(job) = self.pipeline.jobs.get_mut(&file) else {
			return;
		};

		match event {
			JobEvent::Progress(fraction) => {
				job.fraction = fraction;
				self.tree.set_fraction(file, fraction);
				self.touch_file(file);
			}
			JobEvent::Stage(stage) => {
				let waiting = stage == Stage::Queued;
				if waiting != job.video_waiting {
					job.video_waiting = waiting;
					self.pipeline.governor.set_video_waiting(waiting);
				}
				job.stage = Some(stage);
				self.touch_file(file);
			}
			JobEvent::Warning(warning) => self.note(file, warning),
			JobEvent::Parts { .. }
			| JobEvent::Probe { .. }
			| JobEvent::Probed { .. }
			| JobEvent::EncodeAgain(_)
			| JobEvent::Decision(_) => {
				return;
			}
		}

		self.pipeline.workers_dirty = true;
	}

	/// Kept with the attempt of the job that sent it, in the next batch.
	fn decided(&mut self, file: FileId, decision: &Decision) {
		let Some(attempt) = self.pipeline.jobs.get(&file).map(|job| job.attempt) else {
			return;
		};
		if self.tree.file(file).removed || *decision == Decision::default() {
			return;
		}

		let key = self.tree.file(file).key.clone();
		if let Err(err) = self.store.set_decision(&key, attempt, decision) {
			self.fail(err);
		}
	}

	pub(super) fn note(&mut self, file: FileId, text: String) {
		let name = self.tree.file(file).key.display();
		self.log(LogKind::Action, format!("{name}: {text}"));
		self.pipeline.notes.entry(file).or_default().push(text);
	}

	/// Names the output and writes `Committing` durably; only then may the job rename it into place.
	fn commit_intent(
		&mut self,
		file: FileId,
		staged: &Staged,
		made: &Made,
		identity: SourceIdentity,
	) -> Option<Target> {
		let attempt = self
			.pipeline
			.jobs
			.get(&file)
			.filter(|job| !job.cancel.is_cancelled())?
			.attempt;
		let entry = self.tree.file(file);
		let ours = matches!(
			entry.record.state,
			FileState::InProgress { .. } | FileState::Committing { .. }
		);
		if self.stopping || entry.removed || !ours {
			return None;
		}

		if !identity.agrees_with(entry.record.identity) {
			let (size, mtime_ns) = (entry.record.size, entry.record.mtime_ns);
			self.note(
				file,
				"the source was replaced by another file: archiving it again".into(),
			);
			self.identity_changed(file, size, mtime_ns);
			return None;
		}

		if let Made::Placeholder { canonical, target } = made
			&& !self.still_points_at(canonical, target)
		{
			return None;
		}

		let moved_from = match made {
			Made::Moved { .. } if !self.take_over_holds(file, &staged.path) => return None,
			Made::Moved { .. } => self.dedup_holder(file),
			_ => None,
		};

		self.keep_depended_output(file);
		let path = self.output_path(file, made);
		let (how, hash) = match made {
			Made::Encoded { .. } => (How::Encoded, Some(staged.hash)),
			Made::Copied(reason) => (How::Copied(*reason), Some(staged.hash)),
			Made::Placeholder { canonical, .. } => (
				How::Placeholder {
					canonical: canonical.clone(),
					lost: false,
				},
				Some(staged.hash),
			),
			Made::Moved { how, hash, .. } => (how.clone(), *hash),
		};
		let output = Output {
			how,
			path: path.clone(),
			size: staged.size,
			hash,
		};

		let entry = self.tree.file(file);
		let mut record = entry.record.clone();
		record.identity = identity.to_record();
		record.state = FileState::Committing {
			attempt,
			output: output.clone(),
		};
		if let Err(err) = self.store.write_intent(&entry.key, &record) {
			self.fail(err);
			return None;
		}
		self.after_flush();

		let replaces = record
			.replaces
			.as_ref()
			.filter(|previous| **previous == path)
			.map(|previous| self.destination_path(previous));
		self.tree.set_record(file, record);
		self.note_placeholder(file);
		self.touch_file(file);
		self.claim(file, &path);
		if self.crash_point(super::Fault::AfterCommitIntent) {
			return None;
		}

		if let Some(job) = self.pipeline.jobs.get_mut(&file) {
			job.pending = Some(output);
			job.moved_from = moved_from;
		}
		Some(Target {
			path: self.destination_path(&path),
			replaces,
		})
	}

	/// The output is in place, and only now proven ours. A file whose source changed meanwhile keeps the state that
	/// change gave it, and its re-archive replaces this output; a file whose source vanished while still committing
	/// keeps it as `SourceGone`, and one whose source changed first leaves it behind as a file the project doesn't
	/// know, since its record describes the new content; a placeholder that adopted its canonical copy's output
	/// meanwhile lets this one go once that is durable. A take-over whose source changed or vanished meanwhile gives the
	/// moved output back to its holder. An earlier output stays while a record depends on it; one replaced by a file
	/// no longer committing goes only once the record naming the new output is durable.
	fn placed(&mut self, file: FileId) {
		let Some(job) = self.pipeline.jobs.get_mut(&file) else {
			return;
		};
		let Some(output) = job.pending.take() else {
			return;
		};
		let moved_from = job.moved_from.take();
		if self.crash_point(super::Fault::AfterPlaced) {
			return;
		}

		let entry = self.tree.file(file);
		let (removed, committing) = (
			entry.removed,
			matches!(entry.record.state, FileState::Committing { .. }),
		);
		if !committing && let Some((holder, from)) = moved_from {
			self.give_back_moved_output(file, holder, &from, &output.path);
			return;
		}

		let previous = entry.record.replaces.clone().filter(|previous| {
			*previous != output.path && !self.claimed_by_other(file, previous) && !self.has_dependents(file)
		});
		if let Some(previous) = previous {
			self.release_claim(file, &previous);
			if committing || removed {
				self.remove_replaced_output(&previous, &output.path);
			} else {
				self.remove_once_durable(file, previous);
			}
		}

		if removed {
			if committing {
				self.placed_after_removal(file, output);
			}
			return;
		}

		let mut record = self.tree.file(file).record.clone();
		if !committing {
			if record.state.output().is_some_and(|kept| kept.path != output.path) {
				self.remove_once_durable(file, output.path);
			} else {
				record.replaces = Some(output.path);
				self.tree.set_record(file, record);
				self.save_file(file);
			}
			return;
		}

		record.replaces = None;
		self.tree.set_record(file, record);
		self.pipeline.retries.remove(&file);
		self.pipeline.retry_threads.remove(&file);
		self.set_state(file, FileState::Done(output));
		self.output_placed_during_walk(file);
		self.forget_reference_content(file);
		self.file_settled(file);
	}

	/// The file left the project because its source vanished during the rename: the output stays, tracked.
	fn placed_after_removal(&mut self, file: FileId, output: Output) {
		self.claim(file, &output.path);

		let entry = self.tree.file(file);
		let (folder, name) = (entry.folder, entry.name.clone());
		let record = FileRecord {
			replaces: None,
			state: FileState::SourceGone(output),
			..entry.record.clone()
		};
		self.tree.add_file(folder, &name, record);
		self.note_placeholder(file);
		self.track_issue(file);
		self.save_file(file);
		self.touch_file(file);
		self.file_added(file);
		self.output_placed_during_walk(file);
	}

	/// A file became Done or was resolved: whatever waited for it may go on.
	pub(super) fn file_settled(&mut self, file: FileId) {
		for waiter in self.pipeline.waits.settled(file) {
			self.released(waiter);
		}
	}

	pub(super) fn released(&mut self, waiter: FileId) {
		if self.tree.file(waiter).record.state == FileState::Queued && !self.tree.file(waiter).removed {
			self.enqueue(waiter);
		}
	}

	fn name_taken(&mut self, file: FileId) {
		let Some(job) = self.pipeline.jobs.get_mut(&file) else {
			return;
		};
		let Some(output) = job.pending.take() else {
			return;
		};

		job.moved_from = None;
		self.release_claim(file, &output.path);
		self.pipeline.taken.insert(name_key(&output.path));
	}

	fn job_finished(&mut self, file: FileId, result: Result<(), JobError>) {
		let Some(job) = self.pipeline.jobs.remove(&file) else {
			return;
		};

		self.pipeline.governor.finished(job.work);
		if job.video_waiting {
			self.pipeline.governor.set_video_waiting(false);
		}
		self.pipeline.workers_dirty = true;
		self.live.stale.remove(&file);

		if let Some(abandoned) = &job.pending {
			self.release_intent_claim(file, &abandoned.path);
		}

		if self.pipeline.deferred.remove(&file) {
			self.released(file);
		}

		if job.work == Work::Delete {
			self.delete_finished(file, job.requeue, &result);
			return;
		}

		let still_ours = matches!(
			self.tree.file(file).record.state,
			FileState::InProgress { .. } | FileState::Committing { .. }
		);
		if job.not_same_file && still_ours && !self.tree.file(file).removed {
			self.not_the_same_file(file);
			return;
		}

		let Err(err) = result else {
			if still_ours {
				self.requeue(file, job.work);
			} else {
				self.member_done(file);
				self.queue_delete(file);
				self.queue_placeholder_deletes(file);
			}
			return;
		};
		if !still_ours || self.tree.file(file).removed {
			self.queue_delete(file);
			return;
		}

		if job.requeue || err.kind == ErrorKind::Cancelled {
			self.requeue(file, job.work);
			return;
		}

		if let ErrorKind::InsufficientSpace { need, free } = err.kind {
			self.pipeline.disk.pause(need);
			self.notice(
				NoticeLevel::Warning,
				format!(
					"paused: low disk space ({} free, {} needed)",
					size_text(free),
					size_text(need)
				),
			);
			self.requeue(file, job.work);
			return;
		}

		let retries = self.pipeline.retries.get(&file).copied().unwrap_or(0);
		if err.kind.class() == ErrorClass::AutoRetry
			&& let Some(delay) = retry_delay(self.tuning.retry_base, retries)
		{
			self.pipeline.retries.insert(file, retries + 1);
			let job_threads = self.pipeline.retry_threads.get(&file).copied();
			let fewer = err
				.fewer_threads(job_threads)
				.map(|threads| format!(" {}", threads_note(threads)))
				.unwrap_or_default();
			if let Some(threads) = err.retry_threads(job_threads) {
				self.pipeline.retry_threads.insert(file, threads);
			}
			self.note(file, format!("{}; retrying in {} s{fewer}", err.kind, delay.as_secs()));
			self.set_state(file, FileState::Queued);
			self.pipeline.retry_at.insert((Instant::now() + delay, file));
			return;
		}

		self.raise_issue(file, job.attempt, &err);
	}

	/// The source's identity isn't the one recorded: another file took its place, and it is archived again. On some
	/// filesystems a file's identity changes every time it is read; such a file would be archived forever, so after a
	/// few times in a session it is an issue instead.
	pub(super) fn identity_changed(&mut self, file: FileId, size: u64, mtime_ns: i64) {
		if self.verify_first(file, Deferred::IdentityChanged { size, mtime_ns }) {
			return;
		}

		let changes = self.pipeline.identity_changes.entry(file).or_default();
		*changes += 1;
		let unstable = *changes > IDENTITY_CHANGES;

		self.source_changed(file, size, mtime_ns);
		if unstable {
			self.withdraw(file);
			self.set_state(
				file,
				FileState::Issue {
					kind: IssueKind::SourceUnreadable,
					attempt: 0,
					reason: IDENTITY_UNSTABLE.into(),
					output: None,
				},
			);
		}
	}

	/// The user asked for the file again: its identity may change a few more times.
	pub(super) fn forget_identity_changes(&mut self, file: FileId) {
		self.pipeline.identity_changes.remove(&file);
	}

	/// Back to the front of the queue on the next tick.
	fn requeue(&mut self, file: FileId, work: Work) {
		self.set_state(file, FileState::Queued);
		self.pipeline.requeued.push((file, work));
	}

	/// Work requeued since the last tick goes to the front of the queue for the file's current role, which may have
	/// changed meanwhile; a delete is checked again first.
	pub(super) fn requeue_due(&mut self) {
		for (file, work) in std::mem::take(&mut self.pipeline.requeued) {
			if work == Work::Delete {
				self.queue_delete(file);
				continue;
			}

			let entry = self.tree.file(file);
			if entry.removed || entry.record.state != FileState::Queued || self.pipeline.waits.is_waiting(file) {
				continue;
			}

			let work = self.work_for(file);
			if let Some(queue) = self.pipeline.queues.get_mut(&work) {
				queue.push_front(file);
			}
		}
	}

	fn raise_issue(&mut self, file: FileId, attempt: u32, err: &JobError) {
		let key = self.tree.file(file).key.clone();
		let log = FailureLog {
			error: err.kind.to_string(),
			log_tail: err.log_tail.clone(),
		};
		if let Err(store_err) = self.store.set_log(&key, attempt, &log) {
			self.fail(store_err);
		}
		self.last_attempts.insert(key.clone(), attempt);
		self.pipeline.retries.remove(&file);
		self.pipeline.retry_threads.remove(&file);

		self.give_up_output(file);
		self.set_state(
			file,
			FileState::Issue {
				kind: issue_kind(&err.kind),
				attempt,
				reason: err.kind.to_string(),
				output: None,
			},
		);
		self.log(
			LogKind::Row(self.row_state(file)),
			format!("{}: {}", key.display(), err.kind),
		);

		if let ErrorKind::UnsafeDestination(folder) = &err.kind
			&& self.pipeline.refused_folders.insert(folder.clone())
		{
			self.notice(
				NoticeLevel::Warning,
				format!("{folder}; the files archived there stay open issues until that changes"),
			);
		}
	}

	pub(super) fn retry_due(&mut self, now: Instant) {
		while let Some(&(at, file)) = self.pipeline.retry_at.first() {
			if at > now {
				break;
			}

			self.pipeline.retry_at.remove(&(at, file));
			if self.tree.file(file).record.state == FileState::Queued {
				self.enqueue(file);
			}
		}
	}

	/// Cancels running jobs and puts them back in the queue; nothing is left half-written (spec §8.4).
	pub(super) fn cancel_and_requeue(&mut self, files: &[FileId]) {
		for file in files {
			if let Some(job) = self.pipeline.jobs.get_mut(file) {
				job.requeue = true;
				job.cancel.cancel();
			}
		}
	}

	pub(super) fn check_disk(&mut self, now: Instant) {
		if !self.pipeline.disk.is_paused()
			|| self
				.pipeline
				.last_disk_check
				.is_some_and(|last| now.saturating_duration_since(last) < self.tuning.disk_poll)
		{
			return;
		}

		self.pipeline.last_disk_check = Some(now);
		let free = free_space(&self.settings.destination).unwrap_or(0);
		if self.pipeline.disk.check(free) {
			self.log(LogKind::Action, "disk space is back: resuming".into());
			self.view.status_dirty = true;
		}
	}

	/// The destination path for a file's output, avoiding names other files hold and names something already occupies
	/// in the destination, unless that is the file's own earlier output.
	fn output_path(&mut self, file: FileId, made: &Made) -> RelPath {
		let entry = self.tree.file(file);
		let (folder, replaces) = (entry.folder, entry.record.replaces.clone());
		let destination = self.destination_folder(folder);
		let base = match made {
			Made::Encoded { extension }
			| Made::Moved {
				how: How::Encoded,
				extension,
				..
			} => self
				.tree
				.folder(entry.folder)
				.naming
				.encoded_name(&entry.name, OsStr::new(extension)),
			Made::Copied(_) | Made::Moved { .. } => self.copied_output_name(file),
			Made::Placeholder { .. } => placeholder_name(&entry.name),
		};
		let base = if destination.is_root() {
			outside_the_work_area(base)
		} else {
			base
		};

		let mut candidate = base.clone();
		for number in 2.. {
			let key = (destination.clone(), collision_key(&candidate));
			let held = self.pipeline.claims.get(&key).is_some_and(|holder| *holder != file);
			if !held && !self.pipeline.taken.contains(&key) {
				let path = destination.join(&candidate);
				let occupied = fs::symlink_metadata(self.destination_path(&path)).is_ok();
				if !occupied || replaces.as_ref() == Some(&path) {
					break;
				}

				self.pipeline.taken.insert(key);
			}

			candidate = numbered(&base, number);
		}

		destination.join(&candidate)
	}

	/// The mirrored folder: every name mapped to one the destination can hold, and never into the project's own
	/// folder.
	pub(super) fn destination_folder(&self, folder: FolderId) -> RelPath {
		self.tree
			.folder(folder)
			.key
			.components()
			.fold(RelPath::root(), |path, name| {
				let name = destination_name(&name);
				let name = if path.is_root() {
					outside_the_work_area(name)
				} else {
					name
				};
				path.join(&name)
			})
	}

	pub(super) fn claimed_by_other(&self, file: FileId, path: &RelPath) -> bool {
		self.pipeline
			.claims
			.get(&name_key(path))
			.is_some_and(|holder| *holder != file)
	}

	pub(super) fn claim(&mut self, file: FileId, path: &RelPath) {
		self.pipeline.claims.insert(name_key(path), file);
	}

	pub(super) fn release_claim(&mut self, file: FileId, path: &RelPath) {
		let key = name_key(path);
		if self.pipeline.claims.get(&key) == Some(&file) {
			self.pipeline.claims.remove(&key);
		}
	}

	/// An intent that never placed its output gives its name back, unless the file's own earlier output is there.
	pub(super) fn release_intent_claim(&mut self, file: FileId, path: &RelPath) {
		if self.tree.file(file).record.replaces.as_ref() != Some(path) {
			self.release_claim(file, path);
		}
	}

	pub(super) fn release_all_claims(&mut self, file: FileId) {
		self.pipeline.claims.retain(|_, holder| *holder != file);
	}
}

pub(super) fn kept_note(reason: &str) -> String {
	format!("kept the original: {reason}")
}

fn name_key(path: &RelPath) -> (RelPath, String) {
	(path.parent().unwrap_or_else(RelPath::root), collision_key(&path.name()))
}

fn issue_kind(kind: &ErrorKind) -> IssueKind {
	match kind {
		ErrorKind::SourceDamaged(_) => IssueKind::Unrecoverable { damaged: true },
		ErrorKind::SourceUnreadable(_) | ErrorKind::PermissionDenied(_) | ErrorKind::Locked => {
			IssueKind::SourceUnreadable
		}
		_ => IssueKind::Unrecoverable { damaged: false },
	}
}

/// A name at the top of the destination that is the project's own folder, in any case, gets a name of its own: that
/// folder is the project's work area, swept on every open.
fn outside_the_work_area(name: OsString) -> OsString {
	if collision_key(&name) != collision_key(OsStr::new(ARCHIVE_DIR)) {
		return name;
	}

	let mut own = name;
	own.push(" (source)");
	own
}

fn numbered(name: &OsString, number: u32) -> OsString {
	let path = Path::new(name);
	let stem = path.file_stem().unwrap_or(name).to_string_lossy();
	let numbered = match path.extension() {
		Some(extension) => format!("{stem} ({number}).{}", extension.to_string_lossy()),
		None => format!("{stem} ({number})"),
	};
	destination_name(OsStr::new(&numbered))
}

#[cfg(test)]
pub(super) mod tests {
	use std::path::PathBuf;
	use std::sync::mpsc::Receiver;
	use std::time::Duration;

	use cryarchiver_engine::preset::Overrides;
	use tokio::sync::mpsc::UnboundedReceiver;
	use tokio::sync::oneshot;
	use tokio::time::MissedTickBehavior;

	use super::*;
	use crate::issues::{IssueKey, Verb};
	use crate::project::scanning::tests::{key, list, owner};
	use crate::record::{Hash, Identity};
	use crate::view::Update;

	struct Fixture {
		_dir: tempfile::TempDir,
		owner: Owner,
		_updates: Receiver<Update>,
		name: &'static str,
		file: FileId,
	}

	impl Fixture {
		fn new(content: &[u8]) -> Self {
			Self::named("notes.txt", content)
		}

		fn named(name: &'static str, content: &[u8]) -> Self {
			let dir = tempfile::tempdir().unwrap();
			let source = dir.path().join("source");
			fs::create_dir_all(&source).unwrap();
			fs::write(source.join(name), content).unwrap();

			let (mut owner, updates) = owner(&source, &dir.path().join("archive"));
			list(&mut owner, &RelPath::root(), true);
			let file = owner.tree.file_id(&key(name)).unwrap();
			let mut fixture = Self {
				_dir: dir,
				owner,
				_updates: updates,
				name,
				file,
			};
			fixture.queue(Some(content.to_vec()));
			fixture
		}

		fn source(&self) -> PathBuf {
			self.owner.settings.source.join(self.name)
		}

		fn destination(&self, name: &str) -> PathBuf {
			self.owner.settings.destination.join(name)
		}

		fn change_source(&mut self, content: &[u8]) {
			fs::write(self.source(), content).unwrap();
			list(&mut self.owner, &RelPath::root(), false);
		}

		fn remove_source(&mut self) {
			fs::remove_file(self.source()).unwrap();
			list(&mut self.owner, &RelPath::root(), false);
		}

		fn queue(&mut self, content: Option<Vec<u8>>) {
			self.owner.on_ready_checked(
				self.file,
				ReadyCheck::Ready {
					class: MediaClass::Other,
					content,
				},
			);
		}

		/// What `start_job` does, without spawning the job.
		fn running(&mut self) -> CancellationToken {
			for queue in self.owner.pipeline.queues.values_mut() {
				queue.remove(self.file);
			}

			self.owner.pipeline.governor.started(Work::Copy, Instant::now());
			let job = running_job(Work::Copy);
			let cancel = job.cancel.clone();
			self.owner.pipeline.jobs.insert(self.file, job);
			self.owner.set_state(self.file, FileState::InProgress { attempt: 1 });
			cancel
		}

		fn commit(&mut self, content: &[u8]) -> Option<Target> {
			self.commit_as(content, SourceIdentity::default())
		}

		fn commit_as(&mut self, content: &[u8], identity: SourceIdentity) -> Option<Target> {
			let (reply, answer) = oneshot::channel();
			let staged = Staged {
				path: PathBuf::from("staged"),
				size: content.len() as u64,
				hash: Hash(*blake3::hash(content).as_bytes()),
			};

			self.owner.on_job(
				self.file,
				JobMessage::Commit {
					staged,
					made: Made::Copied(CopyReason::NotMedia),
					identity,
					reply,
				},
			);
			answer.blocking_recv().unwrap()
		}

		fn job(&mut self, message: JobMessage) {
			self.owner.on_job(self.file, message);
		}

		fn claims(&self) -> usize {
			self.owner
				.pipeline
				.claims
				.values()
				.filter(|holder| **holder == self.file)
				.count()
		}

		fn record(&self) -> &FileRecord {
			&self.owner.tree.file(self.file).record
		}
	}

	/// A first attempt's job record, as `start_job` makes it.
	pub(crate) fn running_job(work: Work) -> RunningJob {
		RunningJob {
			work,
			attempt: 1,
			cancel: CancellationToken::new(),
			fraction: 0.0,
			stage: None,
			video_waiting: false,
			requeue: false,
			not_same_file: false,
			pending: None,
			moved_from: None,
		}
	}

	/// A job whose commit intent named `output`, before its rename was confirmed.
	pub(crate) fn committing_job(output: Output) -> RunningJob {
		RunningJob {
			pending: Some(output),
			..running_job(Work::Placeholder)
		}
	}

	/// Runs the owner as its task does, on messages and ticks, until `done` holds or `limit` passes; `seen` sees each
	/// message before the owner does. Returns whether `done` held.
	pub(crate) fn drive(
		owner: &mut Owner,
		inbox: &mut UnboundedReceiver<Message>,
		limit: Duration,
		mut seen: impl FnMut(&Message),
		mut done: impl FnMut(&Owner) -> bool,
	) -> bool {
		tokio::runtime::Handle::current().block_on(async {
			let deadline = tokio::time::Instant::now() + limit;
			let mut ticker = tokio::time::interval(owner.tuning.tick);
			ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

			while !done(owner) {
				tokio::select! {
					Some(message) = inbox.recv() => {
						seen(&message);
						owner.message(message);
					}
					_ = ticker.tick() => owner.tick(Instant::now()),
					() = tokio::time::sleep_until(deadline) => return false,
				}
			}
			true
		})
	}

	/// Starts like a PDF, so the ready check copies it as it is.
	pub(crate) fn not_media(len: usize) -> Vec<u8> {
		let mut bytes = b"%PDF-1.4\n".to_vec();
		bytes.resize(len, b'x');
		bytes
	}

	fn place(target: &Target, content: &[u8]) {
		fs::create_dir_all(target.path.parent().unwrap()).unwrap();
		fs::write(&target.path, content).unwrap();
	}

	fn cancelled() -> JobMessage {
		JobMessage::Finished(Err(ErrorKind::Cancelled.into()))
	}

	fn failed(kind: ErrorKind) -> JobMessage {
		JobMessage::Finished(Err(kind.into()))
	}

	#[test]
	fn a_job_built_after_an_overrides_change_carries_the_new_overrides() {
		let mut fixture = Fixture::named("song.flac", b"fLaC");
		let overrides_of = |owner: &mut Owner, file: FileId| match owner.task_for(file, Work::Encode) {
			Some(Task::Encode(job)) => job.overrides,
			_ => panic!("an encode task was expected"),
		};
		let custom = Overrides {
			image: Some(80.0),
			audio_stereo_kbps: Some(160),
			..Overrides::default()
		};

		let before = overrides_of(&mut fixture.owner, fixture.file);
		fixture
			.owner
			.change_settings(&crate::settings::SettingsChange::Overrides(custom));
		let after = overrides_of(&mut fixture.owner, fixture.file);

		assert_eq!(before, Overrides::default());
		assert_eq!(after, custom);
		assert_eq!(fixture.owner.settings.overrides, custom);
	}

	#[test]
	fn nothing_from_the_source_is_ever_mirrored_into_the_work_area() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		for folder in [".cryarchiver", ".cryarchiver2", "Photos/.cryarchiver"] {
			fs::create_dir_all(source.join(folder)).unwrap();
		}
		fs::write(source.join(".cryarchive"), b"a file").unwrap();
		let (mut owner, _updates) = owner(&source, &dir.path().join("archive"));
		list(&mut owner, &RelPath::root(), true);
		crate::project::scanning::tests::list_queued(&mut owner);

		let mirrored = |folder: &str| {
			let folder = owner.tree.folder_id(&key(folder)).unwrap();
			owner.destination_folder(folder).display()
		};
		assert_eq!(mirrored(".cryarchiver"), ".cryarchiver (source)");
		assert_eq!(mirrored(".cryarchiver2"), ".cryarchiver2");
		assert_eq!(mirrored("Photos/.cryarchiver"), "Photos/.cryarchiver");
		assert_eq!(outside_the_work_area(".CryArchiver".into()), ".CryArchiver (source)");
		assert_eq!(outside_the_work_area(".cryarchive".into()), ".cryarchive");
	}

	#[test]
	fn a_source_change_before_taken_never_costs_the_unknown_file_its_place() {
		let mut fixture = Fixture::new(b"first");
		fixture.running();
		let first = fixture.commit(b"first").unwrap();
		assert_eq!(first.path, fixture.destination("notes.txt"));

		fs::write(&first.path, b"someone else's").unwrap();
		fixture.change_source(b"second version");
		fixture.job(JobMessage::Taken);
		fixture.job(cancelled());
		assert_eq!(fixture.record().replaces, None);

		fixture.queue(None);
		fixture.running();
		let second = fixture.commit(b"second version").unwrap();
		assert_eq!(second.path, fixture.destination("notes (2).txt"));
		assert_eq!(second.replaces, None);
		place(&second, b"second version");
		fixture.job(JobMessage::Placed);

		assert_eq!(fs::read(&first.path).unwrap(), b"someone else's");
		assert!(matches!(fixture.record().state, FileState::Done(_)));
	}

	#[test]
	fn an_output_placed_while_its_source_changed_is_replaced_by_the_re_archive() {
		let mut fixture = Fixture::new(b"first");
		fixture.running();
		let first = fixture.commit(b"first").unwrap();

		fixture.change_source(b"second version");
		place(&first, b"first");
		fixture.job(JobMessage::Placed);
		fixture.job(JobMessage::Finished(Ok(())));
		assert_eq!(fixture.record().replaces, Some(key("notes.txt")));
		assert_eq!(fixture.record().state, FileState::Discovered);

		fixture.queue(None);
		fixture.running();
		let again = fixture.commit(b"second version").unwrap();
		assert_eq!(again.path, fixture.destination("notes.txt"));
		assert_eq!(again.replaces, Some(fixture.destination("notes.txt")));
	}

	#[test]
	fn a_file_ready_again_starts_only_once_its_earlier_job_finished() {
		let runtime = tokio::runtime::Runtime::new().unwrap();
		let _entered = runtime.enter();
		let mut fixture = Fixture::new(b"first");
		let mut inbox = fixture.owner.inbox.take().unwrap();
		fixture.owner.started = true;
		fixture.owner.paused = false;

		let earlier = fixture.running();
		fixture.change_source(b"second version");
		assert!(earlier.is_cancelled());
		fixture.queue(None);
		fixture.owner.start_jobs(Instant::now());
		assert_eq!(fixture.owner.pipeline.governor.total(), 1);
		assert_eq!(fixture.record().state, FileState::Queued);

		fixture.job(cancelled());
		assert_eq!(fixture.owner.pipeline.governor.total(), 0);
		fixture.owner.start_jobs(Instant::now());
		assert_eq!(fixture.owner.pipeline.governor.total(), 1);
		while !fixture.owner.pipeline.jobs.is_empty() {
			let message = runtime
				.block_on(tokio::time::timeout(Duration::from_secs(20), inbox.recv()))
				.unwrap()
				.unwrap();
			if let Message::Job(file, message) = message {
				fixture.owner.on_job(file, message);
			}
		}

		assert_eq!(fixture.owner.pipeline.governor.total(), 0);
		assert!(matches!(fixture.record().state, FileState::Done(_)));
		assert_eq!(fs::read(fixture.destination("notes.txt")).unwrap(), b"second version");
	}

	#[cfg(windows)]
	#[test]
	fn a_file_another_program_holds_open_is_checked_at_most_once_a_tick_and_archived_once_released() {
		use std::os::windows::fs::OpenOptionsExt;

		const HELD: Duration = Duration::from_secs(1);

		let runtime = tokio::runtime::Runtime::new().unwrap();
		let _entered = runtime.enter();
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		fs::create_dir_all(&source).unwrap();
		let path = source.join("mailbox.pdf");
		fs::write(&path, not_media(1000)).unwrap();
		let held = fs::File::options()
			.read(true)
			.write(true)
			.share_mode(0)
			.open(&path)
			.unwrap();
		held.set_modified(std::time::SystemTime::now() - Duration::from_hours(1))
			.unwrap();

		let (mut owner, _updates) = owner(&source, &dir.path().join("archive"));
		let mut inbox = owner.inbox.take().unwrap();
		list(&mut owner, &RelPath::root(), true);
		let file = owner.tree.file_id(&key("mailbox.pdf")).unwrap();
		owner.start(false);

		let mut checks = 0_u32;
		let until = Instant::now() + HELD;
		drive(
			&mut owner,
			&mut inbox,
			HELD * 2,
			|message| checks += u32::from(matches!(message, Message::ReadyChecked(..))),
			|_| Instant::now() >= until,
		);

		let ticks = u32::try_from(HELD.as_millis() / owner.tuning.tick.as_millis()).unwrap();
		assert!((1..=ticks + 1).contains(&checks), "{checks} ready checks in {HELD:?}");
		assert_eq!(owner.tree.file(file).record.state, FileState::Discovered);

		drop(held);
		let archived = drive(
			&mut owner,
			&mut inbox,
			Duration::from_secs(20),
			|_| {},
			|owner| matches!(owner.tree.file(file).record.state, FileState::Done(_)),
		);

		assert!(archived, "{:?}", owner.tree.file(file).record.state);
		assert_eq!(
			fs::read(owner.settings.destination.join("mailbox.pdf")).unwrap(),
			not_media(1000)
		);
	}

	#[test]
	fn a_cancelled_job_starts_again_only_on_the_next_tick() {
		let runtime = tokio::runtime::Runtime::new().unwrap();
		let _entered = runtime.enter();
		let mut fixture = Fixture::new(b"first");
		fixture.owner.start(false);
		fixture.running();

		fixture.owner.message(Message::Job(fixture.file, cancelled()));

		assert_eq!(fixture.record().state, FileState::Queued);
		assert!(fixture.owner.pipeline.jobs.is_empty(), "restarted back to back");

		fixture.owner.tick(Instant::now());

		assert!(fixture.owner.pipeline.jobs.contains_key(&fixture.file));
	}

	#[test]
	fn each_retry_after_a_crash_runs_on_half_the_threads() {
		let mut fixture = Fixture::named("song.flac", b"fLaC");
		let threads_of = |owner: &mut Owner, file: FileId| match owner.task_for(file, Work::Encode) {
			Some(Task::Encode(job)) => job.options.threads,
			_ => panic!("an encode task was expected"),
		};
		let crash = || {
			failed(ErrorKind::ToolCrashed(
				"ffmpeg crashed: out of memory (exit code -12)".into(),
			))
		};
		let cores = std::thread::available_parallelism().map_or(2, usize::from);
		let half = (cores / 2).max(1);

		let first = threads_of(&mut fixture.owner, fixture.file);
		fixture.running();
		fixture.job(crash());
		let second = threads_of(&mut fixture.owner, fixture.file);
		fixture.running();
		fixture.job(crash());
		let third = threads_of(&mut fixture.owner, fixture.file);
		fixture.running();
		fixture.job(crash());

		assert_eq!(first, None);
		assert_eq!(second, Some(half));
		assert_eq!(third, Some((half / 2).max(1)));
		assert!(matches!(fixture.record().state, FileState::Issue { .. }));
		assert_eq!(
			threads_of(&mut fixture.owner, fixture.file),
			None,
			"an issue starts afresh"
		);
	}

	#[test]
	fn a_retry_halves_the_engine_budget_the_crash_ran_on_and_says_so() {
		let mut fixture = Fixture::named("song.flac", b"fLaC");
		let crash = |ran_on: usize| {
			JobMessage::Finished(Err(JobError::from(ErrorKind::ToolCrashed(
				"ffmpeg crashed: out of memory (exit code -12)".into(),
			))
			.ran_on(ran_on)))
		};
		let threads_of = |owner: &mut Owner, file: FileId| match owner.task_for(file, Work::Encode) {
			Some(Task::Encode(job)) => job.options.threads,
			_ => panic!("an encode task was expected"),
		};

		fixture.running();
		fixture.job(crash(4));
		let second = threads_of(&mut fixture.owner, fixture.file);
		fixture.running();
		fixture.job(crash(2));
		let third = threads_of(&mut fixture.owner, fixture.file);

		assert_eq!(second, Some(2), "a capped image share of 4 threads");
		assert_eq!(third, Some(1));
		let notes = &fixture.owner.pipeline.notes[&fixture.file];
		assert!(
			notes[0].contains("; retrying in ") && notes[0].ends_with(" s with 2 threads"),
			"{notes:?}"
		);
		assert!(notes[1].ends_with(" with 1 thread"), "{notes:?}");
	}

	#[test]
	fn halved_threads_are_forgotten_once_the_file_is_done_or_withdrawn() {
		let crash = || failed(ErrorKind::ToolCrashed("ffmpeg crashed: exception 0xC0000005".into()));

		let mut done = Fixture::new(b"first");
		done.running();
		done.job(crash());
		assert!(done.owner.pipeline.retry_threads.contains_key(&done.file));
		done.running();
		let target = done.commit(b"first").unwrap();
		place(&target, b"first");
		done.job(JobMessage::Placed);
		assert!(matches!(done.record().state, FileState::Done(_)));
		assert!(!done.owner.pipeline.retry_threads.contains_key(&done.file));

		let mut withdrawn = Fixture::new(b"first");
		withdrawn.running();
		withdrawn.job(crash());
		assert!(withdrawn.owner.pipeline.retry_threads.contains_key(&withdrawn.file));
		withdrawn.change_source(b"second version");
		assert!(!withdrawn.owner.pipeline.retry_threads.contains_key(&withdrawn.file));
	}

	#[test]
	fn other_retried_errors_keep_the_threads() {
		let mut fixture = Fixture::named("song.flac", b"fLaC");

		fixture.running();
		fixture.job(failed(ErrorKind::DestinationIo("reset".into())));

		assert_eq!(fixture.record().state, FileState::Queued);
		assert!(matches!(
			fixture.owner.task_for(fixture.file, Work::Encode),
			Some(Task::Encode(job)) if job.options.threads.is_none()
		));
	}

	#[test]
	fn a_changed_source_starts_its_retries_afresh() {
		let mut fixture = Fixture::new(b"first");
		fixture.owner.pipeline.retries.insert(fixture.file, 2);
		fixture
			.owner
			.pipeline
			.retry_at
			.insert((Instant::now() + Duration::from_mins(1), fixture.file));

		fixture.change_source(b"second version");

		assert!(!fixture.owner.pipeline.retries.contains_key(&fixture.file));
		assert!(fixture.owner.pipeline.retry_at.is_empty());
	}

	#[test]
	fn abandoned_intents_give_their_names_back() {
		let mut fixture = Fixture::new(b"first");

		fixture.running();
		fixture.commit(b"first").unwrap();
		assert_eq!(fixture.claims(), 1);
		fixture.job(failed(ErrorKind::DestinationIo("gone".into())));
		assert_eq!(fixture.record().state, FileState::Queued);
		assert_eq!(fixture.claims(), 0, "a failed rename");

		fixture.running();
		fixture.commit(b"first").unwrap();
		fixture.job(failed(ErrorKind::EncodeFailed("broken".into())));
		assert!(matches!(fixture.record().state, FileState::Issue { .. }));
		assert_eq!(fixture.claims(), 0, "an issue");

		let cancel = fixture.running();
		fixture.commit(b"first").unwrap();
		cancel.cancel();
		fixture.job(JobMessage::Taken);
		assert!(fixture.commit(b"first").is_none());
		fixture.job(cancelled());
		assert_eq!(fixture.claims(), 0, "a cancel between intents");

		fixture.running();
		fixture.commit(b"first").unwrap();
		fixture.remove_source();
		assert!(fixture.owner.tree.file(fixture.file).removed);
		assert_eq!(fixture.claims(), 0, "a forgotten file");
	}

	#[test]
	fn an_archived_reference_file_forgets_what_it_named() {
		const PLAYLIST: &[u8] = b"#EXTM3U\na.flac\n";

		let mut fixture = Fixture::named("mix.m3u", PLAYLIST);
		assert!(fixture.owner.is_reference(fixture.file));

		fixture.running();
		let target = fixture.commit(PLAYLIST).unwrap();
		place(&target, PLAYLIST);
		fixture.job(JobMessage::Placed);

		assert!(matches!(fixture.record().state, FileState::Done(_)));
		assert!(!fixture.owner.is_reference(fixture.file));
	}

	#[test]
	fn a_source_whose_identity_changes_every_time_it_is_read_becomes_an_issue() {
		let mut fixture = Fixture::new(b"first");
		let changing = SourceIdentity {
			at_start: Some(Identity { volume: 1, index: 1 }),
			at_commit: Some(Identity { volume: 1, index: 2 }),
		};

		for _ in 0..IDENTITY_CHANGES {
			fixture.running();
			assert!(fixture.commit_as(b"first", changing).is_none());
			assert_eq!(fixture.record().state, FileState::Discovered);
			fixture.job(cancelled());
			fixture.queue(None);
		}

		fixture.running();
		assert!(fixture.commit_as(b"first", changing).is_none());
		assert!(matches!(
			&fixture.record().state,
			FileState::Issue { kind: IssueKind::SourceUnreadable, reason, .. } if reason == IDENTITY_UNSTABLE
		));
		assert!(!fixture.owner.pipeline.observed.contains_key(&fixture.file));
	}

	#[test]
	fn a_retry_lets_a_source_whose_identity_kept_changing_be_archived_again() {
		let mut fixture = Fixture::new(b"first");
		let changing = SourceIdentity {
			at_start: Some(Identity { volume: 1, index: 1 }),
			at_commit: Some(Identity { volume: 1, index: 2 }),
		};
		for _ in 0..=IDENTITY_CHANGES {
			fixture.running();
			assert!(fixture.commit_as(b"first", changing).is_none());
			fixture.job(cancelled());
			fixture.queue(None);
		}
		assert!(matches!(fixture.record().state, FileState::Issue { .. }));

		let issue = fixture.owner.issue_ids.id(&IssueKey::File(fixture.file));
		fixture.owner.resolve(issue, Verb::Retry);
		fixture.queue(None);
		fixture.running();
		assert!(fixture.commit_as(b"first", changing).is_none());

		assert_eq!(fixture.record().state, FileState::Discovered);
	}

	#[test]
	fn an_output_replaced_by_one_placed_after_a_source_change_goes_only_once_that_is_recorded() {
		let mut fixture = Fixture::new(b"first");
		let earlier = fixture.destination("notes old.txt");
		fs::create_dir_all(earlier.parent().unwrap()).unwrap();
		fs::write(&earlier, b"earlier").unwrap();
		fixture.running();
		let target = fixture.commit(b"first").unwrap();
		let mut record = fixture.record().clone();
		record.replaces = Some(key("notes old.txt"));
		fixture.owner.tree.set_record(fixture.file, record);

		fixture.change_source(b"second version");
		place(&target, b"first");
		fixture.job(JobMessage::Placed);

		assert_eq!(fixture.record().replaces, Some(key("notes.txt")));
		assert!(earlier.exists());

		fixture.owner.store.flush().unwrap();
		fixture.owner.after_flush();
		assert!(!earlier.exists());
		assert_eq!(fs::read(&target.path).unwrap(), b"first");
	}

	#[test]
	fn an_output_placed_after_its_source_changed_and_vanished_is_never_passed_off_as_the_new_content() {
		let mut fixture = Fixture::new(b"first");
		fixture.running();
		let target = fixture.commit(b"first").unwrap();

		fixture.change_source(b"second version");
		fixture.remove_source();
		place(&target, b"first");
		fixture.job(JobMessage::Placed);
		fixture.job(JobMessage::Finished(Ok(())));

		assert!(fixture.owner.tree.file(fixture.file).removed);
		assert_eq!(fixture.claims(), 0);
		assert_eq!(fs::read(&target.path).unwrap(), b"first");
	}

	#[test]
	fn an_output_placed_after_its_source_vanished_stays_tracked() {
		let mut fixture = Fixture::new(b"first");
		fixture.running();
		let target = fixture.commit(b"first").unwrap();

		fixture.remove_source();
		place(&target, b"first");
		fixture.job(JobMessage::Placed);
		fixture.job(JobMessage::Finished(Ok(())));

		assert!(!fixture.owner.tree.file(fixture.file).removed);
		let FileState::SourceGone(output) = &fixture.record().state else {
			panic!("not kept: {:?}", fixture.record().state)
		};
		assert_eq!(output.path, key("notes.txt"));
		assert_eq!(fixture.claims(), 1);
	}
}
