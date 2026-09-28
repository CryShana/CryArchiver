//! The checks of the new project form, run again on every change. They only look at the file system and never write
//! to it; the one check that writes, [`probe_destination`], runs once, when Create is pressed.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use cryarchiver_engine::commit::free_space;
use cryarchiver_engine::text::display_path;
pub use cryarchiver_project::project::MAX_NAME_CHARS;
use cryarchiver_project::project::{ProjectError, check_new_folders};
use cryarchiver_project::project_file;
use cryarchiver_project::settings::{RootConflict, identity_conflict, may_create_in, planned_path, root_conflict};

pub const NOT_EMPTY: &str = "files already there will be listed as unknown and never touched";
#[cfg(windows)]
pub const MARKED_READ_ONLY: &str = "marked read-only; Create checks that it can be written";
const PROBE_PREFIX: &str = ".cryarchiver-probe-";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
	Valid,
	Warning(String),
	Invalid(String),
}

impl Check {
	pub fn is_valid(&self) -> bool {
		!matches!(self, Self::Invalid(_))
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Validation {
	pub source: Check,
	pub destination: Check,
	pub name: Check,
	/// The project file `create` will write, once the destination and the name are filled in.
	pub project_file: Option<PathBuf>,
	/// At the destination's nearest existing folder.
	pub free_space: Option<u64>,
}

impl Validation {
	pub fn passes(&self) -> bool {
		self.source.is_valid() && self.destination.is_valid() && self.name.is_valid()
	}
}

pub fn validate(source: &str, destination: &str, name: &str) -> Validation {
	let project_file = (!destination.is_empty() && !name.is_empty())
		.then(|| project_file(Path::new(destination), OsStr::new(name)).ok())
		.flatten();
	let free_space = (!destination.is_empty())
		.then(|| free_space(Path::new(destination)).ok())
		.flatten();

	Validation {
		source: check_source(source),
		destination: check_destination(source, destination),
		name: check_name(name, project_file.as_deref()),
		project_file,
		free_space,
	}
}

fn check_source(source: &str) -> Check {
	if source.is_empty() {
		return Check::Invalid("enter the folder to archive".into());
	}

	let path = Path::new(source);
	match fs::metadata(path) {
		Err(err) if err.kind() == io::ErrorKind::NotFound => return Check::Invalid("doesn't exist".into()),
		Err(err) => return Check::Invalid(format!("can't be read: {err}")),
		Ok(meta) if !meta.is_dir() => return Check::Invalid("is a file, not a folder".into()),
		Ok(_) => {}
	}

	match fs::read_dir(path).and_then(|mut entries| entries.next().transpose()) {
		Ok(_) => Check::Valid,
		Err(err) => Check::Invalid(format!("can't be listed: {err}")),
	}
}

fn check_destination(source: &str, destination: &str) -> Check {
	if destination.is_empty() {
		return Check::Invalid("enter the folder the archive goes to".into());
	}

	let path = Path::new(destination);
	if !source.is_empty() {
		match root_conflict(Path::new(source), path) {
			Ok(Some(conflict)) => return Check::Invalid(conflict.message().into()),
			Ok(None) => {}
			Err(err) => return Check::Invalid(err.to_string()),
		}
	}

	let (exists, verb) = match fs::metadata(path) {
		Ok(meta) if !meta.is_dir() => return Check::Invalid("is a file, not a folder".into()),
		Ok(_) => (true, "written"),
		Err(err) if matches!(err.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => (false, "created"),
		Err(err) => return Check::Invalid(format!("can't be read: {err}")),
	};

	let folder = match nearest_folder(path) {
		Ok(folder) => folder,
		Err(reason) => return Check::Invalid(format!("can't be created: {reason}")),
	};
	if let Err(err) = planned_path(path)
		.map_err(ProjectError::from)
		.and_then(|planned| check_new_folders(&planned))
	{
		return Check::Invalid(err.to_string());
	}
	let Ok(meta) = fs::metadata(&folder) else {
		return Check::Invalid(format!("can't be {verb}: {} can't be read", display_path(&folder)));
	};

	if !may_create_in(&folder) {
		return Check::Invalid(format!(
			"can't be {verb}: no permission to write in {}",
			display_path(&folder)
		));
	}

	if meta.permissions().readonly() {
		return read_only(verb, &folder);
	}

	if exists && has_entries(path) {
		return Check::Warning(NOT_EMPTY.into());
	}
	Check::Valid
}

/// A Unix folder without write permission can't take new entries. On Windows the read-only mark on a folder doesn't
/// stop new entries, and Explorer sets it on folders such as Pictures; it is only a warning, and Create probes.
#[cfg(windows)]
fn read_only(_: &str, _: &Path) -> Check {
	Check::Warning(MARKED_READ_ONLY.into())
}

#[cfg(not(windows))]
fn read_only(verb: &str, folder: &Path) -> Check {
	Check::Invalid(format!("can't be {verb}: {} is read-only", display_path(folder)))
}

/// The destination itself when it exists, otherwise its nearest existing ancestor, on its planned path; it must be a
/// folder.
fn nearest_folder(destination: &Path) -> Result<PathBuf, String> {
	let planned = planned_path(destination).map_err(|err| err.to_string())?;
	let Some(folder) = planned.ancestors().find(|dir| dir.exists()) else {
		return Err("no part of the path exists".into());
	};

	if !folder.is_dir() {
		return Err(format!("{} is a file, not a folder", display_path(folder)));
	}
	Ok(folder.to_path_buf())
}

fn check_name(name: &str, project_file: Option<&Path>) -> Check {
	if let Err(reason) = cryarchiver_project::check_name(OsStr::new(name)) {
		return Check::Invalid(reason);
	}

	match project_file {
		Some(file) if file.symlink_metadata().is_ok() => Check::Invalid(format!(
			"{} already exists in the destination",
			file.file_name().unwrap_or_default().to_string_lossy()
		)),
		_ => Check::Valid,
	}
}

fn has_entries(folder: &Path) -> bool {
	fs::read_dir(folder).is_ok_and(|mut entries| entries.next().is_some())
}

/// What the probe at Create found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
	Writable,
	/// Writing worked, but the probe folder couldn't be removed again; the text names it.
	Leftover(String),
	Failed(String),
}

/// The one check that writes, run when Create is pressed and every other check has passed: a probe folder with a
/// file inside, in the planned destination's nearest existing folder, then both removed. Some folders allow new
/// subfolders but not new files. Nothing is tried inside the source, which must exist.
pub fn probe_destination(source: &Path, destination: &Path) -> Probe {
	probe_with(source, destination, &remove_entry)
}

fn probe_with(source: &Path, destination: &Path, remove: &dyn Fn(&Path) -> io::Result<()>) -> Probe {
	let (Ok(source), Ok(folder)) = (fs::canonicalize(source), nearest_folder(destination)) else {
		return Probe::Failed("the folders changed; check them again".into());
	};

	let inside_source = fs::canonicalize(&folder).is_ok_and(|folder| folder.starts_with(&source))
		|| matches!(
			identity_conflict(&source, &folder),
			Some(RootConflict::Same | RootConflict::DestinationInsideSource)
		);
	if inside_source {
		return Probe::Failed("its nearest existing folder is inside the source".into());
	}

	let entry = folder.join(probe_name());
	if let Err(err) = fs::create_dir(&entry) {
		return Probe::Failed(err.to_string());
	}

	let inside = entry.join(probe_name());
	match create_file(&inside) {
		Ok(()) => match remove(&inside).and_then(|()| remove(&entry)) {
			Ok(()) => Probe::Writable,
			Err(err) => Probe::Leftover(format!(
				"the destination is writable, but the test folder {} couldn't be removed ({err}); please delete it",
				display_path(&entry)
			)),
		},
		Err(err) => match remove(&entry) {
			Ok(()) => Probe::Failed(err.to_string()),
			Err(_) => Probe::Failed(format!(
				"{err}; the test folder {} couldn't be removed, please delete it",
				display_path(&entry)
			)),
		},
	}
}

fn create_file(path: &Path) -> io::Result<()> {
	fs::OpenOptions::new().write(true).create_new(true).open(path).map(drop)
}

fn remove_entry(path: &Path) -> io::Result<()> {
	if path.is_dir() {
		fs::remove_dir(path)
	} else {
		fs::remove_file(path)
	}
}

fn probe_name() -> String {
	use std::sync::atomic::{AtomicU64, Ordering};

	static NEXT: AtomicU64 = AtomicU64::new(0);
	format!(
		"{PROBE_PREFIX}{}-{}",
		std::process::id(),
		NEXT.fetch_add(1, Ordering::Relaxed)
	)
}

#[cfg(test)]
mod tests {
	use std::fs;
	use std::time::SystemTime;

	use cryarchiver_project::settings::plain_path;

	use super::*;

	struct Folders {
		dir: tempfile::TempDir,
	}

	impl Folders {
		fn new() -> Self {
			let dir = tempfile::tempdir().unwrap();
			fs::create_dir(dir.path().join("photos")).unwrap();
			fs::write(dir.path().join("photos").join("a.jpg"), b"jpg").unwrap();
			Self { dir }
		}

		fn path(&self, relative: &str) -> String {
			relative
				.split('/')
				.fold(self.dir.path().to_path_buf(), |path, part| path.join(part))
				.display()
				.to_string()
		}

		fn validate(&self, source: &str, destination: &str, name: &str) -> Validation {
			validate(&self.path(source), &self.path(destination), name)
		}

		fn entries(&self) -> Vec<String> {
			let mut names: Vec<String> = fs::read_dir(self.dir.path())
				.unwrap()
				.map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
				.collect();
			names.sort();
			names
		}
	}

	/// A folder's entries and modification time: creating and removing an entry changes the time.
	fn state(folder: &Path) -> (usize, SystemTime) {
		(
			fs::read_dir(folder).unwrap().count(),
			fs::metadata(folder).unwrap().modified().unwrap(),
		)
	}

	fn invalid(check: &Check) -> String {
		match check {
			Check::Invalid(reason) => reason.clone(),
			other => panic!("expected a refusal, got {other:?}"),
		}
	}

	#[test]
	fn a_valid_form_passes_and_leaves_nothing_behind() {
		let folders = Folders::new();
		let before = state(folders.dir.path());

		let validation = folders.validate("photos", "archive", "Photos");

		assert!(validation.passes(), "{validation:?}");
		assert_eq!(validation.destination, Check::Valid);
		assert_eq!(
			validation.project_file.unwrap(),
			plain_path(fs::canonicalize(folders.dir.path()).unwrap())
				.join("archive")
				.join("Photos.cryarchive")
		);
		assert!(validation.free_space.is_some_and(|free| free > 0));
		assert_eq!(folders.entries(), ["photos"]);
		assert_eq!(state(folders.dir.path()), before);
	}

	#[test]
	fn the_source_must_be_a_folder_that_exists() {
		let folders = Folders::new();

		assert!(invalid(&folders.validate("missing", "archive", "a").source).contains("doesn't exist"));
		assert!(invalid(&folders.validate("photos/a.jpg", "archive", "a").source).contains("not a folder"));
		assert!(!validate("", &folders.path("archive"), "a").source.is_valid());
	}

	#[test]
	fn the_destination_must_be_apart_from_the_source_even_before_either_exists() {
		let folders = Folders::new();
		let reason = |source: &str, destination: &str| invalid(&folders.validate(source, destination, "a").destination);

		assert_eq!(
			reason("photos", "photos"),
			"the source and the destination are the same folder"
		);
		assert_eq!(
			reason("photos", "photos/archive"),
			"the destination is inside the source"
		);
		assert_eq!(
			reason("photos", "photos/new/../archive"),
			"the destination is inside the source"
		);
		assert_eq!(
			reason("new/photos", "new/photos/archive/deep"),
			"the destination is inside the source"
		);
		assert_eq!(
			reason("new/photos", "new/photos"),
			"the source and the destination are the same folder"
		);
		assert_eq!(reason("new/photos", "new"), "the source is inside the destination");
		assert_eq!(
			reason("photos/deeper", "photos"),
			"the source is inside the destination"
		);
		assert_eq!(folders.entries(), ["photos"]);
		assert_eq!(fs::read_dir(folders.path("photos")).unwrap().count(), 1);
	}

	#[test]
	fn validating_while_typing_never_writes() {
		let folders = Folders::new();
		fs::create_dir(folders.path("photos2")).unwrap();
		let (root, photos2) = (state(folders.dir.path()), state(Path::new(&folders.path("photos2"))));

		for source in ["", "photo", "photos/a.jpg", "missing/deeper", "photos"] {
			for destination in ["photos2/archive", "photos2", "new/archive"] {
				folders.validate(source, destination, "a");
			}
		}

		assert_eq!(state(folders.dir.path()), root);
		assert_eq!(state(Path::new(&folders.path("photos2"))), photos2);
	}

	#[test]
	fn a_half_typed_source_that_names_an_existing_sibling_gets_no_write() {
		let folders = Folders::new();
		let typing = folders.dir.path().join("photos 2024");
		fs::create_dir(&typing).unwrap();
		let before = state(&typing);

		let validation = folders.validate("photos", "photos 2024/archive", "a");

		assert_eq!(validation.destination, Check::Valid);
		assert_eq!(state(&typing), before);
		assert_eq!(state(Path::new(&folders.path("photos"))).0, 1);
	}

	#[test]
	fn the_destination_must_be_a_folder_that_can_be_written() {
		let folders = Folders::new();
		fs::write(folders.path("file"), b"").unwrap();

		assert!(invalid(&folders.validate("photos", "file", "a").destination).contains("not a folder"));
		assert!(invalid(&folders.validate("photos", "file/archive", "a").destination).contains("not a folder"));
		assert!(!validate(&folders.path("photos"), "", "a").destination.is_valid());
	}

	#[test]
	fn new_destination_folders_follow_the_name_rules() {
		let folders = Folders::new();
		let reason = |destination: &str| invalid(&folders.validate("photos", destination, "a").destination);

		assert!(reason("new/CON/archive").contains("reserved"));
		assert!(reason("new/Archive ").contains("end with"));
		assert!(reason("new/Archive.").contains("end with"));
		assert!(reason("new/a|b").contains("can't contain |"));
		assert_eq!(folders.validate("photos", "new/Archive", "a").destination, Check::Valid);
		assert_eq!(folders.entries(), ["photos"]);
	}

	#[test]
	fn the_probe_at_create_writes_only_outside_the_source_and_cleans_up() {
		let folders = Folders::new();
		let source = Path::new(&folders.path("photos")).to_path_buf();

		assert_eq!(
			probe_destination(&source, Path::new(&folders.path("new/archive"))),
			Probe::Writable
		);
		assert_eq!(
			probe_destination(&source, Path::new(&folders.path("photos/archive"))),
			Probe::Failed("its nearest existing folder is inside the source".into())
		);
		assert_eq!(folders.entries(), ["photos"]);
		assert_eq!(state(&source).0, 1);
	}

	#[test]
	fn a_probe_that_cannot_be_removed_is_a_warning_naming_it() {
		let folders = Folders::new();
		let source = Path::new(&folders.path("photos")).to_path_buf();
		let held = |_: &Path| Err(io::Error::other("held open"));

		let probe = probe_with(&source, Path::new(&folders.path("archive")), &held);

		let Probe::Leftover(text) = probe else {
			panic!("{probe:?}");
		};
		assert!(text.starts_with("the destination is writable"), "{text}");
		assert!(text.contains(PROBE_PREFIX), "{text}");
		let leftover: Vec<PathBuf> = fs::read_dir(folders.dir.path())
			.unwrap()
			.map(|entry| entry.unwrap().path())
			.filter(|path| path.to_string_lossy().contains(PROBE_PREFIX))
			.collect();
		assert_eq!(leftover.len(), 1);
		fs::remove_dir_all(&leftover[0]).unwrap();
		assert_eq!(folders.entries(), ["photos"]);
	}

	#[test]
	fn a_destination_that_holds_files_is_only_a_warning() {
		let folders = Folders::new();
		fs::create_dir(folders.path("archive")).unwrap();
		fs::write(folders.path("archive/old.txt"), b"old").unwrap();

		let validation = folders.validate("photos", "archive", "Photos");

		assert_eq!(validation.destination, Check::Warning(NOT_EMPTY.into()));
		assert!(validation.passes());
		assert_eq!(fs::read_dir(folders.path("archive")).unwrap().count(), 1);
	}

	#[test]
	fn names_windows_or_linux_cannot_hold_are_refused() {
		let folders = Folders::new();
		let name = |name: &str| folders.validate("photos", "archive", name).name;

		assert!(name("Photos 2026").is_valid());
		assert!(name("nul-free.v2").is_valid());
		assert!(invalid(&name("")).contains("enter"));
		for bad in [
			"a<b", "a>b", "a:b", "a\"b", "a/b", "a\\b", "a|b", "a?b", "a*b", "a\u{0}b", "a\u{1f}b",
		] {
			assert!(!name(bad).is_valid(), "{bad:?}");
		}
		assert!(invalid(&name("trailing.")).contains("end with"));
		assert!(invalid(&name("trailing ")).contains("end with"));
		assert!(!name(&"x".repeat(MAX_NAME_CHARS + 1)).is_valid());
		assert!(name(&"é".repeat(MAX_NAME_CHARS)).is_valid());
	}

	#[test]
	fn windows_reserved_names_are_refused_with_any_extension_and_case() {
		let folders = Folders::new();
		let name = |name: &str| folders.validate("photos", "archive", name).name;

		for reserved in [
			"CON",
			"prn",
			"Aux",
			"nul",
			"COM1",
			"com9",
			"LPT1",
			"lpt9",
			"con.backup",
			"NUL .x",
		] {
			assert!(invalid(&name(reserved)).contains("reserved"), "{reserved}");
		}
		assert!(name("CONSOLE").is_valid());
		assert!(name("COM10").is_valid());
	}

	#[test]
	fn an_existing_project_file_is_refused() {
		let folders = Folders::new();
		fs::create_dir(folders.path("archive")).unwrap();
		fs::write(folders.path("archive/Photos.cryarchive"), b"").unwrap();

		let validation = folders.validate("photos", "archive", "Photos");

		assert!(invalid(&validation.name).contains("already exists"));
		assert!(!validation.passes());
	}

	#[cfg(windows)]
	#[test]
	fn a_read_only_mark_on_a_windows_folder_is_only_a_warning() {
		let folders = Folders::new();
		let archive = folders.dir.path().join("archive");
		fs::create_dir(&archive).unwrap();
		let original = fs::metadata(&archive).unwrap().permissions();
		let mut marked = original.clone();
		marked.set_readonly(true);
		fs::set_permissions(&archive, marked).unwrap();

		let validation = folders.validate("photos", "archive", "a");
		fs::set_permissions(&archive, original).unwrap();

		assert_eq!(validation.destination, Check::Warning(MARKED_READ_ONLY.into()));
		assert!(validation.passes());
	}

	/// Takes rights away from a folder for the current user, and gives them back when dropped.
	struct Denied {
		path: PathBuf,
	}

	impl Denied {
		/// `None` when the rights still apply to this user (an administrator or root), so the test can't run.
		#[cfg(windows)]
		fn apply(path: &Path, rights: &str, took_effect: impl Fn() -> bool) -> Option<Self> {
			let user = std::env::var("USERNAME").unwrap();
			let status = std::process::Command::new("icacls")
				.arg(path)
				.args(["/deny", &format!("{user}:{rights}")])
				.stdout(std::process::Stdio::null())
				.status()
				.unwrap();
			assert!(status.success());
			let denied = Self {
				path: path.to_path_buf(),
			};
			took_effect().then_some(denied)
		}

		#[cfg(unix)]
		fn apply(path: &Path, mode: u32, took_effect: impl Fn() -> bool) -> Option<Self> {
			use std::os::unix::fs::PermissionsExt;

			fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
			let denied = Self {
				path: path.to_path_buf(),
			};
			took_effect().then_some(denied)
		}
	}

	impl Drop for Denied {
		#[cfg(windows)]
		fn drop(&mut self) {
			let user = std::env::var("USERNAME").unwrap();
			let _ = std::process::Command::new("icacls")
				.arg(&self.path)
				.args(["/remove:d", &user])
				.stdout(std::process::Stdio::null())
				.status();
		}

		#[cfg(unix)]
		fn drop(&mut self) {
			use std::os::unix::fs::PermissionsExt;

			let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(0o755));
		}
	}

	#[cfg(windows)]
	const NO_LISTING: &str = "(RD)";
	#[cfg(unix)]
	const NO_LISTING: u32 = 0o000;
	#[cfg(windows)]
	const NO_NEW_ENTRIES: &str = "(WD,AD)";
	#[cfg(unix)]
	const NO_NEW_ENTRIES: u32 = 0o555;

	#[test]
	fn a_source_that_cannot_be_listed_is_refused() {
		let folders = Folders::new();
		let locked = folders.dir.path().join("locked");
		fs::create_dir(&locked).unwrap();
		fs::write(locked.join("a.jpg"), b"jpg").unwrap();

		let Some(denied) = Denied::apply(&locked, NO_LISTING, || fs::read_dir(&locked).is_err()) else {
			eprintln!("skipped: the listing is still allowed for this user");
			return;
		};
		let validation = folders.validate("locked", "archive", "a");
		drop(denied);

		assert!(
			invalid(&validation.source).contains("can't be listed"),
			"{validation:?}"
		);
		assert!(!validation.passes());
	}

	#[test]
	fn a_destination_that_refuses_new_entries_fails_the_probe() {
		let folders = Folders::new();
		let archive = folders.dir.path().join("archive");
		fs::create_dir(&archive).unwrap();
		let attempt = archive.join("attempt");
		let source = Path::new(&folders.path("photos")).to_path_buf();

		let Some(denied) = Denied::apply(&archive, NO_NEW_ENTRIES, || {
			let refused = fs::create_dir(&attempt).is_err();
			let _ = fs::remove_dir(&attempt);
			refused
		}) else {
			eprintln!("skipped: new entries are still allowed for this user");
			return;
		};
		let existing = probe_destination(&source, &archive);
		let missing = probe_destination(&source, &archive.join("new"));
		let live = folders.validate("photos", "archive", "a");
		drop(denied);

		assert!(matches!(existing, Probe::Failed(_)), "{existing:?}");
		assert!(matches!(missing, Probe::Failed(_)), "{missing:?}");
		if cfg!(unix) {
			assert!(invalid(&live.destination).contains("can't be written"), "{live:?}");
		}
		assert_eq!(fs::read_dir(&archive).unwrap().count(), 0);
	}
}
