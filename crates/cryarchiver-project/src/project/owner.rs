use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use cryarchiver_engine::commit::free_space;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::MissedTickBehavior;

use super::publish::ViewState;
use super::scanning::ScanState;
use super::{Backend, Command, SystemSample, Tuning};
use crate::issues::IssueIds;
use crate::keys::RelPath;
use crate::record::{FileState, Identity};
use crate::scan::Listing;
use crate::settings::Settings;
use crate::store::{Store, StoreError};
use crate::tree::{FileId, FolderId, Tree};
use crate::view::{NoticeLevel, SystemView, Update};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

pub(crate) enum Input {
	Command(Command),
	Shutdown,
}

/// What the project task's own helpers send back to it.
pub(crate) enum Message {
	Listed(Listing, bool),
}

pub(crate) struct Owner {
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
	pub(super) delete_confirmed: bool,
	pub(super) scan: ScanState,
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

			self.settle(Instant::now());
			if self.failure.is_some() {
				break;
			}
		}

		self.finish();
	}

	fn begin(&mut self) {
		self.send(Update::Settings(self.settings.clone()));

		for id in self.tree.folder_ids().skip(1) {
			let folder = self.tree.folder(id);
			self.send(Update::FolderAdded {
				id,
				parent: folder.parent.unwrap_or(FolderId::ROOT),
				name: cryarchiver_engine::text::display_name(&folder.name),
			});
		}

		for id in self.tree.folder_ids() {
			self.touch_folder(id);
		}
		self.view.issues_dirty = true;
		self.publish();

		self.start_scan();
	}

	fn command(&mut self, command: Command) {
		match command {
			Command::Start { delete_confirmed } => self.start(delete_confirmed),
			Command::Pause | Command::StopNow => self.paused = true,
			Command::Rescan => self.start_scan(),
			Command::ChangeSettings(change) => self.change_settings(change),
			Command::Watch(folder, watched) => self.watch_folder(folder, watched),
			Command::Filter(query) => self.set_filter(query),
			Command::Details(file) => self.send_details(file),
			Command::IssueLog(issue, attempt) => self.send_issue_log(issue, attempt),
			Command::Resolve(..) | Command::RetryFailedIn(_) | Command::RestartPendingJobs => {}
		}

		self.view.status_dirty = true;
	}

	fn message(&mut self, message: Message) {
		match message {
			Message::Listed(listing, recursive) => self.on_listed(&listing, recursive),
		}
	}

	fn tick(&mut self, now: Instant) {
		self.pump_listings();
		self.sample_system(now);
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
		if let Err(err) = self.store.flush_if_due(now) {
			self.fail(err);
		}
		self.publish_if_due(now);
	}

	fn finish(&mut self) {
		if let Err(err) = self.store.flush() {
			self.fail(err);
		}
		self.publish();

		if let Some(err) = self.failure.take() {
			self.notice(NoticeLevel::Error, format!("the project stopped: {err}"));
		}
		self.send(Update::Stopped);
	}

	fn start(&mut self, delete_confirmed: bool) {
		if self.settings.delete_originals && !delete_confirmed {
			self.notice(
				NoticeLevel::Error,
				"this project deletes originals: confirm it to start (run with --delete-originals)".into(),
			);
			return;
		}

		self.started = true;
		self.paused = false;
		self.delete_confirmed = delete_confirmed;
	}

	fn change_settings(&mut self, change: crate::settings::SettingsChange) {
		match self.settings.apply(change) {
			Ok(()) => {
				if let Err(err) = self.store.set_settings(&self.settings) {
					self.fail(err);
				}
				self.send(Update::Settings(self.settings.clone()));
			}
			Err(err) => self.notice(NoticeLevel::Warning, err.to_string()),
		}
	}

	pub(super) fn set_state(&mut self, file: FileId, state: FileState) {
		self.tree.set_state(file, state);
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

	pub(super) fn source_path(&self, key: &RelPath) -> PathBuf {
		key.to_path(&self.settings.source)
	}

	pub(super) fn destination_path(&self, key: &RelPath) -> PathBuf {
		key.to_path(&self.settings.destination)
	}
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn percent(share: f32) -> u8 {
	(share.clamp(0.0, 1.0) * 100.0).round() as u8
}
