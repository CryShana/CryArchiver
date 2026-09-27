use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use cryarchiver_engine::commit::free_space;
use cryarchiver_engine::text::display_name;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::MissedTickBehavior;

use super::dangerous::DangerState;
use super::dependencies::ReferenceState;
use super::duplicates::{DedupState, HashOutcome, HashRequest};
use super::finish::FinishState;
use super::jobs::JobMessage;
use super::live::LiveState;
use super::pipeline::Pipeline;
use super::publish::ViewState;
use super::scanning::ScanState;
use super::watching::WatchState;
use super::{Backend, Command, Fault, SystemSample, Tuning};
use crate::issues::IssueIds;
use crate::keys::RelPath;
use crate::record::{FileState, Identity};
use crate::references;
use crate::scan::{DestinationFile, Listing};
use crate::settings::Settings;
use crate::store::{Store, StoreError};
use crate::tree::{FileId, FolderId, Tree};
use crate::view::{LogKind, NoticeLevel, SystemView, Update};
use crate::watch::{Hint, ReadyCheck};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) enum Input {
	Command(Command),
	Shutdown,
}

/// What the project task's own helpers send back to it.
pub(crate) enum Message {
	Listed(Listing, bool),
	ReadyChecked(FileId, ReadyCheck),
	Job(FileId, JobMessage),
	Hashed(FileId, HashRequest, HashOutcome),
	FolderFinished(FolderId, Result<(), String>),
	DestinationWalked(u64, Vec<DestinationFile>),
	Hint(Hint),
	FoldersChanged(Vec<RelPath>),
	DestinationListed(Vec<DestinationFile>),
}

#[allow(clippy::struct_excessive_bools)]
pub(crate) struct Owner {
	pub(super) project_file: PathBuf,
	pub(super) settings: Settings,
	pub(super) store: Store,
	pub(super) tree: Tree,
	pub(super) backend: Arc<dyn Backend>,
	pub(super) tuning: Tuning,
	pub(super) updates: Sender<Update>,
	pub(super) messages: UnboundedSender<Message>,
	pub(super) inbox: Option<UnboundedReceiver<Message>>,
	pub(super) destination_identity: Option<Identity>,
	/// The folders that hold the source root: a link to one of them leads back into the source.
	pub(super) enclosing_identities: Vec<Identity>,
	pub(super) started: bool,
	pub(super) paused: bool,
	pub(super) stopping: bool,
	pub(super) delete_confirmed: bool,
	pub(super) fault: Option<Fault>,
	pub(super) crashed: bool,
	pub(super) scan: ScanState,
	pub(super) pipeline: Pipeline,
	pub(super) dedup: DedupState,
	pub(super) references: ReferenceState,
	pub(super) finish: FinishState,
	pub(super) watching: WatchState,
	pub(super) danger: DangerState,
	pub(super) live: LiveState,
	pub(super) view: ViewState,
	pub(super) issue_ids: IssueIds,
	pub(super) issue_files: BTreeSet<FileId>,
	pub(super) last_attempts: HashMap<RelPath, u32>,
	pub(super) sample: Option<(Instant, SystemSample)>,
	pub(super) failure: Option<StoreError>,
}

impl Owner {
	pub(crate) async fn run(mut self, mut inputs: UnboundedReceiver<Input>) {
		let mut inbox = self.inbox.take().expect("the task runs once");
		self.begin();

		let mut ticker = tokio::time::interval(self.tuning.tick);
		ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
		loop {
			tokio::select! {
				input = inputs.recv() => match input {
					Some(Input::Command(command)) => self.command(command),
					Some(Input::Shutdown) | None => break,
				},
				Some(message) = inbox.recv() => self.message(message),
				_ = ticker.tick() => self.tick(Instant::now()),
			}

			if self.crashed {
				break;
			}

			self.settle(Instant::now());
			if self.failure.is_some() {
				break;
			}
		}

		self.finish(&mut inbox).await;
	}

	fn begin(&mut self) {
		self.send(Update::Settings(self.settings.clone()));

		for id in self.tree.folder_ids().skip(1) {
			let folder = self.tree.folder(id);
			self.send(Update::FolderAdded {
				id,
				parent: folder.parent.unwrap_or(FolderId::ROOT),
				name: display_name(&folder.name),
			});
		}

		for id in self.tree.folder_ids() {
			self.touch_folder(id);
		}
		self.view.issues_dirty = true;
		self.publish();

		self.resume_queued();
		self.start_watching();
		self.start_scan();
		self.reconcile_destination();
	}

	/// Files the last run left waiting go back into the pipeline. A queued one is in no queue yet, and a dedup group
	/// queues only a ready unique member, so it is ready again first.
	fn resume_queued(&mut self) {
		let files: Vec<FileId> = self.tree.file_ids().collect();
		for file in files {
			if self.tree.file(file).removed {
				continue;
			}

			let reference = references::kind_of(&self.tree.file(file).name).is_some();
			match self.tree.file(file).record.state {
				FileState::Discovered => self.observe(file),
				FileState::Ready | FileState::Queued if reference => {
					self.set_state(file, FileState::Discovered);
					self.observe(file);
				}
				FileState::Queued => {
					self.set_state(file, FileState::Ready);
					self.gate(file);
				}
				FileState::Ready => self.gate(file),
				FileState::Done(_)
				| FileState::SourceGone(_)
				| FileState::DeletePending(_)
				| FileState::OriginalDeleted(_)
				| FileState::Issue { .. } => self.join_dedup(file),
				_ => {}
			}
		}

		self.plan_dedup_groups();
	}

	fn command(&mut self, command: Command) {
		match command {
			Command::Start { delete_confirmed } => self.start(delete_confirmed),
			Command::Pause => self.paused = true,
			Command::StopNow => {
				self.paused = true;
				let running: Vec<FileId> = self.pipeline.jobs.keys().copied().collect();
				self.cancel_and_requeue(&running);
			}
			Command::Rescan => {
				self.start_scan();
				self.reconcile_destination();
			}
			Command::ChangeSettings(change) => self.change_settings(&change),
			Command::Watch(folder, watched) => self.watch_folder(folder, watched),
			Command::Filter(query) => self.set_filter(query),
			Command::Details(file) => self.send_details(file),
			Command::IssueLog(issue, attempt) => self.send_issue_log(issue, attempt),
			Command::Resolve(issue, verb) => self.resolve(issue, verb),
			Command::RetryFailedIn(folder) => self.retry_failed_in(folder),
			Command::RestartPendingJobs => self.restart_pending_jobs(),
		}

		self.view.status_dirty = true;
	}

	fn message(&mut self, message: Message) {
		match message {
			Message::Listed(listing, recursive) => self.on_listed(&listing, recursive),
			Message::ReadyChecked(file, check) => self.on_ready_checked(file, check),
			Message::Job(file, message) => self.on_job(file, message),
			Message::Hashed(file, request, hash) => self.on_hashed(file, request, hash),
			Message::FolderFinished(folder, result) => self.on_folder_finished(folder, result),
			Message::DestinationWalked(walk, files) => self.on_destination_walked(walk, &files),
			Message::Hint(hint) => self.on_hint(hint),
			Message::FoldersChanged(folders) => self.on_folders_changed(folders),
			Message::DestinationListed(files) => self.on_destination_listed(&files),
		}
	}

	fn tick(&mut self, now: Instant) {
		self.relist_hinted(now);
		self.safety_pass(now);
		self.pump_listings();
		self.sample_system(now);
		self.check_discovered(now);
		self.retry_due(now);
		self.retry_deletes(now);
		self.check_disk(now);
		self.check_space_estimate(now);
		self.plan_dirty_groups();
		self.start_jobs(now);
		self.finish_folders();
		self.check_completion();
	}

	fn sample_system(&mut self, now: Instant) {
		if self
			.sample
			.is_some_and(|(at, _)| now.saturating_duration_since(at) < SAMPLE_INTERVAL)
		{
			return;
		}

		let sample = self.backend.sample();
		self.sample = Some((now, sample));
		self.pipeline.governor.observe_cpu(now, sample.cpu_busy);
		self.pipeline
			.memory
			.observe(sample.memory_available, sample.memory_total);
		self.send(Update::System(SystemView {
			cpu_percent: percent(sample.cpu_busy),
			ram_used: sample.memory_total.saturating_sub(sample.memory_available),
			ram_total: sample.memory_total,
			disk_free: free_space(&self.settings.destination).unwrap_or(0),
			low_priority: self.settings.performance_limit.is_some(),
			toolchain: self.backend.toolchain(),
		}));
	}

	fn settle(&mut self, now: Instant) {
		match self.store.flush_if_due(now) {
			Ok(_) => self.after_flush(),
			Err(err) => self.fail(err),
		}
		self.publish_if_due(now);
	}

	/// Stop now: running jobs are cancelled and requeued, then everything is written down. After a simulated crash
	/// nothing more is written.
	async fn finish(&mut self, inbox: &mut UnboundedReceiver<Message>) {
		self.stopping = true;
		self.view.status_dirty = true;
		let running: Vec<FileId> = self.pipeline.jobs.keys().copied().collect();
		self.cancel_and_requeue(&running);
		if self.crashed {
			self.send(Update::Stopped);
			return;
		}

		self.publish();
		let deadline = tokio::time::Instant::now() + STOP_TIMEOUT;
		while !self.pipeline.jobs.is_empty() {
			match tokio::time::timeout_at(deadline, inbox.recv()).await {
				Ok(Some(message)) => self.message(message),
				Ok(None) | Err(_) => break,
			}
		}

		match self.store.flush() {
			Ok(()) => self.after_flush(),
			Err(err) => self.fail(err),
		}
		let _ = std::fs::remove_dir(self.pipeline.sink.staged_dir());
		let _ = std::fs::remove_dir(self.pipeline.sink.archive_work_root());
		self.publish();

		if let Some(err) = self.failure.take() {
			self.notice(NoticeLevel::Error, format!("the project stopped: {err}"));
		}
		self.send(Update::Stopped);
	}

	pub(super) fn start(&mut self, delete_confirmed: bool) {
		if self.settings.delete_originals && !delete_confirmed {
			self.notice(
				NoticeLevel::Error,
				"this project deletes originals: confirm it to start (run with --delete-originals)".into(),
			);
			return;
		}

		self.started = true;
		self.paused = false;
		self.delete_confirmed = delete_confirmed && self.settings.delete_originals;
		if self.deleting() {
			self.log(LogKind::Action, "deleting originals is on".into());
			self.queue_all_deletes();
		}
	}

	pub(super) fn set_state(&mut self, file: FileId, state: FileState) {
		self.tree.set_state(file, state);
		self.note_placeholder(file);
		self.track_issue(file);
		self.save_file(file);
		self.touch_file(file);
	}

	pub(super) fn track_issue(&mut self, file: FileId) {
		let open =
			matches!(self.tree.file(file).record.state, FileState::Issue { .. }) && !self.tree.file(file).removed;
		let changed = if open {
			self.issue_files.insert(file)
		} else {
			self.issue_files.remove(&file)
		};
		self.view.issues_dirty |= changed || open;
	}

	pub(super) fn save_file(&mut self, file: FileId) {
		let entry = self.tree.file(file);
		let result = if entry.removed {
			self.store.remove_file(&entry.key);
			Ok(())
		} else {
			self.store.set_file(&entry.key, &entry.record)
		};
		if let Err(err) = result {
			self.fail(err);
		}
	}

	pub(super) fn save_folder(&mut self, folder: FolderId) {
		let entry = self.tree.folder(folder);
		if let Err(err) = self.store.set_folder(&entry.key, &entry.record) {
			self.fail(err);
		}
	}

	pub(super) fn fail(&mut self, err: StoreError) {
		self.failure.get_or_insert(err);
	}

	/// Whether the test's crash point is here; the task then stops without writing anything more.
	pub(super) fn crash_point(&mut self, fault: Fault) -> bool {
		if self.fault == Some(fault) {
			self.crashed = true;
		}
		self.crashed
	}

	pub(super) fn source_path(&self, key: &RelPath) -> PathBuf {
		key.to_path(&self.settings.source)
	}

	pub(super) fn destination_path(&self, key: &RelPath) -> PathBuf {
		key.to_path(&self.settings.destination)
	}

	/// Once a re-archive's new output is placed, removes the earlier output it replaces: only that exact recorded
	/// path inside the destination, never the new output itself. Blocking: one file removal.
	pub(super) fn remove_replaced_output(&mut self, previous: &RelPath, placed: &RelPath) {
		if previous != placed {
			self.remove_own_output(previous);
		}
	}

	/// Removes a file the project placed: only that exact recorded path inside the destination. Blocking: one file
	/// removal.
	pub(super) fn remove_own_output(&mut self, path: &RelPath) {
		let plain = path.components().all(|name| {
			let mut parts = Path::new(&name).components();
			matches!((parts.next(), parts.next()), (Some(Component::Normal(_)), None))
		});
		if path.is_root() || !plain {
			return;
		}

		self.output_vacated_during_walk(path);
		match fs::remove_file(self.destination_path(path)) {
			Err(err) if err.kind() != io::ErrorKind::NotFound => self.notice(
				NoticeLevel::Warning,
				format!("{} can't be removed: {err}", path.display()),
			),
			_ => {}
		}
	}
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn percent(share: f32) -> u8 {
	(share.clamp(0.0, 1.0) * 100.0).round() as u8
}
