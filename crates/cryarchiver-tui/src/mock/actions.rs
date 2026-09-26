use super::{LOW_DISK_TICKS, MockSource};
use crate::format::{as_f64, round_to_u64};
use crate::source::{Action, SettingsChange, Target};
use crate::view::{FileId, FileState, IssueId, IssueKind, Limit, NodeId, NoticeLevel, RunState, Verb};

const CONFIRMATION: &str = "DELETE";

impl MockSource {
	pub(super) fn apply(&mut self, action: Action) {
		match action {
			Action::Start => self.resume(),
			Action::Pause => self.pause(),
			Action::StopNow => self.stop_now(),
			Action::Rescan => self.rescan(),
			Action::RestartPendingJobs => self.restart_pending_jobs(),
			Action::ChangeSettings(change) => self.change_settings(change),
			Action::Resolve(issue, verb) => self.resolve(issue, verb),
			Action::RetryFailedIn(folder) => self.retry_failed_in(folder),
			Action::Expanded(folder, open) => self.expanded(folder, open),
			Action::SetFilter(query) => self.set_filter(query),
			Action::OpenSourceFolder(target) => {
				let folder = self.source_folder(target);
				self.log_action(format!("opened {folder}"));
			}
			Action::OpenOutputFolder(target) => {
				let folder = self.output_folder(target);
				self.log_action(format!("opened {folder}"));
			}
			Action::CopyPath(target) => {
				let path = self.target_path(target);
				self.notify(NoticeLevel::Info, format!("copied {path}"));
			}
		}
	}

	fn resume(&mut self) {
		if self.view.header.run_state != RunState::Paused {
			return;
		}

		let low_disk = (LOW_DISK_TICKS.0..LOW_DISK_TICKS.1).contains(&self.tick);
		self.view.header.run_state = if low_disk {
			RunState::PausedLowDisk
		} else {
			RunState::Running
		};
		self.log_action("resumed".to_owned());
	}

	fn pause(&mut self) {
		let state = &mut self.view.header.run_state;
		if matches!(state, RunState::Running | RunState::PausedLowDisk | RunState::Watching) {
			*state = RunState::Paused;
			self.log_action("paused · running jobs finish their current step".to_owned());
		}
	}

	pub(super) fn stop_now(&mut self) {
		let running: Vec<FileId> = self.jobs.drain(..).map(|job| job.file).collect();
		for file in running.into_iter().rev() {
			self.requeue_front(file);
		}

		self.view.header.run_state = RunState::Stopping;
		self.log_action("stop now · running jobs requeued".to_owned());
	}

	fn rescan(&mut self) {
		if self.scan_remaining == 0 {
			self.scan_remaining = 25;
		}
		self.notify(
			NoticeLevel::Info,
			"re-scan started · the source and destination are compared".to_owned(),
		);
	}

	/// Cancels the jobs the new limit doesn't allow, least progress first, and requeues them.
	fn restart_pending_jobs(&mut self) {
		let over: Vec<FileId> = self
			.jobs
			.iter()
			.filter(|job| job.over_limit)
			.map(|job| job.file)
			.collect();
		if over.is_empty() {
			return;
		}

		self.jobs.retain(|job| !job.over_limit);
		for &file in over.iter().rev() {
			self.requeue_front(file);
		}

		if let Some(limit) = &mut self.view.header.performance_limit {
			limit.pending = false;
		}
		self.notify(
			NoticeLevel::Warning,
			format!("restarted {} jobs · their progress was lost", over.len()),
		);
	}

	fn change_settings(&mut self, change: SettingsChange) {
		let settings = &mut self.view.settings;
		match change {
			SettingsChange::Speed(speed) => settings.speed = speed,
			SettingsChange::MinSavings(percent) => settings.min_savings = percent.min(90),
			SettingsChange::Watch(on) => settings.watch = on,
			SettingsChange::StabilityWindow(window) => settings.stability_window = window,
			SettingsChange::KeepAuxiliaryImages(keep) => settings.keep_auxiliary_images = keep,
			SettingsChange::PerformanceLimit(cap) => return self.set_limit(cap),
			SettingsChange::DeleteOriginals { enabled, confirmation } => {
				return self.set_delete_originals(enabled, &confirmation);
			}
		}
		self.notify(NoticeLevel::Info, "settings saved".to_owned());
	}

	fn set_limit(&mut self, cap: Option<u8>) {
		self.view.settings.performance_limit = cap;
		self.view.system.low_priority = cap.is_some();

		let Some(cap) = cap else {
			self.view.header.performance_limit = None;
			for job in &mut self.jobs {
				job.over_limit = false;
			}
			self.notify(NoticeLevel::Info, "performance limit off · full speed".to_owned());
			return;
		};

		let pending = self.jobs.len() > 1;
		self.mark_over_limit();
		self.view.header.performance_limit = Some(Limit { cap, pending });

		let message = if pending {
			format!("performance limit {cap} % · applying… extra jobs finish first, or restart them now")
		} else {
			format!("performance limit {cap} % · low priority, one job at a time")
		};
		self.notify(NoticeLevel::Info, message);
	}

	/// Keeps the job with the most progress and marks the rest, as limit mode runs one job at a time.
	fn mark_over_limit(&mut self) {
		let progress = |job: &super::Job| match job.phase {
			super::Phase::Search { probe } => f64::from(probe) / 100.0,
			super::Phase::Encode { fraction } | super::Phase::Verify { fraction } => fraction,
		};
		let keep = self
			.jobs
			.iter()
			.enumerate()
			.max_by(|a, b| progress(a.1).total_cmp(&progress(b.1)))
			.map(|(index, _)| index);

		for (index, job) in self.jobs.iter_mut().enumerate() {
			job.over_limit = Some(index) != keep;
		}
	}

	fn set_delete_originals(&mut self, enabled: bool, confirmation: &str) {
		if enabled && confirmation != CONFIRMATION {
			self.notify(
				NoticeLevel::Error,
				"deleting originals stays off: type DELETE to confirm".to_owned(),
			);
			return;
		}

		self.view.settings.delete_originals = enabled;
		self.view.header.deleting_originals = enabled;
		if enabled {
			self.notify(
				NoticeLevel::Warning,
				"deleting originals is ON · each is deleted permanently after its archive is verified".to_owned(),
			);
		} else {
			self.notify(NoticeLevel::Info, "deleting originals is off".to_owned());
		}
	}

	fn resolve(&mut self, id: IssueId, verb: Verb) {
		let Some(position) = self.view.issues.iter().position(|issue| issue.id == id) else {
			return;
		};
		let issue = &self.view.issues[position];
		let Some(label) = issue.verbs.label(verb).map(str::to_owned) else {
			return;
		};

		let issue = self.view.issues.remove(position);
		if let Some(file) = issue.file {
			self.apply_resolution(file, issue.kind, verb);
		}
		self.log_action(format!("{} · {}", label.to_lowercase(), issue.name));
	}

	fn apply_resolution(&mut self, file: FileId, kind: IssueKind, verb: Verb) {
		let bytes_in = self.bytes_in(file);
		match (verb, kind) {
			(Verb::Retry, _) => self.requeue_front(file),
			(Verb::Accept, IssueKind::SourceDamaged | IssueKind::EncodeFailed) => {
				self.set_outcome(file, FileState::Copied, Some(bytes_in));
			}
			(Verb::Accept, _) => {}
			(Verb::Exclude, _) => self.set_outcome(file, FileState::Excluded, None),
		}
	}

	fn retry_failed_in(&mut self, folder: NodeId) {
		let issues: Vec<IssueId> = self
			.view
			.issues
			.iter()
			.filter(|issue| issue.verbs.retry.is_some())
			.filter(|issue| issue.folder.is_some_and(|inside| self.is_inside(inside, folder)))
			.map(|issue| issue.id)
			.collect();

		for issue in issues {
			self.resolve(issue, Verb::Retry);
		}
	}

	fn is_inside(&self, inner: NodeId, folder: NodeId) -> bool {
		inner == folder || self.view.library.ancestors(inner).any(|ancestor| ancestor == folder)
	}

	fn expanded(&mut self, folder: NodeId, open: bool) {
		self.loading.retain(|&(loading, _)| loading != folder);
		if !open {
			self.loaded.remove(&folder);
		} else if self.load_latency == 0 {
			self.loaded.insert(folder);
		} else {
			self.loading.push((folder, self.tick + self.load_latency));
			return;
		}
		self.rows_version += 1;
	}

	pub(super) fn finish_everything(&mut self) {
		self.scan_remaining = 0;
		self.jobs.clear();
		self.queue.clear();

		let issues: Vec<(IssueId, IssueKind)> = self.view.issues.iter().map(|issue| (issue.id, issue.kind)).collect();
		for (id, kind) in issues {
			let verb = match kind {
				IssueKind::EncodeFailed | IssueKind::SourceUnreadable => Verb::Exclude,
				_ => Verb::Accept,
			};
			self.resolve(id, verb);
		}

		let unfinished: Vec<FileId> = self
			.files
			.ids()
			.filter(|&file| {
				let state = self.files.get(file).status.state;
				matches!(state, FileState::Queued | FileState::Waiting) || state.is_active()
			})
			.collect();
		for file in unfinished {
			let bytes_in = self.bytes_in(file);
			let bytes_out = self.kind(file).output_size(bytes_in, &mut self.rng);
			self.set_outcome(file, FileState::Done, Some(bytes_out));
		}

		self.log_state(
			FileState::Done,
			"archive complete · every source file is accounted for".to_owned(),
		);
		self.summarize();

		let saved = round_to_u64(as_f64(self.view.overall.saved_bytes) / 1e9);
		self.log_state(FileState::Done, format!("saved {saved} GB in total"));
		self.notifications.clear();
	}

	fn target_path(&self, target: Target) -> String {
		match target {
			Target::File(file) => self.source_path(file),
			Target::Folder(folder) => self.source_folder(Target::Folder(folder)),
		}
	}
}
