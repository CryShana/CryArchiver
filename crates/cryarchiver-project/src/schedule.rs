//! Scheduling decisions (project layer spec §8): the governor, the queue in discovery order, dependency waits,
//! automatic retries, the order of cancel-and-requeue, and the run-wide low-disk pause. Everything here is pure; the
//! project task feeds it the facts.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::tree::FileId;

pub const MAX_DISK_JOBS: usize = 2;
pub const AUTO_RETRIES: u32 = 2;
pub const MAX_WAITING_VIDEOS: usize = 2;

const IDLE_SHARE: f32 = 0.20;
const VIDEO_EXTENSIONS: &[&str] = &[
	"mp4", "mov", "m4v", "mkv", "webm", "avi", "mts", "m2ts", "3gp", "wmv", "flv", "mpg", "mpeg", "ts", "vob", "ogv",
	"asf", "iso",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Work {
	Encode,
	Copy,
	Hash,
	Placeholder,
	Delete,
}

impl Work {
	/// Copies, hashing and the read-back checks before a delete.
	pub fn disk_bound(self) -> bool {
		matches!(self, Self::Copy | Self::Hash | Self::Delete)
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GovernorConfig {
	/// How long the CPU must stay more than 20 % idle, and how long since the last start, before another encode.
	pub ramp: Duration,
	pub max_encodes: usize,
}

/// Normal mode starts encodes while the CPU has room and keeps disk-bound jobs to two; limit mode runs one job of
/// any kind at a time.
#[derive(Debug)]
pub struct Governor {
	config: GovernorConfig,
	limit: bool,
	running: HashMap<Work, usize>,
	videos_waiting: usize,
	idle_since: Option<Instant>,
	last_start: Option<Instant>,
}

impl Governor {
	pub fn new(config: GovernorConfig) -> Self {
		Self {
			config,
			limit: false,
			running: HashMap::new(),
			videos_waiting: 0,
			idle_since: None,
			last_start: None,
		}
	}

	pub fn set_limit(&mut self, limit: bool) {
		self.limit = limit;
	}

	/// Limit mode: one job of any kind at a time.
	pub fn limited(&self) -> bool {
		self.limit
	}

	/// `busy` is the share of the whole machine's CPU in use, from 0 to 1.
	pub fn observe_cpu(&mut self, now: Instant, busy: f32) {
		if busy < 1.0 - IDLE_SHARE {
			self.idle_since.get_or_insert(now);
		} else {
			self.idle_since = None;
		}
	}

	pub fn may_start(&self, work: Work, now: Instant) -> bool {
		if self.limit {
			return self.total() == 0;
		}

		match work {
			Work::Copy | Work::Hash | Work::Delete => self.disk_jobs() < MAX_DISK_JOBS,
			Work::Placeholder => true,
			Work::Encode => {
				if self.videos_waiting >= MAX_WAITING_VIDEOS {
					return false;
				}

				let busy = self.busy_encodes();
				if busy == 0 {
					return true;
				}

				let ramped = |since: Instant| now.saturating_duration_since(since) >= self.config.ramp;
				busy < self.config.max_encodes
					&& self.idle_since.is_some_and(ramped)
					&& self.last_start.is_none_or(ramped)
			}
		}
	}

	pub fn started(&mut self, work: Work, now: Instant) {
		*self.running.entry(work).or_default() += 1;
		if work == Work::Encode {
			self.last_start = Some(now);
		}
	}

	pub fn finished(&mut self, work: Work) {
		if let Some(count) = self.running.get_mut(&work) {
			*count = count.saturating_sub(1);
		}
	}

	/// A video that waits for the engine's video slot doesn't count against the parallel budget, but no encode starts
	/// while [`MAX_WAITING_VIDEOS`] wait.
	pub fn set_video_waiting(&mut self, waiting: bool) {
		if waiting {
			self.videos_waiting += 1;
		} else {
			self.videos_waiting = self.videos_waiting.saturating_sub(1);
		}
	}

	pub fn total(&self) -> usize {
		self.running.values().sum()
	}

	pub fn videos_waiting(&self) -> usize {
		self.videos_waiting
	}

	/// How many running jobs the current limit wouldn't allow.
	pub fn over_limit(&self) -> usize {
		if self.limit { self.total().saturating_sub(1) } else { 0 }
	}

	fn disk_jobs(&self) -> usize {
		self.running
			.iter()
			.filter(|(work, _)| work.disk_bound())
			.map(|(_, count)| count)
			.sum()
	}

	fn busy_encodes(&self) -> usize {
		let encodes = self.running.get(&Work::Encode).copied().unwrap_or(0);
		let discount = self.videos_waiting.min(self.config.max_encodes);
		encodes.saturating_sub(discount)
	}
}

/// A guess from the name alone, which only orders the encodes: the engine detects the content.
pub fn looks_like_video(name: &OsStr) -> bool {
	Path::new(name)
		.extension()
		.and_then(OsStr::to_str)
		.is_some_and(|extension| VIDEO_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()))
}

/// Files ready to run, in discovery order.
#[derive(Debug, Default)]
pub struct Queue {
	order: VecDeque<FileId>,
	members: HashSet<FileId>,
	pushed: u64,
}

impl Queue {
	pub fn push(&mut self, file: FileId) {
		if self.members.insert(file) {
			self.order.push_back(file);
			self.pushed += 1;
		}
	}

	/// Requeued work goes first, so a restart resumes where it was.
	pub fn push_front(&mut self, file: FileId) {
		if self.members.insert(file) {
			self.order.push_front(file);
			self.pushed += 1;
		}
	}

	/// How many files were ever queued; it changes only when one is.
	pub fn pushed(&self) -> u64 {
		self.pushed
	}

	/// Takes the first queued file `wanted` accepts, leaving the others in their order.
	pub fn take_first(&mut self, wanted: impl Fn(FileId) -> bool) -> Option<FileId> {
		let index = self.order.iter().position(|file| wanted(*file))?;
		let file = self.order.remove(index)?;
		self.members.remove(&file);
		Some(file)
	}

	pub fn pop_front(&mut self) -> Option<FileId> {
		let file = self.order.pop_front()?;
		self.members.remove(&file);
		Some(file)
	}

	pub fn remove(&mut self, file: FileId) {
		if self.members.remove(&file) {
			self.order.retain(|queued| *queued != file);
		}
	}

	pub fn contains(&self, file: FileId) -> bool {
		self.members.contains(&file)
	}

	pub fn len(&self) -> usize {
		self.order.len()
	}

	pub fn is_empty(&self) -> bool {
		self.order.is_empty()
	}

	pub fn iter(&self) -> impl Iterator<Item = FileId> + '_ {
		self.order.iter().copied()
	}
}

/// Files that wait for other files to be done: duplicates for their canonical copy, reference files for the files
/// they name.
#[derive(Debug, Default)]
pub struct Waits {
	waiting_on: HashMap<FileId, HashSet<FileId>>,
	dependents: HashMap<FileId, HashSet<FileId>>,
}

impl Waits {
	pub fn wait(&mut self, waiter: FileId, dependencies: impl IntoIterator<Item = FileId>) {
		for dependency in dependencies {
			if dependency == waiter {
				continue;
			}
			self.waiting_on.entry(waiter).or_default().insert(dependency);
			self.dependents.entry(dependency).or_default().insert(waiter);
		}
	}

	pub fn is_waiting(&self, waiter: FileId) -> bool {
		self.waiting_on.contains_key(&waiter)
	}

	/// Whether `waiter` waits for `dependency`, directly or through the files it waits for.
	pub fn waits_for(&self, waiter: FileId, dependency: FileId) -> bool {
		let mut seen = HashSet::new();
		let mut next = vec![waiter];
		while let Some(file) = next.pop() {
			for &awaited in self.waiting_on.get(&file).into_iter().flatten() {
				if awaited == dependency {
					return true;
				}

				if seen.insert(awaited) {
					next.push(awaited);
				}
			}
		}
		false
	}

	/// Marks `dependency` done and returns the waiters that no longer wait for anything.
	pub fn settled(&mut self, dependency: FileId) -> Vec<FileId> {
		let mut released = Vec::new();
		for waiter in self.dependents.remove(&dependency).unwrap_or_default() {
			if let Some(remaining) = self.waiting_on.get_mut(&waiter) {
				remaining.remove(&dependency);
				if remaining.is_empty() {
					self.waiting_on.remove(&waiter);
					released.push(waiter);
				}
			}
		}
		released.sort();
		released
	}

	pub fn forget(&mut self, waiter: FileId) {
		for dependency in self.waiting_on.remove(&waiter).unwrap_or_default() {
			if let Some(waiters) = self.dependents.get_mut(&dependency) {
				waiters.remove(&waiter);
			}
		}
	}
}

/// The delay before an automatic retry, growing with each one; `None` once the retries are used up.
pub fn retry_delay(base: Duration, retries_so_far: u32) -> Option<Duration> {
	(retries_so_far < AUTO_RETRIES).then(|| base * 2u32.pow(retries_so_far))
}

/// The jobs to cancel and requeue so that only `keep` run, least progress first (project layer spec §8.4).
pub fn least_progress_first(running: &[(FileId, f64)], keep: usize) -> Vec<FileId> {
	let mut ordered = running.to_vec();
	ordered.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
	ordered
		.iter()
		.take(running.len().saturating_sub(keep))
		.map(|(file, _)| *file)
		.collect()
}

/// The run-wide pause while the destination is short of space (project layer spec §8.5).
#[derive(Debug, Default)]
pub struct DiskGuard {
	need: Option<u64>,
}

impl DiskGuard {
	pub fn pause(&mut self, need: u64) {
		self.need = Some(self.need.map_or(need, |known| known.max(need)));
	}

	pub fn is_paused(&self) -> bool {
		self.need.is_some()
	}

	/// Resumes once `free` covers what the paused job needed; returns whether it resumed.
	pub fn check(&mut self, free: u64) -> bool {
		if self.need.is_some_and(|need| free >= need) {
			self.need = None;
			return true;
		}
		false
	}
}

/// The space the rest of the archive is expected to take: remaining source bytes times the observed ratio.
#[allow(
	clippy::cast_precision_loss,
	clippy::cast_possible_truncation,
	clippy::cast_sign_loss
)]
pub fn space_estimate(remaining_source: u64, bytes_in: u64, bytes_out: u64) -> u64 {
	let ratio = if bytes_in == 0 {
		1.0
	} else {
		bytes_out as f64 / bytes_in as f64
	};
	(remaining_source as f64 * ratio.min(1.0)) as u64
}

#[cfg(test)]
mod tests {
	use std::ffi::OsStr;

	use super::*;

	const CONFIG: GovernorConfig = GovernorConfig {
		ramp: Duration::from_secs(3),
		max_encodes: 8,
	};

	#[test]
	fn the_first_encode_starts_at_once_and_the_next_waits_for_idle_cpu() {
		let mut governor = Governor::new(CONFIG);
		let start = Instant::now();

		assert!(governor.may_start(Work::Encode, start));
		governor.started(Work::Encode, start);
		assert!(!governor.may_start(Work::Encode, start));

		governor.observe_cpu(start, 0.95);
		assert!(!governor.may_start(Work::Encode, start + Duration::from_secs(10)));

		governor.observe_cpu(start + Duration::from_secs(10), 0.5);
		assert!(!governor.may_start(Work::Encode, start + Duration::from_secs(11)));
		assert!(governor.may_start(Work::Encode, start + Duration::from_secs(13)));
	}

	#[test]
	fn a_video_waiting_for_its_slot_does_not_count() {
		let mut governor = Governor::new(CONFIG);
		let now = Instant::now();
		governor.started(Work::Encode, now);

		governor.set_video_waiting(true);

		assert!(governor.may_start(Work::Encode, now));
		governor.set_video_waiting(false);
		assert!(!governor.may_start(Work::Encode, now));
	}

	#[test]
	fn no_encode_starts_while_two_videos_wait_for_a_slot() {
		let mut governor = Governor::new(CONFIG);
		let start = Instant::now();
		let later = start + Duration::from_secs(10);
		governor.observe_cpu(start, 0.1);
		for _ in 0..3 {
			governor.started(Work::Encode, start);
		}

		governor.set_video_waiting(true);
		governor.set_video_waiting(true);
		assert!(!governor.may_start(Work::Encode, later));

		governor.set_video_waiting(false);
		assert!(governor.may_start(Work::Encode, later));
		assert!(!governor.may_start(Work::Encode, start));
	}

	#[test]
	fn two_waiting_videos_hold_back_even_the_first_running_encode() {
		let mut governor = Governor::new(CONFIG);
		let now = Instant::now();
		governor.started(Work::Encode, now);
		governor.started(Work::Encode, now);

		governor.set_video_waiting(true);
		governor.set_video_waiting(true);

		assert!(!governor.may_start(Work::Encode, now));
		assert!(governor.may_start(Work::Copy, now));
	}

	#[test]
	fn at_most_two_disk_jobs_run() {
		let mut governor = Governor::new(CONFIG);
		let now = Instant::now();
		governor.started(Work::Copy, now);
		governor.started(Work::Hash, now);

		assert!(!governor.may_start(Work::Delete, now));
		assert!(governor.may_start(Work::Placeholder, now));
		assert!(governor.may_start(Work::Encode, now));

		governor.finished(Work::Hash);
		assert!(governor.may_start(Work::Copy, now));
	}

	#[test]
	fn limit_mode_runs_one_job_of_any_kind() {
		let mut governor = Governor::new(CONFIG);
		let now = Instant::now();
		governor.started(Work::Encode, now);
		governor.started(Work::Copy, now);

		governor.set_limit(true);

		assert!(!governor.may_start(Work::Placeholder, now));
		assert_eq!(governor.over_limit(), 1);
		governor.finished(Work::Encode);
		governor.finished(Work::Copy);
		assert!(governor.may_start(Work::Copy, now));
	}

	#[test]
	fn requeued_work_goes_first_and_nothing_is_queued_twice() {
		let mut queue = Queue::default();
		queue.push(FileId(1));
		queue.push(FileId(2));
		queue.push(FileId(1));
		queue.push_front(FileId(3));

		assert_eq!(queue.iter().collect::<Vec<_>>(), [FileId(3), FileId(1), FileId(2)]);
		queue.remove(FileId(1));
		assert!(!queue.contains(FileId(1)));
		assert_eq!(queue.len(), 2);
		assert_eq!(queue.pop_front(), Some(FileId(3)));
		assert!(!queue.contains(FileId(3)));
	}

	#[test]
	fn the_first_wanted_file_is_taken_and_the_rest_keep_their_order() {
		let mut queue = Queue::default();
		for file in 1..=4 {
			queue.push(FileId(file));
		}
		let pushed = queue.pushed();

		assert_eq!(queue.take_first(|file| file.0 % 2 == 0), Some(FileId(2)));
		assert_eq!(queue.take_first(|file| file.0 > 9), None);
		assert!(!queue.contains(FileId(2)));
		assert_eq!(queue.iter().collect::<Vec<_>>(), [FileId(1), FileId(3), FileId(4)]);
		assert_eq!(queue.pushed(), pushed, "taking queues nothing");

		queue.push_front(FileId(2));
		queue.push(FileId(3));
		assert_eq!(queue.pushed(), pushed + 1, "only a file not queued yet counts");
	}

	#[test]
	fn videos_are_guessed_from_the_extension_alone() {
		for name in [
			"clip.mp4",
			"CLIP.MKV",
			"show.m2ts",
			"home.vob",
			"a.b.webm",
			"film.ogv",
			"old.ASF",
			"Film.iso",
		] {
			assert!(looks_like_video(OsStr::new(name)), "{name}");
		}
		for name in ["photo.jpg", "song.flac", "mp4", "notes.txt", "song.m4a"] {
			assert!(!looks_like_video(OsStr::new(name)), "{name}");
		}
	}

	#[test]
	fn a_waiter_is_released_when_all_its_dependencies_are_done() {
		let mut waits = Waits::default();
		let cue = FileId(1);
		waits.wait(cue, [FileId(2), FileId(3)]);

		assert!(waits.settled(FileId(2)).is_empty());
		assert!(waits.is_waiting(cue));
		assert_eq!(waits.settled(FileId(3)), [cue]);
		assert!(!waits.is_waiting(cue));
	}

	#[test]
	fn forgetting_a_waiter_drops_its_dependencies() {
		let mut waits = Waits::default();
		waits.wait(FileId(1), [FileId(2)]);

		waits.forget(FileId(1));

		assert!(waits.settled(FileId(2)).is_empty());
	}

	#[test]
	fn a_wait_is_followed_through_the_files_it_waits_for() {
		let mut waits = Waits::default();
		waits.wait(FileId(1), [FileId(2)]);
		waits.wait(FileId(2), [FileId(3)]);
		waits.wait(FileId(3), [FileId(2)]);

		assert!(waits.waits_for(FileId(1), FileId(3)));
		assert!(!waits.waits_for(FileId(3), FileId(1)));
		assert!(!waits.waits_for(FileId(4), FileId(1)));
	}

	#[test]
	fn retries_back_off_and_stop_after_two() {
		let base = Duration::from_secs(10);

		assert_eq!(retry_delay(base, 0), Some(Duration::from_secs(10)));
		assert_eq!(retry_delay(base, 1), Some(Duration::from_secs(20)));
		assert_eq!(retry_delay(base, 2), None);
	}

	#[test]
	fn restarts_cancel_the_least_progress_first() {
		let running = [(FileId(1), 0.9), (FileId(2), 0.1), (FileId(3), 0.5)];

		assert_eq!(least_progress_first(&running, 1), [FileId(2), FileId(3)]);
		assert!(least_progress_first(&running, 3).is_empty());
	}

	#[test]
	fn low_disk_pauses_until_the_space_is_back() {
		let mut guard = DiskGuard::default();
		guard.pause(1000);

		assert!(guard.is_paused());
		assert!(!guard.check(999));
		assert!(guard.check(1000));
		assert!(!guard.is_paused());
	}

	#[test]
	fn the_space_estimate_uses_the_observed_ratio() {
		assert_eq!(space_estimate(1000, 0, 0), 1000);
		assert_eq!(space_estimate(1000, 200, 50), 250);
		assert_eq!(space_estimate(1000, 100, 150), 1000);
	}
}
