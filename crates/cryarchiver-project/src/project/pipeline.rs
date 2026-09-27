//! A file's way through the run (project layer spec §5 and §8): the ready rule, the queues, the governor, the jobs,
//! the commit intent, retries and issues.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use cryarchiver_engine::commit::free_space;
use cryarchiver_engine::engine::{Encode, Job, JobEvent, Stage};
use cryarchiver_engine::naming::{collision_key, destination_name, placeholder_name};
use cryarchiver_engine::outcome::{ErrorClass, ErrorKind, JobError};
use cryarchiver_engine::sink::ArchivalSink;
use tokio_util::sync::CancellationToken;

use super::jobs::{JobContext, JobMessage, Made, Target, Task};
use super::owner::{Message, Owner};
use crate::keys::RelPath;
use crate::memory::MemoryBudget;
use crate::record::{CopyReason, FailureLog, FileState, How, IssueKind, MediaClass, Output};
use crate::references;
use crate::run::Staged;
use crate::schedule::{DiskGuard, Governor, GovernorConfig, Queue, Waits, Work, retry_delay};
use crate::tree::{FileId, FolderId};
use crate::view::{LogKind, NoticeLevel};
use crate::watch::{ReadyCheck, check_ready, is_stable};

const READY_CHECKS: usize = 8;
const START_ORDER: [Work; 5] = [Work::Placeholder, Work::Hash, Work::Delete, Work::Copy, Work::Encode];

pub(crate) struct RunningJob {
	pub(super) work: Work,
	pub(super) attempt: u32,
	pub(super) cancel: CancellationToken,
	pub(super) fraction: f64,
	pub(super) stage: Option<Stage>,
	pub(super) video_waiting: bool,
	pub(super) requeue: bool,
	pending: Option<Output>,
}

pub(crate) struct Pipeline {
	pub(super) observed: HashMap<FileId, Instant>,
	checking: HashSet<FileId>,
	pub(super) queues: HashMap<Work, Queue>,
	pub(super) waits: Waits,
	pub(super) retry_at: BTreeSet<(Instant, FileId)>,
	retries: HashMap<FileId, u32>,
	pub(super) jobs: HashMap<FileId, RunningJob>,
	pub(super) governor: Governor,
	pub(super) encoder: Arc<dyn Encode>,
	pub(super) memory: Arc<MemoryBudget>,
	pub(super) sink: ArchivalSink,
	pub(super) disk: DiskGuard,
	last_disk_check: Option<Instant>,
	pub(super) chosen_copies: HashSet<FileId>,
	pub(super) claims: HashMap<(FolderId, String), FileId>,
	taken: HashSet<(FolderId, String)>,
	pub(super) notes: HashMap<FileId, Vec<String>>,
	pub(super) workers_dirty: bool,
}

impl Pipeline {
	pub(crate) fn new(
		destination: &Path,
		config: GovernorConfig,
		encoder: Arc<dyn Encode>,
		memory: Arc<MemoryBudget>,
	) -> Self {
		Self {
			observed: HashMap::new(),
			checking: HashSet::new(),
			queues: START_ORDER.iter().map(|work| (*work, Queue::default())).collect(),
			waits: Waits::default(),
			retry_at: BTreeSet::new(),
			retries: HashMap::new(),
			jobs: HashMap::new(),
			governor: Governor::new(config),
			encoder,
			memory,
			sink: ArchivalSink::new(destination),
			disk: DiskGuard::default(),
			last_disk_check: None,
			chosen_copies: HashSet::new(),
			claims: HashMap::new(),
			taken: HashSet::new(),
			notes: HashMap::new(),
			workers_dirty: false,
		}
	}
}

impl Owner {
	/// Starts watching a discovered file for the ready rule.
	pub(super) fn observe(&mut self, file: FileId) {
		self.pipeline.observed.insert(file, Instant::now());
	}

	pub(super) fn check_discovered(&mut self, now: Instant) {
		let window = self.settings.stability_window;
		let candidates: Vec<FileId> = self
			.pipeline
			.observed
			.iter()
			.filter(|(file, since)| {
				let record = &self.tree.file(**file).record;
				!self.pipeline.checking.contains(file) && is_stable(record.mtime_ns, **since, now, window)
			})
			.map(|(file, _)| *file)
			.take(READY_CHECKS.saturating_sub(self.pipeline.checking.len()))
			.collect();

		for file in candidates {
			self.pipeline.checking.insert(file);
			let entry = self.tree.file(file);
			let path = self.source_path(&entry.key);
			let (size, mtime_ns) = (entry.record.size, entry.record.mtime_ns);
			let want_content = references::kind_of(&entry.name).is_some();
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
				let mut record = entry.record.clone();
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
			ReadyCheck::Busy => self.observe(file),
			ReadyCheck::Unreadable(reason) => {
				self.pipeline.observed.remove(&file);
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
				self.pipeline.observed.remove(&file);
				let mut record = entry.record.clone();
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

		for work in START_ORDER {
			while self.pipeline.governor.may_start(work, now) {
				let next = self.pipeline.queues.get_mut(&work).and_then(Queue::pop_front);
				let Some(file) = next else {
					break;
				};

				if work == Work::Hash {
					self.start_hash(file, now);
				} else {
					self.start_job(file, work, now);
				}
			}
		}
	}

	fn start_job(&mut self, file: FileId, work: Work, now: Instant) {
		let entry = self.tree.file(file);
		if entry.removed || entry.record.state != FileState::Queued {
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
			sink: self.pipeline.sink.clone(),
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
				pending: None,
			},
		);
		self.pipeline.workers_dirty = true;
		self.set_state(file, FileState::InProgress { attempt });
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
			})),
			Work::Copy if self.pipeline.chosen_copies.contains(&file) => Some(Task::Copy(CopyReason::Chosen)),
			Work::Copy => Some(self.reference_task(file).unwrap_or(Task::Copy(CopyReason::NotMedia))),
			Work::Placeholder => self.duplicate_task(file),
			Work::Hash | Work::Delete => None,
		}
	}

	pub(super) fn on_job(&mut self, file: FileId, message: JobMessage) {
		match message {
			JobMessage::Event(event) => self.on_job_event(file, event),
			JobMessage::Commit { staged, made, reply } => {
				let target = self.commit_intent(file, &staged, &made);
				let _ = reply.send(target);
			}
			JobMessage::Placed => self.placed(file),
			JobMessage::Taken => self.name_taken(file),
			JobMessage::Kept(reason) => self.note(file, format!("kept the original: {reason}")),
			JobMessage::Finished(result) => self.job_finished(file, result),
		}
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
		}

		self.pipeline.workers_dirty = true;
	}

	pub(super) fn note(&mut self, file: FileId, text: String) {
		let name = self.tree.file(file).key.display();
		self.log(LogKind::Action, format!("{name}: {text}"));
		self.pipeline.notes.entry(file).or_default().push(text);
	}

	/// Names the output and writes `Committing` durably; only then may the job rename it into place.
	fn commit_intent(&mut self, file: FileId, staged: &Staged, made: &Made) -> Option<Target> {
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

		let path = self.output_path(file, made);
		let (how, hash) = match made {
			Made::Encoded { .. } => (How::Encoded, Some(staged.hash)),
			Made::Copied(reason) => (How::Copied(*reason), Some(staged.hash)),
			Made::Placeholder { canonical } => (
				How::Placeholder {
					canonical: canonical.clone(),
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
		record.state = FileState::Committing {
			attempt,
			output: output.clone(),
		};
		if let Err(err) = self.store.write_intent(&entry.key, &record) {
			self.fail(err);
			return None;
		}

		let replaces = record
			.replaces
			.as_ref()
			.filter(|previous| **previous == path)
			.map(|previous| self.destination_path(previous));
		self.tree.set_record(file, record);
		self.touch_file(file);
		self.claim(file, &path);
		if self.crash_point(super::Fault::AfterCommitIntent) {
			return None;
		}

		if let Some(job) = self.pipeline.jobs.get_mut(&file) {
			job.pending = Some(output);
		}
		Some(Target {
			path: self.destination_path(&path),
			replaces,
		})
	}

	/// The output is in place. A file whose source changed or vanished meanwhile keeps the state that change gave it:
	/// a change already names this output as the one its re-archive replaces.
	fn placed(&mut self, file: FileId) {
		let Some(output) = self.pipeline.jobs.get_mut(&file).and_then(|job| job.pending.take()) else {
			return;
		};
		if self.crash_point(super::Fault::AfterPlaced) {
			return;
		}

		let entry = self.tree.file(file);
		if entry.removed || !matches!(entry.record.state, FileState::Committing { .. }) {
			return;
		}

		let previous = entry.record.replaces.clone();
		if let Some(previous) =
			previous.filter(|previous| *previous != output.path && !self.claimed_by_other(file, previous))
		{
			self.release_claim(file, &previous);
			self.remove_replaced_output(&previous, &output.path);
		}

		let mut record = self.tree.file(file).record.clone();
		record.replaces = None;
		self.tree.set_record(file, record);
		self.pipeline.retries.remove(&file);
		self.set_state(file, FileState::Done(output));
		self.file_settled(file);
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

		let folder = self.tree.file(file).folder;
		self.release_claim(file, &output.path);
		self.pipeline.taken.insert((folder, collision_key(&output.path.name())));
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

		let still_ours = matches!(
			self.tree.file(file).record.state,
			FileState::InProgress { .. } | FileState::Committing { .. }
		);
		let Err(err) = result else {
			if still_ours {
				self.requeue(file, job.work);
			} else {
				self.member_done(file);
			}
			return;
		};
		if !still_ours || self.tree.file(file).removed {
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
				format!("paused: low disk space ({free} bytes free, {need} needed)"),
			);
			self.requeue(file, job.work);
			return;
		}

		let retries = self.pipeline.retries.get(&file).copied().unwrap_or(0);
		if err.kind.class() == ErrorClass::AutoRetry
			&& let Some(delay) = retry_delay(self.tuning.retry_base, retries)
		{
			self.pipeline.retries.insert(file, retries + 1);
			self.note(file, format!("{}; retrying in {} s", err.kind, delay.as_secs()));
			self.set_state(file, FileState::Queued);
			self.pipeline.retry_at.insert((Instant::now() + delay, file));
			return;
		}

		self.raise_issue(file, job.attempt, &err);
	}

	fn requeue(&mut self, file: FileId, work: Work) {
		self.set_state(file, FileState::Queued);
		if let Some(queue) = self.pipeline.queues.get_mut(&work) {
			queue.push_front(file);
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

	/// The destination path for a file's output, avoiding names other files already hold.
	fn output_path(&self, file: FileId, made: &Made) -> RelPath {
		let entry = self.tree.file(file);
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
				.encoded_name(&entry.name, std::ffi::OsStr::new(extension)),
			Made::Copied(_) | Made::Moved { .. } => self.copied_output_name(file),
			Made::Placeholder { .. } => placeholder_name(&entry.name),
		};

		let mut candidate = base.clone();
		for number in 2.. {
			let key = (entry.folder, collision_key(&candidate));
			let held = self.pipeline.claims.get(&key).is_some_and(|holder| *holder != file);
			if !held && !self.pipeline.taken.contains(&key) {
				break;
			}

			candidate = numbered(&base, number);
		}

		self.destination_folder(entry.folder).join(&candidate)
	}

	/// The mirrored folder: every name mapped to one the destination can hold.
	pub(super) fn destination_folder(&self, folder: FolderId) -> RelPath {
		self.tree
			.folder(folder)
			.key
			.components()
			.fold(RelPath::root(), |path, name| path.join(&destination_name(&name)))
	}

	pub(super) fn claimed_by_other(&self, file: FileId, path: &RelPath) -> bool {
		let key = (self.tree.file(file).folder, collision_key(&path.name()));
		self.pipeline.claims.get(&key).is_some_and(|holder| *holder != file)
	}

	pub(super) fn claim(&mut self, file: FileId, path: &RelPath) {
		let folder = self.tree.file(file).folder;
		self.pipeline.claims.insert((folder, collision_key(&path.name())), file);
	}

	pub(super) fn release_claim(&mut self, file: FileId, path: &RelPath) {
		let folder = self.tree.file(file).folder;
		let key = (folder, collision_key(&path.name()));
		if self.pipeline.claims.get(&key) == Some(&file) {
			self.pipeline.claims.remove(&key);
		}
	}
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

fn numbered(name: &OsString, number: u32) -> OsString {
	let path = Path::new(name);
	let stem = path.file_stem().unwrap_or(name).to_string_lossy();
	let numbered = match path.extension() {
		Some(extension) => format!("{stem} ({number}).{}", extension.to_string_lossy()),
		None => format!("{stem} ({number})"),
	};
	destination_name(std::ffi::OsStr::new(&numbered))
}
