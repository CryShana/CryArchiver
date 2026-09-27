//! The recent projects of the start menu, kept in `recent.bin`.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const FILE_NAME: &str = "recent.bin";
pub const CAPACITY: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recent {
	#[serde(with = "cryarchiver_project::keys::path_bytes")]
	pub path: PathBuf,
	pub opened_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Loaded {
	pub entries: Vec<Recent>,
	pub notice: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentStore {
	file: PathBuf,
}

impl RecentStore {
	pub fn locate() -> Self {
		let exe = std::env::current_exe().ok();
		let exe_dir = exe.as_deref().and_then(Path::parent);
		Self::locate_in(exe_dir, |name| std::env::var_os(name))
	}

	/// Next to the executable when a probe file can be created there, otherwise in the user's config folder.
	pub fn locate_in(exe_dir: Option<&Path>, env: impl Fn(&str) -> Option<OsString>) -> Self {
		if let Some(dir) = exe_dir.filter(|dir| writable(dir)) {
			return Self::at(dir.join(FILE_NAME));
		}

		let folder = config_dir(&env)
			.or_else(|| exe_dir.map(Path::to_path_buf))
			.unwrap_or_default();
		Self::at(folder.join(FILE_NAME))
	}

	pub fn at(file: PathBuf) -> Self {
		Self { file }
	}

	pub fn file(&self) -> &Path {
		&self.file
	}

	/// A file that can't be read or decoded gives an empty list and a notice; it stays as it is until the next write.
	pub fn load(&self) -> Loaded {
		match self.read() {
			Ok(entries) => Loaded {
				entries: tidy(entries),
				notice: None,
			},
			Err(err) => Loaded {
				entries: Vec::new(),
				notice: Some(format!(
					"the recent projects couldn't be read ({err}); the list starts empty"
				)),
			},
		}
	}

	pub fn record(&self, project: &Path, opened_unix_ms: i64) -> io::Result<Vec<Recent>> {
		let path = std::path::absolute(project)?;
		self.update(|entries| entries.push(Recent { path, opened_unix_ms }))
	}

	pub fn remove(&self, project: &Path) -> io::Result<Vec<Recent>> {
		self.update(|entries| entries.retain(|entry| !same_path(&entry.path, project)))
	}

	pub fn clear_missing(&self) -> io::Result<Vec<Recent>> {
		self.update(|entries| entries.retain(|entry| entry.path.exists()))
	}

	/// Re-reads the file first, so another instance's changes since this one loaded are kept.
	fn update(&self, change: impl FnOnce(&mut Vec<Recent>)) -> io::Result<Vec<Recent>> {
		let mut entries = self.read().unwrap_or_default();
		change(&mut entries);
		let entries = tidy(entries);

		self.write(&entries)?;
		Ok(entries)
	}

	fn read(&self) -> io::Result<Vec<Recent>> {
		let bytes = match std::fs::read(&self.file) {
			Ok(bytes) => bytes,
			Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
			Err(err) => return Err(err),
		};

		rmp_serde::from_slice(&bytes).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
	}

	fn write(&self, entries: &[Recent]) -> io::Result<()> {
		let folder = self.file.parent().unwrap_or(Path::new("."));
		std::fs::create_dir_all(folder)?;

		let bytes = rmp_serde::to_vec_named(entries).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
		let temp = folder.join(unique_name(FILE_NAME, "tmp"));
		let written = write_synced(&temp, &bytes).and_then(|()| std::fs::rename(&temp, &self.file));
		if written.is_err() {
			let _ = std::fs::remove_file(&temp);
		}
		written
	}
}

pub fn now_unix_ms() -> i64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map_or(0, |since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
}

/// Newest first, one entry per path with its newest time, at most [`CAPACITY`].
fn tidy(mut entries: Vec<Recent>) -> Vec<Recent> {
	entries.sort_by_key(|entry| std::cmp::Reverse(entry.opened_unix_ms));

	let mut kept: Vec<Recent> = Vec::with_capacity(CAPACITY);
	for entry in entries {
		if kept.len() < CAPACITY && !kept.iter().any(|seen| same_path(&seen.path, &entry.path)) {
			kept.push(entry);
		}
	}
	kept
}

/// Windows paths differ only by ASCII case when they name the same file; unpaired surrogates stay distinct.
fn same_path(a: &Path, b: &Path) -> bool {
	let (a, b) = (a.as_os_str().as_encoded_bytes(), b.as_os_str().as_encoded_bytes());
	if cfg!(windows) {
		a.eq_ignore_ascii_case(b)
	} else {
		a == b
	}
}

fn writable(dir: &Path) -> bool {
	let probe = dir.join(unique_name(".recent-probe", "tmp"));
	match std::fs::OpenOptions::new().write(true).create_new(true).open(&probe) {
		Ok(file) => {
			drop(file);
			let _ = std::fs::remove_file(&probe);
			true
		}
		Err(_) => false,
	}
}

fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
	use std::io::Write;

	let mut file = std::fs::File::create(path)?;
	file.write_all(bytes)?;
	file.sync_all()
}

fn unique_name(stem: &str, extension: &str) -> String {
	use std::sync::atomic::{AtomicU64, Ordering};

	static NEXT: AtomicU64 = AtomicU64::new(0);
	let serial = NEXT.fetch_add(1, Ordering::Relaxed);
	format!("{stem}.{}-{serial}.{extension}", std::process::id())
}

#[cfg(windows)]
fn config_dir(env: &impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
	env("APPDATA")
		.map(PathBuf::from)
		.filter(|dir| dir.is_absolute())
		.map(|dir| dir.join("CryArchiver"))
}

#[cfg(not(windows))]
fn config_dir(env: &impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
	let absolute = |name: &str| env(name).map(PathBuf::from).filter(|dir| dir.is_absolute());

	absolute("XDG_CONFIG_HOME")
		.or_else(|| absolute("HOME").map(|home| home.join(".config")))
		.map(|dir| dir.join("cryarchiver"))
}

#[cfg(test)]
mod tests {
	use std::fs;

	use super::*;

	fn store(dir: &Path) -> RecentStore {
		RecentStore::at(dir.join(FILE_NAME))
	}

	fn paths(entries: &[Recent]) -> Vec<PathBuf> {
		entries.iter().map(|entry| entry.path.clone()).collect()
	}

	#[test]
	fn a_missing_file_is_an_empty_list_without_a_notice() {
		let dir = tempfile::tempdir().unwrap();

		assert_eq!(store(dir.path()).load(), Loaded::default());
	}

	#[test]
	fn a_corrupt_file_is_an_empty_list_with_a_notice_until_the_next_write() {
		let dir = tempfile::tempdir().unwrap();
		let store = store(dir.path());
		fs::write(store.file(), b"\xc1 not messagepack").unwrap();

		let loaded = store.load();
		assert!(loaded.entries.is_empty());
		assert!(loaded.notice.is_some());
		assert_eq!(fs::read(store.file()).unwrap(), b"\xc1 not messagepack");

		store.record(&dir.path().join("a.cryarchive"), 5).unwrap();
		let loaded = store.load();
		assert_eq!(paths(&loaded.entries), [dir.path().join("a.cryarchive")]);
		assert_eq!(loaded.notice, None);
	}

	#[test]
	fn the_newest_ten_are_kept_newest_first_and_a_reopen_moves_up() {
		let dir = tempfile::tempdir().unwrap();
		let store = store(dir.path());
		let project = |index: i64| dir.path().join(format!("p{index}.cryarchive"));

		for index in 0..12 {
			store.record(&project(index), index * 1000).unwrap();
		}
		store.record(&project(5), 20_000).unwrap();

		let entries = store.load().entries;
		assert_eq!(entries.len(), CAPACITY);
		assert_eq!(
			entries[0],
			Recent {
				path: project(5),
				opened_unix_ms: 20_000
			}
		);
		assert_eq!(
			paths(&entries[1..]),
			[11, 10, 9, 8, 7, 6, 4, 3, 2].map(project).to_vec()
		);
	}

	#[test]
	fn a_record_keeps_the_newer_time_of_an_entry() {
		let dir = tempfile::tempdir().unwrap();
		let store = store(dir.path());
		let project = dir.path().join("a.cryarchive");

		store.record(&project, 9_000).unwrap();
		store.record(&project, 1_000).unwrap();

		assert_eq!(
			store.load().entries,
			[Recent {
				path: project,
				opened_unix_ms: 9_000
			}]
		);
	}

	#[test]
	fn two_writers_merge_instead_of_overwriting() {
		let dir = tempfile::tempdir().unwrap();
		let first = store(dir.path());
		let second = store(dir.path());
		let (a, b, c) = (
			dir.path().join("a.cryarchive"),
			dir.path().join("b.cryarchive"),
			dir.path().join("c.cryarchive"),
		);

		let before_second = first.load();
		first.record(&a, 1).unwrap();
		second.record(&b, 2).unwrap();
		first.record(&c, 3).unwrap();

		assert!(before_second.entries.is_empty());
		assert_eq!(paths(&first.load().entries), [c, b, a]);
		let leftovers: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
		assert_eq!(leftovers.len(), 1);
	}

	#[test]
	fn remove_and_clear_missing_edit_the_list() {
		let dir = tempfile::tempdir().unwrap();
		let store = store(dir.path());
		let (kept, removed, missing) = (
			dir.path().join("kept.cryarchive"),
			dir.path().join("removed.cryarchive"),
			dir.path().join("missing.cryarchive"),
		);
		fs::write(&kept, b"").unwrap();
		fs::write(&removed, b"").unwrap();
		for (index, path) in [&kept, &removed, &missing].into_iter().enumerate() {
			store.record(path, i64::try_from(index).unwrap()).unwrap();
		}

		assert_eq!(paths(&store.remove(&removed).unwrap()), [missing.clone(), kept.clone()]);
		assert_eq!(paths(&store.clear_missing().unwrap()), std::slice::from_ref(&kept));
		assert_eq!(paths(&store.load().entries), [kept]);
	}

	#[cfg(unix)]
	#[test]
	fn non_utf8_names_survive_the_round_trip() {
		use std::ffi::OsStr;
		use std::os::unix::ffi::OsStrExt;

		let dir = tempfile::tempdir().unwrap();
		let store = store(dir.path());
		let project = dir.path().join(OsStr::from_bytes(b"caf\xe9.cryarchive"));

		store.record(&project, 1).unwrap();

		assert_eq!(paths(&store.load().entries), [project]);
	}

	#[cfg(windows)]
	#[test]
	fn unpaired_surrogates_survive_the_round_trip() {
		use std::os::windows::ffi::OsStringExt;

		let dir = tempfile::tempdir().unwrap();
		let store = store(dir.path());
		let name = OsString::from_wide(&[u16::from(b'a'), 0xD800, u16::from(b'.'), u16::from(b'x')]);
		let project = dir.path().join(name);

		store.record(&project, 1).unwrap();

		assert_eq!(paths(&store.load().entries), [project]);
	}

	#[test]
	fn a_writable_exe_folder_holds_the_list() {
		let dir = tempfile::tempdir().unwrap();

		let store = RecentStore::locate_in(Some(dir.path()), |_| None);

		assert_eq!(store.file(), dir.path().join(FILE_NAME));
		assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
	}

	#[test]
	fn an_unwritable_exe_folder_falls_back_to_the_config_folder() {
		let dir = tempfile::tempdir().unwrap();
		let blocked = dir.path().join("file");
		fs::write(&blocked, b"").unwrap();
		let exe_dir = blocked.join("bin");
		let config = dir.path().join("config");
		let home = dir.path().join("home");
		let env = |name: &str| match name {
			"APPDATA" | "XDG_CONFIG_HOME" => Some(config.clone().into_os_string()),
			"HOME" => Some(home.clone().into_os_string()),
			_ => None,
		};

		let store = RecentStore::locate_in(Some(&exe_dir), env);

		if cfg!(windows) {
			assert_eq!(store.file(), config.join("CryArchiver").join(FILE_NAME));
		} else {
			assert_eq!(store.file(), config.join("cryarchiver").join(FILE_NAME));
		}
		store.record(&dir.path().join("a.cryarchive"), 1).unwrap();
		assert_eq!(store.load().entries.len(), 1);
	}

	#[cfg(unix)]
	#[test]
	fn without_xdg_config_home_the_list_goes_under_dot_config() {
		let dir = tempfile::tempdir().unwrap();
		let home = dir.path().join("home");
		let env = |name: &str| (name == "HOME").then(|| home.clone().into_os_string());

		let store = RecentStore::locate_in(Some(&dir.path().join("missing")), env);

		assert_eq!(store.file(), home.join(".config").join("cryarchiver").join(FILE_NAME));
	}
}
