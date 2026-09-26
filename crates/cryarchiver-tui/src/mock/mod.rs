//! A fake archive run for developing the TUI before the project layer exists. Seeded, so the same seed and the
//! same number of [`MockSource::step`] calls always produce the same view.

mod build;
mod details;
mod rng;
mod sim;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use self::rng::Rng;
use crate::source::{Action, ArchiveSource};
use crate::view::{ArchiveView, FileDetails, Header, Log, NodeId, Overall, RunState, SystemStats, Workers};

const TICK: Duration = Duration::from_millis(500);
const START_CLOCK: u32 = 14 * 3600 + 2 * 60 + 7;
const LOG_CAPACITY: usize = 500;
const HISTORY_LEN: usize = 60;
const SAVINGS_WINDOW: usize = 8;
const VIDEO_SLOTS: usize = 3;
const IMAGE_SLOTS: usize = 3;
const SCAN_BATCH_INDEX: u64 = 29_000;
const SCAN_EXTRA: u64 = 2_200;

pub struct MockSource {
	rng: Rng,
	tick: u64,
	view: ArchiveView,
	kinds: Vec<Option<MediaKind>>,
	showcase: BTreeMap<NodeId, Showcase>,
	queue: VecDeque<NodeId>,
	jobs: Vec<Job>,
	duplicate_of: BTreeMap<NodeId, NodeId>,
	excluded: BTreeSet<NodeId>,
	waiting: Vec<(NodeId, u64)>,
	scan_target: NodeId,
	scan_remaining: u64,
	scan_discovered: u64,
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
	file: NodeId,
	phase: Phase,
	fps: f64,
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
		let waiting = built
			.issues
			.iter()
			.filter(|issue| issue.state == crate::view::FileState::Waiting)
			.map(|issue| (issue.file, 90))
			.collect();

		let mut source = Self {
			view: ArchiveView {
				header: Header {
					source: r"D:\Archive\Family".to_owned(),
					destination: r"E:\Archive-AV1\Family".to_owned(),
					speed: "MODERATE".to_owned(),
					codecs: "LATEST".to_owned(),
					performance_limit: Some(50),
					run_state: RunState::Running,
				},
				overall: Overall {
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
					savings_history: initial_history(&mut rng, 60, 400),
					eta: None,
					parity: None,
				},
				workers: Workers {
					jobs: Vec::new(),
					slots: format!("{VIDEO_SLOTS} video + {IMAGE_SLOTS} image slots"),
				},
				system: SystemStats {
					cpu_percent: 46,
					cpu_history: initial_history(&mut rng, 38, 54),
					ram_used: 11_200_000_000,
					ram_total: 32_000_000_000,
					disk_label: "E:".to_owned(),
					disk_free: 1_200_000_000_000,
					low_priority: true,
					toolchain: "ffmpeg 9.0.1 · SVT-AV1 4.2".to_owned(),
				},
				library: built.library,
				log: Log::with_capacity(LOG_CAPACITY),
				issues: built.issues,
			},
			rng,
			tick: 0,
			kinds: built.kinds,
			showcase: built.showcase,
			queue: built.queue,
			jobs: built.jobs,
			duplicate_of: built.duplicate_of,
			excluded: BTreeSet::new(),
			waiting,
			scan_target: built.scan_target,
			scan_remaining: SCAN_EXTRA,
			scan_discovered: 0,
			recent_savings: VecDeque::new(),
			last_refresh: None,
		};

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

	/// Advances the simulation by one tick of half a second.
	pub fn step(&mut self) {
		let saved_before = self.view.overall.saved_bytes;
		self.tick += 1;
		self.update_system();

		if self.view.header.run_state == RunState::Running {
			self.discover();
			self.advance_jobs();
			self.retry_waiting();
			self.start_jobs();
		}

		self.summarize();
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

	fn details(&self, file: NodeId) -> Option<FileDetails> {
		details::build(self, file)
	}

	fn perform(&mut self, action: Action) {
		self.apply(action);
		self.summarize();
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
