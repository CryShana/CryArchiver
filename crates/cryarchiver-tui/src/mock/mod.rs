//! A fake archive run for developing the TUI before the project layer exists. Seeded, so the same seed and the
//! same number of [`MockSource::step`] calls always produce the same view.

mod actions;
mod build;
mod details;
mod filter;
mod issues;
mod rng;
mod sim;
mod store;
mod summary;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use self::rng::Rng;
use self::store::FileStore;
use crate::source::{Action, ArchiveSource};
use crate::view::{
	ArchiveView, FileDetails, FileId, FileRow, FilterResult, Header, IssueId, Log, NodeId, Notification, Overall,
	RunState, Settings, Speed, SystemStats, WorkKind, Workers,
};

const TICK: Duration = Duration::from_millis(500);
const START_CLOCK: u32 = 14 * 3600 + 2 * 60 + 7;
const LOG_CAPACITY: usize = 500;
const HISTORY_LEN: usize = 60;
const SAVINGS_WINDOW: usize = 8;
const VIDEO_SLOTS: usize = 3;
const IMAGE_SLOTS: usize = 3;
const SCAN_BATCH_INDEX: u64 = 29_000;
const SCAN_EXTRA: u64 = 2_200;
const LOW_DISK_TICKS: (u64, u64) = (80, 120);

pub struct MockSource {
	rng: Rng,
	tick: u64,
	view: ArchiveView,
	files: FileStore,
	showcase: BTreeMap<FileId, Showcase>,
	queue: VecDeque<FileId>,
	jobs: Vec<Job>,
	duplicate_of: BTreeMap<FileId, FileId>,
	next_issue: u64,
	issue_logs: BTreeMap<IssueId, Vec<issues::AttemptLog>>,
	scan_target: NodeId,
	scan_remaining: u64,
	scan_discovered: u64,
	loaded: BTreeSet<NodeId>,
	loading: Vec<(NodeId, u64)>,
	load_latency: u64,
	rows_version: u64,
	filter: Option<FilterResult>,
	notifications: Vec<Notification>,
	delete_cursor: u64,
	recent_savings: VecDeque<u64>,
	last_refresh: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaKind {
	Photo,
	TallPhoto,
	Video,
	Audio,
	Screenshot,
	Document,
}

#[derive(Debug, Clone, Copy)]
struct Showcase {
	source_info: &'static str,
	plan: &'static [(&'static str, &'static str)],
	quality: Option<&'static str>,
	note: Option<&'static str>,
}

#[derive(Debug, Clone, Copy)]
struct Job {
	file: FileId,
	work: WorkKind,
	phase: Phase,
	fps: f64,
	over_limit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
	Search { probe: u32 },
	Encode { fraction: f64 },
	Verify { fraction: f64 },
}

impl MockSource {
	pub const DEFAULT_SEED: u64 = 0x00C0_FFEE;

	pub fn new(seed: u64) -> Self {
		let mut rng = Rng::new(seed);
		let built = build::build(Rng::new(rng.next_u64()));
		let (issues, issue_logs): (Vec<_>, BTreeMap<_, _>) = (0u64..)
			.zip(built.issues)
			.map(|(id, issue)| {
				let id = IssueId(id);
				let (issue, logs) = issue.into_issue(id);
				(issue, (id, logs))
			})
			.unzip();

		let mut source = Self {
			view: ArchiveView {
				header: Header {
					source: r"D:\Archive\Family".to_owned(),
					destination: r"E:\Archive-AV1\Family".to_owned(),
					codecs: Some("LATEST".to_owned()),
					limit_pending: false,
					run_state: RunState::Running,
					deleting_originals: false,
					deleting_paused: false,
					demo: true,
				},
				settings: Settings {
					speed: Speed::Moderate,
					min_savings: 10,
					watch: false,
					performance_limit: None,
					stability_window: Duration::from_secs(30),
					keep_auxiliary_images: true,
					delete_originals: false,
				},
				overall: initial_overall(&mut rng),
				workers: Workers {
					jobs: Vec::new(),
					slots: format!("{VIDEO_SLOTS} video + {IMAGE_SLOTS} image slots"),
				},
				system: SystemStats {
					cpu_percent: 72,
					cpu_history: initial_history(&mut rng, 60, 92),
					ram_used: 11_200_000_000,
					ram_total: 32_000_000_000,
					disk_label: "E:".to_owned(),
					disk_free: 1_200_000_000_000,
					low_priority: false,
					toolchain: "ffmpeg 9.0.1 · SVT-AV1 4.2".to_owned(),
				},
				library: built.library,
				log: Log::with_capacity(LOG_CAPACITY),
				issues,
			},
			rng,
			tick: 0,
			next_issue: 0,
			issue_logs,
			files: built.files,
			showcase: built.showcase,
			queue: built.queue,
			jobs: built.jobs,
			duplicate_of: built.duplicate_of,
			scan_target: built.scan_target,
			scan_remaining: SCAN_EXTRA,
			scan_discovered: 0,
			loaded: BTreeSet::new(),
			loading: Vec::new(),
			load_latency: 0,
			rows_version: 0,
			filter: None,
			notifications: Vec::new(),
			delete_cursor: 0,
			recent_savings: VecDeque::new(),
			last_refresh: None,
		};

		source.next_issue = source.view.issues.len() as u64;
		source.seed_log();
		source.summarize();
		source
	}

	/// The same archive with every file finished and nothing left open, for the completion screen.
	pub fn completed(seed: u64) -> Self {
		let mut source = Self::new(seed);
		source.finish_everything();
		source
	}

	/// Folder rows then take this many ticks to load after [`Action::Expanded`], as a slow project would.
	#[must_use]
	pub fn with_load_latency(mut self, ticks: u64) -> Self {
		self.load_latency = ticks;
		self
	}

	/// Advances the simulation by one tick of half a second.
	pub fn step(&mut self) {
		let saved_before = self.view.overall.saved_bytes;
		let files_before = self.files.len();
		self.tick += 1;
		self.update_system();
		self.finish_loading();
		self.update_disk_pause();

		if self.view.header.run_state == RunState::Watching {
			self.watch_for_new_files();
		}

		if self.view.header.run_state == RunState::Running {
			self.discover();
		}

		if matches!(self.view.header.run_state, RunState::Running | RunState::PausedLowDisk) {
			self.advance_jobs();
		}

		if self.view.header.run_state == RunState::Running {
			self.start_jobs();
			self.delete_originals();
		}

		self.summarize();
		self.refresh_filter(self.files.len() != files_before);
		self.record_savings(saved_before);
	}
}

impl Default for MockSource {
	fn default() -> Self {
		Self::new(Self::DEFAULT_SEED)
	}
}

impl ArchiveSource for MockSource {
	fn refresh(&mut self) -> bool {
		let now = Instant::now();
		let Some(last) = self.last_refresh else {
			self.last_refresh = Some(now);
			return false;
		};

		let due = now.duration_since(last).as_millis() / TICK.as_millis();
		if due == 0 {
			return false;
		}

		for _ in 0..due.min(20) {
			self.step();
		}
		self.last_refresh = Some(now);
		true
	}

	fn view(&self) -> &ArchiveView {
		&self.view
	}

	fn files(&self, folder: NodeId) -> Option<&[FileRow]> {
		self.loaded.contains(&folder).then(|| self.files.in_folder(folder))
	}

	fn rows_version(&self) -> u64 {
		self.rows_version
	}

	fn filter(&self) -> Option<&FilterResult> {
		self.filter.as_ref()
	}

	fn details(&self, file: FileId) -> Option<FileDetails> {
		details::build(self, file)
	}

	fn issue_log(&self, issue: IssueId, attempt: u32) -> Option<Vec<String>> {
		let index = usize::try_from(attempt.checked_sub(1)?).ok()?;
		self.issue_logs.get(&issue)?.get(index).cloned()
	}

	fn take_notifications(&mut self) -> Vec<Notification> {
		std::mem::take(&mut self.notifications)
	}

	fn perform(&mut self, action: Action) {
		self.apply(action);
		self.summarize();
		self.refresh_filter(false);
	}

	fn shutdown(&mut self) {
		self.stop_now();
		self.summarize();
	}
}

fn initial_overall(rng: &mut Rng) -> Overall {
	Overall {
		files_done: 0,
		files_active: 0,
		files_queued: 0,
		files_total: 0,
		scan_complete: false,
		progress: 0.0,
		kept: 0,
		duplicates: 0,
		open_issues: 0,
		saved_bytes: 0,
		saved_fraction: 0.0,
		savings_history: initial_history(rng, 60, 400),
		eta: None,
		parity: None,
		originals_deleted: 0,
	}
}

fn initial_history(rng: &mut Rng, low: u64, high: u64) -> Vec<u64> {
	(0..HISTORY_LEN).map(|_| rng.range_u64(low, high)).collect()
}

impl MediaKind {
	fn codec(self) -> Option<&'static str> {
		match self {
			Self::Photo | Self::Screenshot => Some("AVIF"),
			Self::TallPhoto => Some("JXL"),
			Self::Video => Some("AV1"),
			Self::Audio => Some("Opus"),
			Self::Document => None,
		}
	}

	fn is_video(self) -> bool {
		self == Self::Video
	}

	fn input_size(self, rng: &mut Rng) -> u64 {
		match self {
			Self::Photo => rng.range_u64(5_800_000, 15_400_000),
			Self::TallPhoto => rng.range_u64(20_000_000, 40_000_000),
			Self::Video => rng.range_u64(48_000_000, 4_100_000_000),
			Self::Audio => rng.range_u64(2_800_000, 12_600_000),
			Self::Screenshot => rng.range_u64(900_000, 6_200_000),
			Self::Document => rng.range_u64(80_000, 3_400_000),
		}
	}

	fn output_size(self, bytes_in: u64, rng: &mut Rng) -> u64 {
		let ratio = match self {
			Self::Photo => rng.range_f64(0.14, 0.32),
			Self::TallPhoto => rng.range_f64(0.35, 0.45),
			Self::Video => rng.range_f64(0.22, 0.48),
			Self::Audio => rng.range_f64(0.38, 0.52),
			Self::Screenshot => rng.range_f64(0.18, 0.36),
			Self::Document => 1.0,
		};
		crate::format::round_to_u64(crate::format::as_f64(bytes_in) * ratio)
	}
}
