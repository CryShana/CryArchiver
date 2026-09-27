//! Watching the source (project layer spec §6.3 and §6.4): watcher events are hints that re-list a folder after a
//! short quiet period, the safety pass re-lists only folders whose modification time changed, and the ready rule
//! keeps files that are still being written out of the queue.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use cryarchiver_engine::detect::{MediaClass as Detected, classify_header};
use cryarchiver_engine::outcome::is_sharing_violation;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

use crate::keys::RelPath;
use crate::record::{MediaClass, mtime_ns};
use crate::references::MAX_REFERENCE_BYTES;

const HEADER_BYTES: u64 = 8192;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hint {
	Folder(RelPath),
	/// The watcher lost events: only a full reconcile is safe.
	Rescan,
}

pub struct SourceWatcher {
	_watcher: RecommendedWatcher,
}

/// Starts watching the whole source tree. Each event becomes a hint for the folder whose contents changed.
pub fn watch_source(root: &Path, on_hint: impl Fn(Hint) + Send + 'static) -> notify::Result<SourceWatcher> {
	let base: PathBuf = root.to_path_buf();
	let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| match result {
		Ok(event) if event.need_rescan() => on_hint(Hint::Rescan),
		Ok(event) => {
			for path in &event.paths {
				let folder = path
					.parent()
					.and_then(|parent| parent.strip_prefix(&base).ok())
					.and_then(RelPath::from_path);
				if let Some(folder) = folder {
					on_hint(Hint::Folder(folder));
				}
			}
		}
		Err(_) => on_hint(Hint::Rescan),
	})?;

	watcher.watch(root, RecursiveMode::Recursive)?;
	Ok(SourceWatcher { _watcher: watcher })
}

/// Collects folder hints until each folder has been quiet for the delay.
#[derive(Debug)]
pub struct Debouncer {
	delay: Duration,
	pending: HashMap<RelPath, Instant>,
}

impl Debouncer {
	pub fn new(delay: Duration) -> Self {
		Self {
			delay,
			pending: HashMap::new(),
		}
	}

	pub fn hint(&mut self, folder: RelPath, now: Instant) {
		self.pending.insert(folder, now);
	}

	pub fn due(&mut self, now: Instant) -> Vec<RelPath> {
		let mut due: Vec<RelPath> = self
			.pending
			.iter()
			.filter(|(_, last)| now.saturating_duration_since(**last) >= self.delay)
			.map(|(folder, _)| folder.clone())
			.collect();
		for folder in &due {
			self.pending.remove(folder);
		}
		due.sort();
		due
	}

	pub fn is_empty(&self) -> bool {
		self.pending.is_empty()
	}
}

/// A discovered file has been still for the stability window: either it was watched that long without a change,
/// or its modification time is already older than the window.
pub fn is_stable(modified_ns: i64, observed_since: Instant, now: Instant, window: Duration) -> bool {
	let watched = now.saturating_duration_since(observed_since) >= window;
	let untouched =
		mtime_ns(SystemTime::now()).saturating_sub(modified_ns) >= i64::try_from(window.as_nanos()).unwrap_or(i64::MAX);
	watched || untouched
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadyCheck {
	Ready {
		class: MediaClass,
		/// The whole file, for reference files small enough to rewrite.
		content: Option<Vec<u8>>,
	},
	Changed {
		size: u64,
		mtime_ns: i64,
	},
	/// Another program still holds it open for writing.
	Busy,
	Unreadable(String),
}

/// The ready rule's last step: the file still has its recorded size and time, and (on Windows) it opens with a share
/// mode that denies other writers. Reads the header to tell media from other files.
pub fn check_ready(path: &Path, size: u64, modified_ns: i64, want_content: bool) -> ReadyCheck {
	let meta = match std::fs::metadata(path) {
		Ok(meta) => meta,
		Err(err) => return ReadyCheck::Unreadable(err.to_string()),
	};
	let now_ns = meta.modified().map(mtime_ns).unwrap_or_default();
	if meta.len() != size || now_ns != modified_ns {
		return ReadyCheck::Changed {
			size: meta.len(),
			mtime_ns: now_ns,
		};
	}

	let file = match open_denying_writers(path) {
		Ok(file) => file,
		Err(err) if is_sharing_violation(&err) => return ReadyCheck::Busy,
		Err(err) => return ReadyCheck::Unreadable(err.to_string()),
	};

	let limit = if want_content && size <= MAX_REFERENCE_BYTES {
		MAX_REFERENCE_BYTES
	} else {
		HEADER_BYTES
	};
	let mut bytes = Vec::new();
	if let Err(err) = file.take(limit).read_to_end(&mut bytes) {
		return ReadyCheck::Unreadable(err.to_string());
	}

	if u64::try_from(bytes.len()).ok() != Some(size.min(limit)) {
		let meta = std::fs::metadata(path).ok();
		return ReadyCheck::Changed {
			size: meta.as_ref().map_or(0, std::fs::Metadata::len),
			mtime_ns: meta
				.and_then(|meta| meta.modified().ok())
				.map(mtime_ns)
				.unwrap_or_default(),
		};
	}

	let class = media_class(size, &bytes);
	let content = (want_content && size <= MAX_REFERENCE_BYTES).then_some(bytes);
	ReadyCheck::Ready { class, content }
}

/// Files the header already proves aren't media are copied; everything else goes to the engine, which decides.
pub fn media_class(size: u64, header: &[u8]) -> MediaClass {
	match classify_header(size, header) {
		Some(Detected::Other(_)) => MediaClass::Other,
		_ => MediaClass::Media,
	}
}

/// The folders whose modification time differs from the recorded one; adding, removing or renaming a child changes
/// it. Blocking.
pub fn changed_folders(root: &Path, folders: &[(RelPath, Option<i64>)]) -> Vec<RelPath> {
	folders
		.iter()
		.filter(|(folder, recorded)| {
			let current = std::fs::metadata(folder.to_path(root))
				.and_then(|meta| meta.modified())
				.ok()
				.map(mtime_ns);
			current != *recorded
		})
		.map(|(folder, _)| folder.clone())
		.collect()
}

#[cfg(windows)]
fn open_denying_writers(path: &Path) -> io::Result<File> {
	use std::os::windows::fs::OpenOptionsExt;

	const FILE_SHARE_READ: u32 = 0x1;

	OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(path)
}

#[cfg(not(windows))]
fn open_denying_writers(path: &Path) -> io::Result<File> {
	OpenOptions::new().read(true).open(path)
}

#[cfg(test)]
mod tests {
	use std::ffi::OsStr;
	use std::fs;
	use std::sync::mpsc;

	use super::*;

	#[test]
	fn hints_wait_for_a_quiet_period() {
		let mut debouncer = Debouncer::new(Duration::from_secs(2));
		let start = Instant::now();
		let folder = RelPath::root().join(OsStr::new("Captures"));

		debouncer.hint(folder.clone(), start);
		debouncer.hint(folder.clone(), start + Duration::from_secs(1));

		assert!(debouncer.due(start + Duration::from_secs(2)).is_empty());
		assert_eq!(debouncer.due(start + Duration::from_secs(3)), [folder]);
		assert!(debouncer.is_empty());
	}

	#[test]
	fn old_files_are_stable_at_once_and_new_ones_after_the_window() {
		let window = Duration::from_secs(30);
		let now = Instant::now();
		let fresh = mtime_ns(SystemTime::now());
		let old = fresh - 60_000_000_000;

		assert!(is_stable(old, now, now, window));
		assert!(!is_stable(fresh, now, now, window));
		assert!(is_stable(fresh, now, now + window, window));
	}

	#[test]
	fn a_changed_file_restarts_the_window() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("capture.mp4");
		fs::write(&path, b"part").unwrap();
		let modified = mtime_ns(fs::metadata(&path).unwrap().modified().unwrap());

		assert_eq!(
			check_ready(&path, 2, modified, false),
			ReadyCheck::Changed {
				size: 4,
				mtime_ns: modified
			}
		);
	}

	#[test]
	fn ready_files_are_classified_from_their_header() {
		let dir = tempfile::tempdir().unwrap();
		let html = dir.path().join("page.html");
		fs::write(&html, b"<html><head><title>x</title></head></html>").unwrap();
		let meta = fs::metadata(&html).unwrap();

		let check = check_ready(&html, meta.len(), mtime_ns(meta.modified().unwrap()), false);

		assert_eq!(
			check,
			ReadyCheck::Ready {
				class: MediaClass::Other,
				content: None
			}
		);
		assert_eq!(media_class(0, b""), MediaClass::Other);
		assert_eq!(media_class(12, b"fLaC\0\0\0\x22\x10\x00\x10\x00"), MediaClass::Media);
	}

	#[test]
	fn small_reference_files_come_with_their_content() {
		let dir = tempfile::tempdir().unwrap();
		let cue = dir.path().join("a.cue");
		fs::write(&cue, b"FILE \"a.flac\" WAVE\n").unwrap();
		let meta = fs::metadata(&cue).unwrap();

		let check = check_ready(&cue, meta.len(), mtime_ns(meta.modified().unwrap()), true);

		assert!(
			matches!(check, ReadyCheck::Ready { content: Some(content), .. } if content == b"FILE \"a.flac\" WAVE\n")
		);
	}

	#[cfg(windows)]
	#[test]
	fn a_file_held_open_for_writing_is_busy() {
		use std::os::windows::fs::OpenOptionsExt;

		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("recording.mp4");
		let writer = OpenOptions::new()
			.create(true)
			.truncate(true)
			.write(true)
			.share_mode(0x1 | 0x2)
			.open(&path)
			.unwrap();
		let meta = fs::metadata(&path).unwrap();

		assert_eq!(
			check_ready(&path, meta.len(), mtime_ns(meta.modified().unwrap()), false),
			ReadyCheck::Busy
		);

		drop(writer);
		assert!(matches!(
			check_ready(&path, meta.len(), mtime_ns(meta.modified().unwrap()), false),
			ReadyCheck::Ready { .. }
		));
	}

	#[test]
	fn the_safety_pass_finds_folders_whose_time_changed() {
		let dir = tempfile::tempdir().unwrap();
		fs::create_dir(dir.path().join("same")).unwrap();
		fs::create_dir(dir.path().join("changed")).unwrap();
		let time = |name: &str| {
			Some(mtime_ns(
				fs::metadata(dir.path().join(name)).unwrap().modified().unwrap(),
			))
		};
		let same = RelPath::root().join(OsStr::new("same"));
		let changed = RelPath::root().join(OsStr::new("changed"));
		let recorded = vec![(same, time("same")), (changed.clone(), Some(0))];

		assert_eq!(changed_folders(dir.path(), &recorded), [changed]);
	}

	#[test]
	fn the_watcher_hints_at_the_folder_that_changed() {
		let dir = tempfile::tempdir().unwrap();
		let root = fs::canonicalize(dir.path()).unwrap();
		fs::create_dir(root.join("Captures")).unwrap();
		let (sender, receiver) = mpsc::channel();
		let _watcher = watch_source(&root, move |hint| {
			let _ = sender.send(hint);
		})
		.unwrap();

		fs::write(root.join("Captures").join("clip.mp4"), b"x").unwrap();

		let wanted = Hint::Folder(RelPath::root().join(OsStr::new("Captures")));
		let deadline = Instant::now() + Duration::from_secs(5);
		let mut seen = Vec::new();
		while Instant::now() < deadline {
			if let Ok(hint) = receiver.recv_timeout(Duration::from_millis(100)) {
				if hint == wanted {
					return;
				}
				seen.push(hint);
			}
		}
		panic!("no hint for Captures: {seen:?}");
	}
}
