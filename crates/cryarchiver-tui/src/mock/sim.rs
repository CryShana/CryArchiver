use super::issues::{NewIssue, attempt};
use super::{
	IMAGE_SLOTS, Job, LOW_DISK_TICKS, MediaKind, MockSource, Phase, SCAN_BATCH_INDEX, START_CLOCK, VIDEO_SLOTS, build,
};
use crate::format::{bytes, saving};
use crate::view::{
	AttemptLog, ClockTime, FileId, FileState, FileStatus, IssueId, IssueKind, LogLine, LogMarker, NoticeLevel,
	Notification, RunState, WorkKind,
};

pub(super) const PROBES: [(f64, f64); 4] = [(34.0, 92.4), (29.0, 96.0), (30.25, 95.1), (30.75, 94.9)];

impl MockSource {
	pub(super) fn clock(&self) -> ClockTime {
		ClockTime(START_CLOCK + u32::try_from(self.tick / 2).unwrap_or(u32::MAX - START_CLOCK))
	}

	pub(super) fn kind(&self, file: FileId) -> MediaKind {
		self.files.kind(file)
	}

	pub(super) fn name(&self, file: FileId) -> String {
		self.files.get(file).name.clone()
	}

	fn log(&mut self, marker: LogMarker, message: String) {
		let time = self.clock();
		self.view.log.push(LogLine { time, marker, message });
	}

	pub(super) fn log_state(&mut self, state: FileState, message: String) {
		self.log(LogMarker::State(state), message);
	}

	pub(super) fn log_action(&mut self, message: String) {
		self.log(LogMarker::Action, message);
	}

	/// Raises a notification and keeps it in the log as well.
	pub(super) fn notify(&mut self, level: NoticeLevel, text: String) {
		self.log_action(text.clone());
		self.notifications.push(Notification { level, text });
	}

	pub(super) fn seed_log(&mut self) {
		let entries = [
			(
				FileState::Done,
				"02. 試験曲.mp3 → Opus 1.71 MB (−53 %) · cover + tags kept",
			),
			(
				FileState::Waiting,
				"GOPR0107.MP4 waits for GOPR0042.MP4 to finish (same size group)",
			),
			(
				FileState::KeptOriginal,
				"action-clip.webm kept · predicted saving 3 %",
			),
			(
				FileState::Warning,
				"garbage_64k_in_middle.mp4: 212 decoder errors concealed · output kept",
			),
			(
				FileState::QualitySearch,
				"group 1080p·60·mpeg2·interlaced: probing CRF for a new group",
			),
			(
				FileState::SourceDamaged,
				"truncated_half.jpg source damaged (no EOI) · waiting for your decision",
			),
			(
				FileState::Done,
				"DSC_0001.JPG → AVIF 2.10 MB (−83 %) · SSIMULACRA2 86.4",
			),
		];

		for (age, (state, message)) in entries.into_iter().enumerate().rev() {
			self.view.log.push(LogLine {
				time: ClockTime(START_CLOCK - u32::try_from(age).unwrap_or_default() * 7),
				marker: LogMarker::State(state),
				message: message.to_owned(),
			});
		}
	}

	pub(super) fn update_system(&mut self) {
		let running = self.view.header.run_state == RunState::Running;
		let (low, high) = match (running, self.view.header.performance_limit) {
			(false, _) => (2, 12),
			(true, Some(limit)) => (u64::from(limit.cap).saturating_sub(18), u64::from(limit.cap) + 4),
			(true, None) => (70, 97),
		};

		let system = &mut self.view.system;
		let target = self.rng.range_u64(low, high);
		let cpu = (u64::from(system.cpu_percent) * 2 + target) / 3;
		system.cpu_percent = u8::try_from(cpu).unwrap_or(100);
		super::summary::push_history(&mut system.cpu_history, cpu);

		let ram = system.ram_used + self.rng.range_u64(0, 200_000_000);
		system.ram_used = ram.saturating_sub(100_000_000).clamp(9_000_000_000, 14_000_000_000);
	}

	pub(super) fn finish_loading(&mut self) {
		let tick = self.tick;
		let due: Vec<_> = self
			.loading
			.iter()
			.filter(|&&(_, at)| at <= tick)
			.map(|&(folder, _)| folder)
			.collect();
		if due.is_empty() {
			return;
		}

		self.loading.retain(|&(_, at)| at > tick);
		self.loaded.extend(due);
		self.rows_version += 1;
	}

	pub(super) fn update_disk_pause(&mut self) {
		let state = self.view.header.run_state;
		if self.tick == LOW_DISK_TICKS.0 && state == RunState::Running {
			self.view.header.run_state = RunState::PausedLowDisk;
			self.notify(
				NoticeLevel::Warning,
				"paused: low disk space on E: · resumes when space frees".to_owned(),
			);
		} else if self.tick == LOW_DISK_TICKS.1 && state == RunState::PausedLowDisk {
			self.view.header.run_state = RunState::Running;
			self.notify(NoticeLevel::Info, "disk space freed on E: · resumed".to_owned());
		}
	}

	pub(super) fn watch_for_new_files(&mut self) {
		if !self.tick.is_multiple_of(20) {
			return;
		}

		self.add_screenshot();
		self.view.header.run_state = RunState::Running;
		self.log_state(
			FileState::Queued,
			"watch: a new file appeared in Screenshots/Desktop".to_owned(),
		);
	}

	fn add_screenshot(&mut self) {
		let index = SCAN_BATCH_INDEX + self.scan_discovered;
		let status = FileStatus {
			state: FileState::Queued,
			progress: None,
			codec: MediaKind::Screenshot.codec().map(str::to_owned),
			bytes_in: MediaKind::Screenshot.input_size(&mut self.rng),
			bytes_out: None,
		};

		let id = self.files.add(
			self.scan_target,
			build::screenshot_name(index),
			status,
			MediaKind::Screenshot,
		);
		self.queue.push_back(id);
		self.scan_discovered += 1;
		if self.loaded.contains(&self.scan_target) {
			self.rows_version += 1;
		}
	}

	pub(super) fn discover(&mut self) {
		if self.scan_remaining == 0 || !self.tick.is_multiple_of(3) {
			return;
		}

		let found = self.rng.range_u64(7, 36).min(self.scan_remaining);
		for _ in 0..found {
			self.add_screenshot();
			self.scan_remaining -= 1;
		}

		if self.scan_remaining == 0 {
			let total = self.files.len();
			self.notify(NoticeLevel::Info, format!("scan finished · {total} files found"));
		}
	}

	pub(super) fn advance_jobs(&mut self) {
		let mut finished = Vec::new();
		for index in 0..self.jobs.len() {
			let job = self.jobs[index];
			match self.next_phase(job) {
				Some(phase) => {
					self.jobs[index].phase = phase;
					self.show_phase(job, phase);
				}
				None => finished.push(job),
			}
		}

		self.jobs
			.retain(|job| !finished.iter().any(|done| done.file == job.file));
		for job in finished {
			self.finish(job);
		}
		self.settle_limit();
	}

	fn next_phase(&mut self, job: Job) -> Option<Phase> {
		match job.phase {
			Phase::Search { probe } if probe < 4 => Some(Phase::Search {
				probe: probe + u32::from(self.tick.is_multiple_of(2)),
			}),
			Phase::Search { .. } => {
				let name = self.name(job.file);
				self.log_state(
					FileState::QualitySearch,
					format!("{name}: CRF 30.75 → VMAF 94.9 (4 probes) · encoding"),
				);
				Some(Phase::Encode { fraction: 0.0 })
			}
			Phase::Encode { fraction } => {
				let next = fraction + self.work_rate(job);
				let verifies = job.work == WorkKind::Encode;
				Some(match (next >= 1.0, verifies) {
					(true, true) => Phase::Verify { fraction: 0.0 },
					(true, false) => Phase::Verify { fraction: 1.0 },
					(false, _) => Phase::Encode { fraction: next },
				})
			}
			Phase::Verify { fraction } if fraction >= 1.0 => None,
			Phase::Verify { fraction } => {
				let next = fraction + self.verify_rate(self.kind(job.file));
				(next < 1.0).then_some(Phase::Verify { fraction: next })
			}
		}
	}

	fn work_rate(&mut self, job: Job) -> f64 {
		match (job.work, self.kind(job.file)) {
			(WorkKind::Copy | WorkKind::Hash | WorkKind::Placeholder | WorkKind::Delete, _) => {
				self.rng.range_f64(0.4, 0.7)
			}
			(WorkKind::Encode, MediaKind::Video) => self.rng.range_f64(0.012, 0.04),
			(WorkKind::Encode, MediaKind::TallPhoto) => self.rng.range_f64(0.05, 0.1),
			(WorkKind::Encode, MediaKind::Audio) => self.rng.range_f64(0.25, 0.5),
			(WorkKind::Encode, _) => self.rng.range_f64(0.12, 0.3),
		}
	}

	fn verify_rate(&mut self, kind: MediaKind) -> f64 {
		match kind {
			MediaKind::Video => self.rng.range_f64(0.08, 0.15),
			_ => self.rng.range_f64(0.35, 0.6),
		}
	}

	fn show_phase(&mut self, job: Job, phase: Phase) {
		let (state, progress) = match (job.work, phase) {
			(WorkKind::Delete, _) => (FileState::DeletePending, None),
			(WorkKind::Hash | WorkKind::Placeholder, _) => (FileState::Waiting, None),
			(_, Phase::Search { .. }) => (FileState::QualitySearch, None),
			(_, Phase::Encode { fraction }) => (FileState::Encoding, Some(fraction)),
			(_, Phase::Verify { fraction }) => (FileState::Verifying, Some(fraction.min(0.99))),
		};

		let status = self.files.status_mut(job.file);
		status.state = state;
		status.progress = progress;
	}

	fn finish(&mut self, job: Job) {
		match job.work {
			WorkKind::Encode => self.finish_encode(job.file),
			WorkKind::Copy => {
				let bytes_in = self.bytes_in(job.file);
				self.set_outcome(job.file, FileState::Copied, Some(bytes_in));
			}
			WorkKind::Hash => self.jobs.push(Job {
				work: WorkKind::Placeholder,
				phase: Phase::Encode { fraction: 0.0 },
				..job
			}),
			WorkKind::Placeholder => {
				self.set_outcome(job.file, FileState::Duplicate, Some(96));
				let name = self.name(job.file);
				self.log_state(
					FileState::Duplicate,
					format!("{name} is a duplicate · placeholder written"),
				);
			}
			WorkKind::Delete => {
				self.set_outcome(
					job.file,
					FileState::OriginalDeleted,
					self.files.get(job.file).status.bytes_out,
				);
				let name = self.name(job.file);
				self.log_state(
					FileState::OriginalDeleted,
					format!("original deleted after verification · {name}"),
				);
			}
		}
	}

	fn finish_encode(&mut self, file: FileId) {
		let kind = self.kind(file);
		let roll = self.rng.unit();
		match kind {
			MediaKind::Video if roll < 0.04 => self.finish_failed(file),
			MediaKind::Video if roll < 0.06 => self.finish_with_warning(file),
			MediaKind::Photo | MediaKind::Screenshot if roll < 0.012 => self.finish_damaged(file),
			_ if roll < 0.09 => self.finish_kept(file),
			_ => self.finish_done(file),
		}
	}

	pub(super) fn set_outcome(&mut self, file: FileId, state: FileState, bytes_out: Option<u64>) {
		let status = self.files.status_mut(file);
		let newly_written = bytes_out.filter(|_| status.bytes_out.is_none());
		status.state = state;
		status.progress = None;
		status.bytes_out = bytes_out;

		if let Some(written) = newly_written {
			let system = &mut self.view.system;
			system.disk_free = system.disk_free.saturating_sub(written);
		}
	}

	pub(super) fn bytes_in(&self, file: FileId) -> u64 {
		self.files.get(file).status.bytes_in
	}

	fn finish_done(&mut self, file: FileId) {
		let kind = self.kind(file);
		let bytes_in = self.bytes_in(file);
		let bytes_out = kind.output_size(bytes_in, &mut self.rng);
		self.set_outcome(file, FileState::Done, Some(bytes_out));

		if kind.is_video() || self.rng.chance(0.3) {
			let name = self.name(file);
			let codec = kind.codec().unwrap_or("copy");
			let saved = saving(bytes_in, bytes_out).replace('%', " %");
			self.log_state(
				FileState::Done,
				format!("{name} → {codec} {} ({saved})", bytes(bytes_out)),
			);
		}
	}

	fn finish_kept(&mut self, file: FileId) {
		let bytes_in = self.bytes_in(file);
		self.set_outcome(file, FileState::KeptOriginal, Some(bytes_in));

		let name = self.name(file);
		let predicted = self.rng.range_u64(2, 9);
		self.log_state(
			FileState::KeptOriginal,
			format!("{name} kept · predicted saving {predicted} %"),
		);
	}

	fn finish_failed(&mut self, file: FileId) {
		self.set_outcome(file, FileState::Failed, None);

		let name = self.name(file);
		self.log_state(
			FileState::Failed,
			format!("{name} failed 3 times · waiting for your decision"),
		);
		let lines = ["[h264] Invalid NAL unit size (1197 > 1024)", "Conversion failed!"];
		let attempts = (1..=3).map(|number| attempt(number, &[lines[0], lines[1]])).collect();
		self.open_issue(
			file,
			IssueKind::EncodeFailed,
			"Encode failed the same way 3 times",
			attempts,
		);
	}

	fn finish_damaged(&mut self, file: FileId) {
		self.set_outcome(file, FileState::SourceDamaged, None);

		let name = self.name(file);
		self.log_state(
			FileState::SourceDamaged,
			format!("{name} source damaged (truncated) · waiting for your decision"),
		);
		let lines = [
			"[png] IDAT: inflate returned -5",
			"decoded 640 of 1440 rows",
			"structural check: IEND not found",
		];
		self.open_issue(
			file,
			IssueKind::SourceDamaged,
			"Source damaged: the file ends early",
			vec![attempt(1, &lines)],
		);
	}

	fn finish_with_warning(&mut self, file: FileId) {
		let kind = self.kind(file);
		let bytes_in = self.bytes_in(file);
		let bytes_out = kind.output_size(bytes_in, &mut self.rng);
		self.set_outcome(file, FileState::Warning, Some(bytes_out));

		let name = self.name(file);
		let errors = self.rng.range_u64(3, 240);
		self.log_state(
			FileState::Warning,
			format!("{name}: {errors} decoder errors concealed · output kept"),
		);
	}

	pub(super) fn open_issue(&mut self, file: FileId, kind: IssueKind, reason: &str, attempts: Vec<AttemptLog>) {
		let folder = self.files.folder(file);
		let issue = NewIssue {
			kind,
			file: Some(file),
			folder: Some(folder),
			name: self.name(file),
			location: self.view.library.relative_path(folder),
			reason,
			attempts,
		};

		let id = IssueId(self.next_issue);
		self.next_issue += 1;
		self.view.issues.push(issue.into_issue(id));
	}

	pub(super) fn requeue_front(&mut self, file: FileId) {
		self.set_outcome(file, FileState::Queued, None);
		self.queue.push_front(file);
	}

	pub(super) fn start_jobs(&mut self) {
		if let Some(limit) = self.view.header.performance_limit {
			if !limit.pending && self.jobs.is_empty() {
				let Some(file) = self.queue.pop_front() else { return };
				self.start_job(file);
			}
			return;
		}

		while self.jobs_where(true) < VIDEO_SLOTS {
			let Some(file) = self.take_queued(true) else { break };
			self.start_job(file);
		}

		while self.jobs_where(false) < IMAGE_SLOTS {
			let Some(file) = self.take_queued(false) else { break };
			self.start_job(file);
		}
	}

	fn jobs_where(&self, video: bool) -> usize {
		self.jobs
			.iter()
			.filter(|job| job.work != WorkKind::Delete && self.kind(job.file).is_video() == video)
			.count()
	}

	fn take_queued(&mut self, video: bool) -> Option<FileId> {
		let position = self
			.queue
			.iter()
			.position(|&file| self.kind(file).is_video() == video)?;
		self.queue.remove(position)
	}

	fn start_job(&mut self, file: FileId) {
		let kind = self.kind(file);
		let work = match kind {
			MediaKind::Document => WorkKind::Copy,
			MediaKind::Photo if self.rng.chance(0.03) => WorkKind::Hash,
			_ => WorkKind::Encode,
		};
		let phase = if kind.is_video() && self.rng.chance(0.4) {
			Phase::Search { probe: 1 }
		} else {
			Phase::Encode { fraction: 0.0 }
		};
		let fps = if kind.is_video() {
			self.rng.range_f64(40.0, 160.0)
		} else {
			0.0
		};

		let job = Job {
			file,
			work,
			phase,
			fps,
			over_limit: false,
		};
		self.jobs.push(job);
		self.show_phase(job, phase);
	}

	/// Once the jobs beyond a new limit have finished on their own, the limit is fully applied.
	fn settle_limit(&mut self) {
		let Some(limit) = &mut self.view.header.performance_limit else {
			return;
		};

		if limit.pending && self.jobs.len() <= 1 {
			limit.pending = false;
			for job in &mut self.jobs {
				job.over_limit = false;
			}
			self.notify(NoticeLevel::Info, "performance limit fully applied".to_owned());
		}
	}

	pub(super) fn delete_originals(&mut self) {
		let deleting = self.jobs.iter().any(|job| job.work == WorkKind::Delete);
		if !self.view.header.deleting_originals || deleting {
			return;
		}

		let total = self.files.len() as u64;
		for _ in 0..total.min(500) {
			let file = FileId(self.delete_cursor % total.max(1));
			self.delete_cursor += 1;

			let status = &self.files.get(file).status;
			let open_issue = self.view.issues.iter().any(|issue| issue.file == Some(file));
			if status.state == FileState::Done && !open_issue {
				let job = Job {
					file,
					work: WorkKind::Delete,
					phase: Phase::Encode { fraction: 0.0 },
					fps: 0.0,
					over_limit: false,
				};
				self.jobs.push(job);
				self.show_phase(job, job.phase);
				return;
			}
		}
	}
}
