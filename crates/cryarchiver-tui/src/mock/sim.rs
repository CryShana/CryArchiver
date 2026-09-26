use std::collections::BTreeSet;
use std::time::Duration;

use super::{
	HISTORY_LEN, IMAGE_SLOTS, Job, MediaKind, MockSource, Phase, SAVINGS_WINDOW, SCAN_BATCH_INDEX, START_CLOCK,
	VIDEO_SLOTS, build,
};
use crate::format::{as_f64, bytes, fraction, round_to_u64, saving};
use crate::source::Action;
use crate::view::{
	ClockTime, Entry, FileState, FileStatus, Issue, JobActivity, LogLine, LogMarker, NodeId, Parity, RunState,
	WorkerJob,
};

pub(super) const PROBES: [(f64, f64); 4] = [(34.0, 92.4), (29.0, 96.0), (30.25, 95.1), (30.75, 94.9)];

#[derive(Debug, Default, Clone, Copy)]
struct Tally {
	done: u64,
	total: u64,
	partial: f64,
	bytes_in: u64,
	bytes_out: u64,
	issues: u32,
	unscanned: bool,
}

impl Tally {
	fn add(&mut self, other: &Self) {
		self.done += other.done;
		self.total += other.total;
		self.partial += other.partial;
		self.bytes_in += other.bytes_in;
		self.bytes_out += other.bytes_out;
		self.issues += other.issues;
		self.unscanned |= other.unscanned;
	}

	fn complete(&self) -> bool {
		self.done == self.total && self.issues == 0 && !self.unscanned
	}

	fn progress(&self) -> f64 {
		if self.total == 0 {
			return 1.0;
		}

		((as_f64(self.done) + self.partial) / as_f64(self.total)).clamp(0.0, 1.0)
	}
}

#[derive(Debug, Default)]
struct Counts {
	queued: u64,
	kept: u64,
	duplicates: u64,
}

impl MockSource {
	pub(super) fn clock(&self) -> ClockTime {
		ClockTime(START_CLOCK + u32::try_from(self.tick / 2).unwrap_or(u32::MAX - START_CLOCK))
	}

	pub(super) fn kind(&self, file: NodeId) -> MediaKind {
		self.kinds[file.index()].unwrap_or(MediaKind::Document)
	}

	fn name(&self, file: NodeId) -> String {
		self.view.library.node(file).name.clone()
	}

	fn log(&mut self, marker: LogMarker, message: String) {
		let time = self.clock();
		self.view.log.push(LogLine { time, marker, message });
	}

	fn log_state(&mut self, state: FileState, message: String) {
		self.log(LogMarker::State(state), message);
	}

	fn log_action(&mut self, message: String) {
		self.log(LogMarker::Action, message);
	}

	pub(super) fn seed_log(&mut self) {
		let entries = [
			(
				FileState::Done,
				"02. 試験曲.mp3 → Opus 1.71 MB (−53 %) · cover + tags kept",
			),
			(
				FileState::Waiting,
				"GOPR0107.MP4 waiting for space · needs 4.3 GB, 3.9 GB free",
			),
			(
				FileState::KeptOriginal,
				"action-clip.webm kept · predicted saving 3 %",
			),
			(
				FileState::Warning,
				"E: free 1.20 TB is below 110 % of the 1.62 TB still to process · continuing",
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
		let paused = self.view.header.run_state != RunState::Running;
		let limited = self.view.header.performance_limit.is_some();
		let (low, high) = match (paused, limited) {
			(true, _) => (2, 12),
			(false, true) => (34, 54),
			(false, false) => (70, 97),
		};

		let system = &mut self.view.system;
		let target = self.rng.range_u64(low, high);
		let cpu = (u64::from(system.cpu_percent) * 2 + target) / 3;
		system.cpu_percent = u8::try_from(cpu).unwrap_or(100);
		push_history(&mut system.cpu_history, cpu);

		let ram = system.ram_used + self.rng.range_u64(0, 200_000_000);
		system.ram_used = ram.saturating_sub(100_000_000).clamp(9_000_000_000, 14_000_000_000);
	}

	pub(super) fn discover(&mut self) {
		if self.scan_remaining == 0 || !self.tick.is_multiple_of(3) {
			return;
		}

		let found = self.rng.range_u64(7, 36).min(self.scan_remaining);
		for _ in 0..found {
			let index = SCAN_BATCH_INDEX + self.scan_discovered;
			let status = FileStatus {
				state: FileState::Queued,
				progress: None,
				codec: MediaKind::Screenshot.codec().map(str::to_owned),
				bytes_in: MediaKind::Screenshot.input_size(&mut self.rng),
				bytes_out: None,
			};

			let id = self
				.view
				.library
				.add_file(self.scan_target, build::screenshot_name(index), status);
			self.kinds.push(Some(MediaKind::Screenshot));
			self.queue.push_back(id);
			self.scan_remaining -= 1;
			self.scan_discovered += 1;
		}

		if self.scan_remaining == 0 {
			let total = self.view.library.len();
			self.log_state(FileState::Done, format!("scan finished · {total} entries found"));
		}
	}

	pub(super) fn advance_jobs(&mut self) {
		let mut finished = Vec::new();
		for index in 0..self.jobs.len() {
			let job = self.jobs[index];
			match self.next_phase(job) {
				Some(phase) => {
					self.jobs[index].phase = phase;
					self.show_phase(job.file, phase);
				}
				None => finished.push(job.file),
			}
		}

		self.jobs.retain(|job| !finished.contains(&job.file));
		for file in finished {
			self.finish(file);
		}
	}

	fn next_phase(&mut self, job: Job) -> Option<Phase> {
		let kind = self.kind(job.file);
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
				let next = fraction + self.encode_rate(kind);
				Some(if next >= 1.0 {
					Phase::Verify { fraction: 0.0 }
				} else {
					Phase::Encode { fraction: next }
				})
			}
			Phase::Verify { fraction } => {
				let next = fraction + self.verify_rate(kind);
				(next < 1.0).then_some(Phase::Verify { fraction: next })
			}
		}
	}

	fn encode_rate(&mut self, kind: MediaKind) -> f64 {
		match kind {
			MediaKind::Video => self.rng.range_f64(0.012, 0.04),
			MediaKind::TallPhoto => self.rng.range_f64(0.05, 0.1),
			MediaKind::Audio => self.rng.range_f64(0.25, 0.5),
			MediaKind::Photo | MediaKind::Screenshot | MediaKind::Document => self.rng.range_f64(0.12, 0.3),
		}
	}

	fn verify_rate(&mut self, kind: MediaKind) -> f64 {
		match kind {
			MediaKind::Video => self.rng.range_f64(0.08, 0.15),
			_ => self.rng.range_f64(0.35, 0.6),
		}
	}

	fn show_phase(&mut self, file: NodeId, phase: Phase) {
		let (state, progress) = match phase {
			Phase::Search { .. } => (FileState::QualitySearch, None),
			Phase::Encode { fraction } => (FileState::Encoding, Some(fraction)),
			Phase::Verify { fraction } => (FileState::Verifying, Some(fraction)),
		};

		if let Some(status) = self.view.library.file_mut(file) {
			status.state = state;
			status.progress = progress;
		}
	}

	fn finish(&mut self, file: NodeId) {
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

	fn set_outcome(&mut self, file: NodeId, state: FileState, bytes_out: Option<u64>) {
		if let Some(status) = self.view.library.file_mut(file) {
			status.state = state;
			status.progress = None;
			status.bytes_out = bytes_out;
		}

		if let Some(written) = bytes_out {
			let system = &mut self.view.system;
			system.disk_free = system.disk_free.saturating_sub(written);
		}
	}

	fn bytes_in(&self, file: NodeId) -> u64 {
		self.view.library.file(file).map_or(0, |status| status.bytes_in)
	}

	fn finish_done(&mut self, file: NodeId) {
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

	fn finish_kept(&mut self, file: NodeId) {
		let bytes_in = self.bytes_in(file);
		self.set_outcome(file, FileState::KeptOriginal, Some(bytes_in));

		let name = self.name(file);
		let predicted = self.rng.range_u64(2, 9);
		self.log_state(
			FileState::KeptOriginal,
			format!("{name} kept · predicted saving {predicted} %"),
		);
	}

	fn finish_failed(&mut self, file: NodeId) {
		self.set_outcome(file, FileState::Failed, None);

		let name = self.name(file);
		self.log_state(
			FileState::Failed,
			format!("{name} failed 3 times · waiting for your decision"),
		);
		self.open_issue(
			file,
			FileState::Failed,
			"Encode failed the same way 3 times",
			"Waiting for your decision",
			&[
				"[h264] Invalid NAL unit size (1197 > 1024)",
				"Error while decoding stream #0:0: Invalid data found when processing input",
				"Conversion failed!",
				"exit code 1 (attempt 3/3)",
			],
		);
	}

	fn finish_damaged(&mut self, file: NodeId) {
		self.set_outcome(file, FileState::SourceDamaged, None);

		let name = self.name(file);
		self.log_state(
			FileState::SourceDamaged,
			format!("{name} source damaged (truncated) · waiting for your decision"),
		);
		self.open_issue(
			file,
			FileState::SourceDamaged,
			"Source damaged: the file ends early",
			"Waiting for your decision",
			&[
				"[png] IDAT: inflate returned -5",
				"decoded 640 of 1440 rows",
				"structural check: IEND not found",
			],
		);
	}

	fn finish_with_warning(&mut self, file: NodeId) {
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
		self.open_issue(
			file,
			FileState::Warning,
			&format!("Encoded with a warning: {errors} decoder errors concealed"),
			"Output kept · glitches carried over",
			&[
				"[h264] error while decoding MB 12 31",
				"[h264] concealing 840 DC, 840 AC, 840 MV errors",
			],
		);
	}

	fn open_issue(&mut self, file: NodeId, state: FileState, reason: &str, resolution: &str, log: &[&str]) {
		let library = &self.view.library;
		let folder = library
			.node(file)
			.parent
			.map(|parent| library.relative_path(parent))
			.unwrap_or_default();
		self.view.issues.push(Issue {
			file,
			state,
			name: library.node(file).name.clone(),
			folder,
			reason: reason.to_owned(),
			resolution: resolution.to_owned(),
			log: log.iter().map(|&line| line.to_owned()).collect(),
		});
	}

	fn close_issue(&mut self, file: NodeId) -> bool {
		let before = self.view.issues.len();
		self.view.issues.retain(|issue| issue.file != file);
		self.waiting.retain(|&(waiting, _)| waiting != file);
		before != self.view.issues.len()
	}

	fn requeue_front(&mut self, file: NodeId) {
		self.set_outcome(file, FileState::Queued, None);
		self.queue.push_front(file);
	}

	pub(super) fn retry_waiting(&mut self) {
		let tick = self.tick;
		let due: Vec<NodeId> = self
			.waiting
			.iter()
			.filter(|&&(_, at)| at <= tick)
			.map(|&(file, _)| file)
			.collect();

		for file in due {
			self.close_issue(file);
			self.requeue_front(file);

			let name = self.name(file);
			self.log_state(FileState::Waiting, format!("{name}: space freed · queued again"));
		}
	}

	pub(super) fn start_jobs(&mut self) {
		while self.jobs_where(true) < VIDEO_SLOTS {
			let Some(file) = self.take_queued(true) else { break };
			self.start(file);
		}

		while self.jobs_where(false) < IMAGE_SLOTS {
			let Some(file) = self.take_queued(false) else { break };
			self.start(file);
		}
	}

	fn jobs_where(&self, video: bool) -> usize {
		self.jobs
			.iter()
			.filter(|job| self.kind(job.file).is_video() == video)
			.count()
	}

	fn take_queued(&mut self, video: bool) -> Option<NodeId> {
		let position = self
			.queue
			.iter()
			.position(|&file| self.kind(file).is_video() == video)?;
		self.queue.remove(position)
	}

	fn start(&mut self, file: NodeId) {
		let kind = self.kind(file);
		if kind == MediaKind::Document {
			let bytes_in = self.bytes_in(file);
			self.set_outcome(file, FileState::Copied, Some(bytes_in));
			return;
		}

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

		self.jobs.push(Job { file, phase, fps });
		self.show_phase(file, phase);
	}
}

impl MockSource {
	pub(super) fn summarize(&mut self) {
		let open: BTreeSet<NodeId> = self.view.issues.iter().map(|issue| issue.file).collect();
		let (tallies, counts) = self.tally(&open);
		self.write_folder_stats(&tallies);

		let root = tallies[self.view.library.root().index()];
		self.write_overall(&root, &counts, open.len());
		self.write_workers();
	}

	pub(super) fn record_savings(&mut self, saved_before: u64) {
		let delta = self.view.overall.saved_bytes.saturating_sub(saved_before) / 1_000_000;
		self.recent_savings.push_back(delta);
		if self.recent_savings.len() > SAVINGS_WINDOW {
			self.recent_savings.pop_front();
		}

		let window = self.recent_savings.iter().sum();
		push_history(&mut self.view.overall.savings_history, window);
	}

	fn tally(&self, open: &BTreeSet<NodeId>) -> (Vec<Tally>, Counts) {
		let library = &self.view.library;
		let mut tallies = vec![Tally::default(); library.len()];
		let mut counts = Counts::default();

		for id in library.ids().rev() {
			let node = library.node(id);
			let own = match &node.entry {
				Entry::File(status) => self.file_tally(id, status, open, &mut counts),
				Entry::Folder(_) => {
					let tally = &mut tallies[id.index()];
					tally.unscanned |= id == self.scan_target && self.scan_remaining > 0;
					*tally
				}
			};

			if let Some(parent) = node.parent {
				tallies[parent.index()].add(&own);
			}
		}

		(tallies, counts)
	}

	fn file_tally(&self, id: NodeId, status: &FileStatus, open: &BTreeSet<NodeId>, counts: &mut Counts) -> Tally {
		match status.state {
			FileState::Queued | FileState::Waiting => counts.queued += 1,
			FileState::KeptOriginal => counts.kept += 1,
			FileState::Duplicate => counts.duplicates += 1,
			_ => {}
		}

		let finished = matches!(
			status.state,
			FileState::Done | FileState::KeptOriginal | FileState::Copied | FileState::Duplicate
		) || self.excluded.contains(&id);
		let written = status.bytes_out.filter(|_| finished);

		Tally {
			done: u64::from(finished),
			total: 1,
			partial: if status.state.is_active() {
				status.progress.unwrap_or(0.0)
			} else {
				0.0
			},
			bytes_in: written.map_or(0, |_| status.bytes_in),
			bytes_out: written.unwrap_or(0),
			issues: u32::from(open.contains(&id)),
			unscanned: false,
		}
	}

	fn write_folder_stats(&mut self, tallies: &[Tally]) {
		let folders: Vec<NodeId> = self
			.view
			.library
			.ids()
			.filter(|&id| self.view.library.is_folder(id))
			.collect();
		for id in folders {
			let tally = tallies[id.index()];
			if let Entry::Folder(stats) = &mut self.view.library.node_mut(id).entry {
				stats.files_done = tally.done;
				stats.files_total = tally.total;
				stats.progress = tally.progress();
				stats.bytes_in = tally.bytes_in;
				stats.bytes_out = tally.bytes_out;
				stats.open_issues = tally.issues;
				stats.complete = tally.complete();
			}
		}
	}

	fn write_overall(&mut self, root: &Tally, counts: &Counts, open_issues: usize) {
		let active = self.jobs.len() as u64;
		let idle = self.queue.is_empty() && self.jobs.is_empty() && self.waiting.is_empty();
		let scan_complete = self.scan_remaining == 0;
		let saved = root.bytes_in.saturating_sub(root.bytes_out);

		if idle && scan_complete {
			self.view.header.run_state = RunState::Finished;
		}

		let overall = &mut self.view.overall;
		overall.files_done = root.done;
		overall.files_active = active;
		overall.files_queued = counts.queued;
		overall.files_total = root.total;
		overall.scan_complete = scan_complete;
		overall.progress = root.progress();
		overall.kept = counts.kept;
		overall.duplicates = counts.duplicates;
		overall.open_issues = open_issues as u64;
		overall.saved_bytes = saved;
		overall.saved_fraction = fraction(saved, root.bytes_in);
		overall.eta = (!idle).then(|| Duration::from_secs(counts.queued * 6 / 5 + active * 20));
		overall.parity = (idle && scan_complete && open_issues == 0).then(|| Parity {
			source_files: root.total,
			archive_files: root.total - self.excluded.len() as u64,
			excluded: self.excluded.len() as u64,
		});
	}

	fn write_workers(&mut self) {
		let jobs = self
			.jobs
			.iter()
			.map(|job| {
				let kind = self.kind(job.file);
				let status = self.view.library.file(job.file);
				WorkerJob {
					file: job.file,
					codec: kind.codec().unwrap_or("copy").to_owned(),
					name: self.view.library.node(job.file).name.clone(),
					state: status.map_or(FileState::Encoding, |status| status.state),
					activity: activity(job, kind),
				}
			})
			.collect();
		self.view.workers.jobs = jobs;
	}
}

fn activity(job: &Job, kind: MediaKind) -> JobActivity {
	match job.phase {
		Phase::Search { probe } => {
			let (crf, score) = PROBES[(probe.clamp(1, 4) - 1) as usize];
			JobActivity::QualitySearch {
				probe,
				probes: 4,
				crf,
				metric: "VMAF".to_owned(),
				score,
			}
		}
		Phase::Encode { fraction } => JobActivity::Progress {
			fraction,
			fps: kind.is_video().then_some(job.fps),
			speed: kind.is_video().then_some(job.fps / 30.0),
		},
		Phase::Verify { fraction } => JobActivity::Progress {
			fraction,
			fps: None,
			speed: None,
		},
	}
}

fn push_history(history: &mut Vec<u64>, value: u64) {
	history.push(value);
	if history.len() > HISTORY_LEN {
		history.remove(0);
	}
}

impl MockSource {
	pub(super) fn apply(&mut self, action: Action) {
		match action {
			Action::TogglePause => self.toggle_pause(),
			Action::TogglePerformanceLimit => self.toggle_limit(),
			Action::Retry(file) => self.retry(file),
			Action::RetryFailedIn(folder) => self.retry_in(folder),
			Action::CopyOriginal(file) => self.copy_original(file),
			Action::Exclude(file) => self.exclude(file),
			Action::OpenSourceFolder(id) => {
				let folder = self.source_folder(id);
				self.log_action(format!("opened {folder}"));
			}
			Action::OpenOutputFolder(id) => {
				let folder = self.output_folder(id);
				self.log_action(format!("opened {folder}"));
			}
			Action::CopyPath(id) => {
				let path = self.source_path(id);
				self.log_action(format!("copied {path}"));
			}
		}
	}

	fn toggle_pause(&mut self) {
		let header = &mut self.view.header;
		let message = match header.run_state {
			RunState::Running => {
				header.run_state = RunState::Paused;
				"paused · running jobs finish their current step"
			}
			RunState::Paused => {
				header.run_state = RunState::Running;
				"resumed"
			}
			RunState::Finished => return,
		};
		self.log_action(message.to_owned());
	}

	fn toggle_limit(&mut self) {
		let limit = if self.view.header.performance_limit.is_some() {
			None
		} else {
			Some(50)
		};
		self.view.header.performance_limit = limit;
		self.view.system.low_priority = limit.is_some();

		let message = match limit {
			Some(percent) => format!("performance limited to {percent} % CPU · low priority"),
			None => "full speed · normal priority".to_owned(),
		};
		self.log_action(message);
	}

	fn retry(&mut self, file: NodeId) {
		let name = self.name(file);
		let reopened = self.close_issue(file) | self.excluded.remove(&file);
		if !reopened {
			self.log_action(format!("nothing to retry · {name}"));
			return;
		}

		self.requeue_front(file);
		self.log_action(format!("retry · {name}"));
	}

	fn retry_in(&mut self, folder: NodeId) {
		let files: Vec<NodeId> = self
			.view
			.issues
			.iter()
			.map(|issue| issue.file)
			.filter(|&file| self.is_inside(file, folder))
			.collect();

		for file in files {
			self.retry(file);
		}
	}

	fn is_inside(&self, file: NodeId, folder: NodeId) -> bool {
		let mut current = Some(file);
		while let Some(id) = current {
			if id == folder {
				return true;
			}
			current = self.view.library.node(id).parent;
		}
		false
	}

	fn copy_original(&mut self, file: NodeId) {
		if !self.close_issue(file) {
			return;
		}

		let bytes_in = self.bytes_in(file);
		self.set_outcome(file, FileState::Copied, Some(bytes_in));

		let name = self.name(file);
		self.log_action(format!("original copied as you chose · {name}"));
	}

	fn exclude(&mut self, file: NodeId) {
		if !self.close_issue(file) {
			return;
		}

		self.excluded.insert(file);

		let name = self.name(file);
		self.log_action(format!("excluded from the archive · {name}"));
	}

	pub(super) fn finish_everything(&mut self) {
		self.scan_remaining = 0;
		self.jobs.clear();
		self.queue.clear();

		let issues: Vec<(NodeId, FileState)> = self.view.issues.iter().map(|issue| (issue.file, issue.state)).collect();
		for (file, state) in issues {
			if state == FileState::Failed {
				self.exclude(file);
			} else {
				self.copy_original(file);
			}
		}

		let unfinished: Vec<NodeId> = self
			.view
			.library
			.ids()
			.filter(|&id| {
				self.view.library.file(id).is_some_and(|status| {
					status.state == FileState::Queued || status.state.is_active() || status.state == FileState::Waiting
				})
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
	}
}
