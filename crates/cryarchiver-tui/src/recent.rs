//! The recent projects of the start menu, kept in `recent.bin`.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use cryarchiver_project::settings::{plain_path, planned_path};
use cryarchiver_project::store::{Presence, presence};
use serde::{Deserialize, Serialize};

pub const FILE_NAME: &str = "recent.bin";
pub const CAPACITY: usize = 10;

const DISABLED: &str = "the recent projects are off for this session: there is no writable folder to keep them in";

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
	/// `None` when there is nowhere to keep the list; it is then empty and never saved.
	file: Option<PathBuf>,
}

enum ReadError {
	/// The file was read but isn't a list; the next write may replace it.
	Corrupt(String),
	/// The file couldn't be read at all; it is left alone.
	Unreadable(io::Error),
}

impl RecentStore {
	pub fn locate() -> Self {
		let exe = std::env::current_exe().ok();
		let exe_dir = exe.as_deref().and_then(Path::parent);
		Self::locate_in(exe_dir, |name| std::env::var_os(name))
	}

	/// Next to the executable when a probe file can be created there, otherwise in the user's config folder. With
	/// neither, the list is off for the session rather than kept in the current folder.
	pub fn locate_in(exe_dir: Option<&Path>, env: impl Fn(&str) -> Option<OsString>) -> Self {
		if let Some(dir) = exe_dir.filter(|dir| writable(dir)) {
			return Self::at(dir.join(FILE_NAME));
		}

		Self {
			file: config_dir(&env).map(|folder| folder.join(FILE_NAME)),
		}
	}

	pub fn at(file: PathBuf) -> Self {
		Self { file: Some(file) }
	}

	pub fn file(&self) -> Option<&Path> {
		self.file.as_deref()
	}

	/// A file that can't be read or decoded gives an empty list and a notice; it stays as it is until the next write.
	pub fn load(&self) -> Loaded {
		if self.file.is_none() {
			return Loaded {
				entries: Vec::new(),
				notice: Some(DISABLED.to_owned()),
			};
		}

		match self.read() {
			Ok(entries) => Loaded {
				entries: tidy(entries),
				notice: None,
			},
			Err(ReadError::Corrupt(reason)) => Loaded {
				entries: Vec::new(),
				notice: Some(format!(
					"the recent projects couldn't be read ({reason}); the list starts empty"
				)),
			},
			Err(ReadError::Unreadable(err)) => Loaded {
				entries: Vec::new(),
				notice: Some(format!(
					"the recent projects couldn't be read ({err}); the list is left as it is"
				)),
			},
		}
	}

	pub fn record(&self, project: &Path, opened_unix_ms: i64) -> io::Result<Vec<Recent>> {
		let path = plain_path(planned_path(project)?);
		self.update(|entries| entries.push(Recent { path, opened_unix_ms }))
	}

	pub fn remove(&self, project: &Path) -> io::Result<Vec<Recent>> {
		self.update(|entries| entries.retain(|entry| !same_path(&entry.path, project)))
	}

	/// Drops only the projects whose file is gone from a folder that is still there. A project on a drive or share
	/// that is offline, or behind a folder that can't be reached, stays.
	pub fn clear_missing(&self) -> io::Result<Vec<Recent>> {
		self.update(|entries| entries.retain(|entry| presence(&entry.path) != Presence::Missing))
	}

	/// Re-reads the file first, so another instance's changes since this one loaded are kept. A file that couldn't
	/// be read is not replaced: the change is not saved.
	fn update(&self, change: impl FnOnce(&mut Vec<Recent>)) -> io::Result<Vec<Recent>> {
		let Some(file) = &self.file else {
			return Err(io::Error::other(DISABLED));
		};

		let mut entries = match self.read() {
			Ok(entries) => entries,
			Err(ReadError::Corrupt(_)) => Vec::new(),
			Err(ReadError::Unreadable(err)) => {
				return Err(io::Error::new(
					err.kind(),
					format!("{FILE_NAME} couldn't be read ({err}), so it was left as it is"),
				));
			}
		};
		change(&mut entries);
		let entries = tidy(entries);

		write(file, &entries)?;
		Ok(entries)
	}

	fn read(&self) -> Result<Vec<Recent>, ReadError> {
		let Some(file) = &self.file else {
			return Ok(Vec::new());
		};

		let bytes = match std::fs::read(file) {
			Ok(bytes) => bytes,
			Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
			Err(err) => return Err(ReadError::Unreadable(err)),
		};

		rmp_serde::from_slice(&bytes).map_err(|err| ReadError::Corrupt(err.to_string()))
	}
}

fn write(file: &Path, entries: &[Recent]) -> io::Result<()> {
	let folder = file.parent().unwrap_or(Path::new("."));
	std::fs::create_dir_all(folder)?;

	let bytes = rmp_serde::to_vec_named(entries).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
	let temp = folder.join(unique_name(FILE_NAME, "tmp"));
	let written = write_synced(&temp, &bytes).and_then(|()| std::fs::rename(&temp, file));
	if written.is_err() {
		let _ = std::fs::remove_file(&temp);
	}
	written?;

	#[cfg(unix)]
	std::fs::File::open(folder)?.sync_all()?;
	Ok(())
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

#[cfg(windows)]
fn same_path(a: &Path, b: &Path) -> bool {
	folded(a) == folded(b)
}

#[cfg(not(windows))]
fn same_path(a: &Path, b: &Path) -> bool {
	a == b
}

/// Windows compares names without case. The `\\?\` form is dropped first; an unpaired surrogate is kept as a value
/// no character can have, so it only ever equals itself.
#[cfg(windows)]
fn folded(path: &Path) -> Vec<u32> {
	use std::os::windows::ffi::OsStrExt;

	const VERBATIM_UNC: [u16; 8] = [92, 92, 63, 92, 85, 78, 67, 92];
	const VERBATIM: [u16; 4] = [92, 92, 63, 92];
	const UNPAIRED: u32 = 0x11_0000;

	let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
	let upper = |unit: u16| if (97..=122).contains(&unit) { unit - 32 } else { unit };
	let starts = |prefix: &[u16]| wide.len() >= prefix.len() && wide.iter().zip(prefix).all(|(&a, &b)| upper(a) == b);
	let plain: Vec<u16> = if starts(&VERBATIM_UNC) {
		[92, 92]
			.into_iter()
			.chain(wide[VERBATIM_UNC.len()..].iter().copied())
			.collect()
	} else if starts(&VERBATIM) {
		wide[VERBATIM.len()..].to_vec()
	} else {
		wide
	};

	char::decode_utf16(plain)
		.flat_map(|decoded| match decoded {
			Ok(c) => c.to_lowercase().map(u32::from).collect::<Vec<_>>(),
			Err(unpaired) => vec![UNPAIRED + u32::from(unpaired.unpaired_surrogate())],
		})
		.collect()
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

	/// What `record` keeps for a path in `dir`: the temp folder may be reached through a link or a short name.
	fn kept(dir: &Path, name: &str) -> PathBuf {
		plain_path(fs::canonicalize(dir).unwrap()).join(name)
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
		let file = store.file().unwrap().to_path_buf();
		fs::write(&file, b"\xc1 not messagepack").unwrap();

		let loaded = store.load();
		assert!(loaded.entries.is_empty());
		assert!(loaded.notice.is_some());
		assert_eq!(fs::read(&file).unwrap(), b"\xc1 not messagepack");

		store.record(&dir.path().join("a.cryarchive"), 5).unwrap();
		let loaded = store.load();
		assert_eq!(paths(&loaded.entries), [kept(dir.path(), "a.cryarchive")]);
		assert_eq!(loaded.notice, None);
	}

	#[test]
	fn the_newest_ten_are_kept_newest_first_and_a_reopen_moves_up() {
		let dir = tempfile::tempdir().unwrap();
		let store = store(dir.path());
		let project = |index: i64| dir.path().join(format!("p{index}.cryarchive"));
		let saved = |index: i64| kept(dir.path(), &format!("p{index}.cryarchive"));

		for index in 0..12 {
			store.record(&project(index), index * 1000).unwrap();
		}
		store.record(&project(5), 20_000).unwrap();

		let entries = store.load().entries;
		assert_eq!(entries.len(), CAPACITY);
		assert_eq!(
			entries[0],
			Recent {
				path: saved(5),
				opened_unix_ms: 20_000
			}
		);
		assert_eq!(paths(&entries[1..]), [11, 10, 9, 8, 7, 6, 4, 3, 2].map(saved).to_vec());
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
				path: kept(dir.path(), "a.cryarchive"),
				opened_unix_ms: 9_000
			}]
		);
	}

	#[test]
	fn two_writers_merge_instead_of_overwriting() {
		let dir = tempfile::tempdir().unwrap();
		let first = store(dir.path());
		let second = store(dir.path());

		let before_second = first.load();
		first.record(&dir.path().join("a.cryarchive"), 1).unwrap();
		second.record(&dir.path().join("b.cryarchive"), 2).unwrap();
		first.record(&dir.path().join("c.cryarchive"), 3).unwrap();

		assert!(before_second.entries.is_empty());
		assert_eq!(
			paths(&first.load().entries),
			["c", "b", "a"].map(|name| kept(dir.path(), &format!("{name}.cryarchive")))
		);
		let leftovers: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
		assert_eq!(leftovers.len(), 1);
	}

	#[test]
	fn remove_and_clear_missing_edit_the_list() {
		let dir = tempfile::tempdir().unwrap();
		let store = store(dir.path());
		let (kept_file, removed, missing) = (
			dir.path().join("kept.cryarchive"),
			dir.path().join("removed.cryarchive"),
			dir.path().join("missing.cryarchive"),
		);
		fs::write(&kept_file, b"").unwrap();
		fs::write(&removed, b"").unwrap();
		for (index, path) in [&kept_file, &removed, &missing].into_iter().enumerate() {
			store.record(path, i64::try_from(index).unwrap()).unwrap();
		}
		let (kept_file, missing) = (
			kept(dir.path(), "kept.cryarchive"),
			kept(dir.path(), "missing.cryarchive"),
		);

		assert_eq!(paths(&store.remove(&removed).unwrap()), [missing, kept_file.clone()]);
		assert_eq!(paths(&store.clear_missing().unwrap()), std::slice::from_ref(&kept_file));
		assert_eq!(paths(&store.load().entries), [kept_file]);
	}

	#[test]
	fn clear_missing_keeps_projects_that_are_only_out_of_reach() {
		let dir = tempfile::tempdir().unwrap();
		let store = store(dir.path());
		let offline = dir.path().join("unplugged").join("a.cryarchive");
		let folder = dir.path().join("folder.cryarchive");
		fs::create_dir(&folder).unwrap();
		store.record(&offline, 1).unwrap();
		store.record(&folder, 2).unwrap();
		store.record(&dir.path().join("gone.cryarchive"), 3).unwrap();

		let left = store.clear_missing().unwrap();

		assert_eq!(
			paths(&left),
			[
				kept(dir.path(), "folder.cryarchive"),
				kept(dir.path(), "unplugged").join("a.cryarchive")
			]
		);
	}

	#[test]
	fn the_same_file_is_listed_once_however_its_path_is_written() {
		let dir = tempfile::tempdir().unwrap();
		let store = store(dir.path());
		let project = dir.path().join("a.cryarchive");
		fs::write(&project, b"").unwrap();

		store.record(&project, 1).unwrap();
		store.record(&dir.path().join(".").join("a.cryarchive"), 2).unwrap();
		store.record(&fs::canonicalize(&project).unwrap(), 3).unwrap();

		assert_eq!(paths(&store.load().entries), [kept(dir.path(), "a.cryarchive")]);
	}

	#[cfg(windows)]
	#[test]
	fn windows_paths_match_across_case_and_the_verbatim_form() {
		let base = plain_path(fs::canonicalize(tempfile::tempdir().unwrap().path().parent().unwrap()).unwrap());
		let plain = base.join("Ärchiv").join("Fotos.cryarchive");
		let text = plain.display().to_string();

		assert!(same_path(&plain, &PathBuf::from(text.to_uppercase())));
		assert!(same_path(&plain, &PathBuf::from(text.to_lowercase())));
		assert!(same_path(&plain, &PathBuf::from(format!(r"\\?\{text}"))));
		assert!(same_path(
			Path::new(r"\\server\share\a.cryarchive"),
			Path::new(r"\\?\UNC\SERVER\share\A.cryarchive")
		));
		assert!(!same_path(&plain, &base.join("Archiv").join("Fotos.cryarchive")));
	}

	#[cfg(unix)]
	#[test]
	fn non_utf8_names_survive_the_round_trip() {
		use std::ffi::OsStr;
		use std::os::unix::ffi::OsStrExt;

		let dir = tempfile::tempdir().unwrap();
		let store = store(dir.path());
		let name = OsStr::from_bytes(b"caf\xe9.cryarchive");

		store.record(&dir.path().join(name), 1).unwrap();

		assert_eq!(
			paths(&store.load().entries),
			[plain_path(fs::canonicalize(dir.path()).unwrap()).join(name)]
		);
	}

	#[cfg(windows)]
	#[test]
	fn unpaired_surrogates_survive_the_round_trip() {
		use std::os::windows::ffi::OsStringExt;

		let dir = tempfile::tempdir().unwrap();
		let store = store(dir.path());
		let name = OsString::from_wide(&[u16::from(b'a'), 0xD800, u16::from(b'.'), u16::from(b'x')]);
		let other = OsString::from_wide(&[u16::from(b'a'), 0xD801, u16::from(b'.'), u16::from(b'x')]);

		store.record(&dir.path().join(&name), 1).unwrap();
		store.record(&dir.path().join(&other), 2).unwrap();

		let base = plain_path(fs::canonicalize(dir.path()).unwrap());
		assert_eq!(paths(&store.load().entries), [base.join(other), base.join(name)]);
	}

	#[cfg(windows)]
	#[test]
	fn a_list_that_cannot_be_read_is_left_as_it_is() {
		use std::os::windows::fs::OpenOptionsExt;

		const FILE_SHARE_DELETE: u32 = 4;

		let dir = tempfile::tempdir().unwrap();
		let store = store(dir.path());
		let file = store.file().unwrap().to_path_buf();
		store.record(&dir.path().join("a.cryarchive"), 1).unwrap();

		let holder = fs::OpenOptions::new()
			.read(true)
			.share_mode(FILE_SHARE_DELETE)
			.open(&file)
			.unwrap();
		let loaded = store.load();
		let recorded = store.record(&dir.path().join("b.cryarchive"), 2);
		drop(holder);

		assert!(loaded.entries.is_empty());
		assert!(loaded.notice.unwrap().contains("left as it is"));
		assert!(recorded.unwrap_err().to_string().contains("left as it is"));
		assert_eq!(paths(&store.load().entries), [kept(dir.path(), "a.cryarchive")]);
		store.record(&dir.path().join("b.cryarchive"), 2).unwrap();
		assert_eq!(store.load().entries.len(), 2);
	}

	#[cfg(unix)]
	#[test]
	fn a_list_that_cannot_be_read_is_left_as_it_is() {
		use std::os::unix::fs::PermissionsExt;

		let dir = tempfile::tempdir().unwrap();
		let store = store(dir.path());
		let file = store.file().unwrap().to_path_buf();
		store.record(&dir.path().join("a.cryarchive"), 1).unwrap();

		fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
		if fs::read(&file).is_ok() {
			fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
			eprintln!("skipped: permissions don't apply to this user");
			return;
		}
		let loaded = store.load();
		let recorded = store.record(&dir.path().join("b.cryarchive"), 2);
		fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();

		assert!(loaded.entries.is_empty());
		assert!(loaded.notice.unwrap().contains("left as it is"));
		assert!(recorded.is_err());
		assert_eq!(paths(&store.load().entries), [kept(dir.path(), "a.cryarchive")]);
	}

	#[test]
	fn a_writable_exe_folder_holds_the_list() {
		let dir = tempfile::tempdir().unwrap();

		let store = RecentStore::locate_in(Some(dir.path()), |_| None);

		assert_eq!(store.file(), Some(dir.path().join(FILE_NAME).as_path()));
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

		let folder = if cfg!(windows) { "CryArchiver" } else { "cryarchiver" };
		assert_eq!(store.file(), Some(config.join(folder).join(FILE_NAME).as_path()));
		store.record(&dir.path().join("a.cryarchive"), 1).unwrap();
		assert_eq!(store.load().entries.len(), 1);
	}

	#[test]
	fn without_a_writable_folder_the_list_is_off_for_the_session() {
		let dir = tempfile::tempdir().unwrap();
		let blocked = dir.path().join("file");
		fs::write(&blocked, b"").unwrap();

		let store = RecentStore::locate_in(Some(&blocked.join("bin")), |_| None);

		assert_eq!(store.file(), None);
		let loaded = store.load();
		assert!(loaded.entries.is_empty());
		assert!(loaded.notice.unwrap().contains("off for this session"));
		assert!(store.record(&dir.path().join("a.cryarchive"), 1).is_err());
		assert!(!Path::new(FILE_NAME).exists());
		assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
	}

	#[cfg(unix)]
	#[test]
	fn without_xdg_config_home_the_list_goes_under_dot_config() {
		let dir = tempfile::tempdir().unwrap();
		let home = dir.path().join("home");
		let env = |name: &str| (name == "HOME").then(|| home.clone().into_os_string());

		let store = RecentStore::locate_in(Some(&dir.path().join("missing")), env);

		assert_eq!(
			store.file(),
			Some(home.join(".config").join("cryarchiver").join(FILE_NAME).as_path())
		);
	}
}
