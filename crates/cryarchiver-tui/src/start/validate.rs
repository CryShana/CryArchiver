//! The checks of the new project form, run again on every change. Each one only looks at the file system, except the
//! destination probe: once the source checks out, it creates and removes one entry outside the source, once for each
//! source and destination.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use cryarchiver_engine::commit::free_space;
use cryarchiver_engine::naming::is_reserved_device_name;
use cryarchiver_engine::text::display_path;
use cryarchiver_project::project_file;
use cryarchiver_project::settings::{planned_path, root_conflict};

pub const MAX_NAME_CHARS: usize = 100;
const INVALID_CHARS: &str = "<>:\"/\\|?*";
pub const NOT_EMPTY: &str = "files already there will be listed as unknown and never touched";
pub const NOT_CHECKED: &str = "checked once the source is valid";
const PROBE_PREFIX: &str = ".cryarchiver-probe-";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
	Valid,
	Warning(String),
	/// Not checked yet: it depends on another field that doesn't pass.
	Pending(String),
	Invalid(String),
}

impl Check {
	pub fn is_valid(&self) -> bool {
		matches!(self, Self::Valid | Self::Warning(_))
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

/// The last destination probe, kept so that edits to other fields don't write again.
#[derive(Debug, Default)]
pub struct Probes {
	last: Option<(PathBuf, PathBuf, Probe)>,
	made: usize,
}

impl Probes {
	pub fn made(&self) -> usize {
		self.made
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Probe {
	Writable,
	/// Writing worked, but the probe entry couldn't be removed again.
	Leftover {
		path: PathBuf,
		reason: String,
	},
	Failed(String),
}

pub fn validate(source: &str, destination: &str, name: &str, probes: &mut Probes) -> Validation {
	let project_file = (!destination.is_empty() && !name.is_empty())
		.then(|| project_file(Path::new(destination), OsStr::new(name)).ok())
		.flatten();
	let free_space = (!destination.is_empty())
		.then(|| free_space(Path::new(destination)).ok())
		.flatten();
	let source_check = check_source(source);

	Validation {
		destination: check_destination(source, source_check.is_valid(), destination, probes),
		source: source_check,
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

/// The probe runs only for a source that checks out, so it never writes while the source is half typed.
fn check_destination(source: &str, source_valid: bool, destination: &str, probes: &mut Probes) -> Check {
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

	let exists = match fs::metadata(path) {
		Ok(meta) if !meta.is_dir() => return Check::Invalid("is a file, not a folder".into()),
		Ok(_) => true,
		Err(err) if err.kind() == io::ErrorKind::NotFound => false,
		Err(err) => return Check::Invalid(format!("can't be read: {err}")),
	};

	if !source_valid {
		return Check::Pending(NOT_CHECKED.into());
	}

	let (Ok(source), Ok(planned)) = (fs::canonicalize(source), planned_path(path)) else {
		return Check::Pending(NOT_CHECKED.into());
	};
	let probe = match &probes.last {
		Some((last_source, last_destination, probe)) if *last_source == source && *last_destination == planned => {
			probe.clone()
		}
		_ => {
			let probe = probe(&planned, exists, &source, &remove_entry);
			probes.made += 1;
			probes.last = Some((source, planned, probe.clone()));
			probe
		}
	};
	destination_check(&probe, exists, path)
}

fn destination_check(probe: &Probe, exists: bool, destination: &Path) -> Check {
	match probe {
		Probe::Failed(reason) if exists => Check::Invalid(format!("can't be written: {reason}")),
		Probe::Failed(reason) => Check::Invalid(format!("can't be created: {reason}")),
		Probe::Leftover { path, reason } => Check::Warning(format!(
			"writable, but the test entry {} couldn't be removed ({reason}); please delete it",
			display_path(path)
		)),
		Probe::Writable if exists && has_entries(destination) => Check::Warning(NOT_EMPTY.into()),
		Probe::Writable => Check::Valid,
	}
}

fn check_name(name: &str, project_file: Option<&Path>) -> Check {
	if name.is_empty() {
		return Check::Invalid("enter a name for the project".into());
	}

	if name.chars().count() > MAX_NAME_CHARS {
		return Check::Invalid(format!("at most {MAX_NAME_CHARS} characters"));
	}

	if name.chars().any(char::is_control) {
		return Check::Invalid("can't contain control characters".into());
	}

	if let Some(bad) = name.chars().find(|c| INVALID_CHARS.contains(*c)) {
		return Check::Invalid(format!("can't contain {bad}"));
	}

	if name.ends_with(['.', ' ']) {
		return Check::Invalid("can't end with a dot or a space".into());
	}

	let stem = name.split('.').next().unwrap_or(name).trim_end();
	if is_reserved_device_name(stem) {
		return Check::Invalid(format!("{stem} is reserved by Windows for a device"));
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

/// Tries a new file in the destination when it exists. A missing destination is created later with all its missing
/// parents, so a folder and a file inside it are tried in its nearest existing ancestor instead: some folders allow new
/// subfolders but not new files. `planned` is the destination's planned path and `source` is canonical.
fn probe(planned: &Path, exists: bool, source: &Path, remove: &dyn Fn(&Path) -> io::Result<()>) -> Probe {
	let folder = if exists {
		Some(planned)
	} else {
		planned.ancestors().skip(1).find(|dir| dir.exists())
	};
	let Some(folder) = folder else {
		return Probe::Failed("no part of the path exists".into());
	};

	if !folder.is_dir() {
		return Probe::Failed(format!("{} is a file, not a folder", display_path(folder)));
	}

	if fs::canonicalize(folder).is_ok_and(|folder| folder.starts_with(source)) {
		return Probe::Failed("its nearest existing folder is inside the source".into());
	}

	let entry = folder.join(probe_name());
	if exists {
		if let Err(err) = create_file(&entry) {
			return Probe::Failed(err.to_string());
		}
		return match remove(&entry) {
			Ok(()) => Probe::Writable,
			Err(err) => leftover(entry, &err),
		};
	}

	if let Err(err) = fs::create_dir(&entry) {
		return Probe::Failed(err.to_string());
	}

	let inside = entry.join(probe_name());
	match create_file(&inside) {
		Ok(()) => match remove(&inside).and_then(|()| remove(&entry)) {
			Ok(()) => Probe::Writable,
			Err(err) => leftover(entry, &err),
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

fn leftover(path: PathBuf, err: &io::Error) -> Probe {
	Probe::Leftover {
		path,
		reason: err.to_string(),
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

	use cryarchiver_project::settings::plain_path;

	use super::*;

	struct Folders {
		dir: tempfile::TempDir,
		probes: std::cell::RefCell<Probes>,
	}

	impl Folders {
		fn new() -> Self {
			let dir = tempfile::tempdir().unwrap();
			fs::create_dir(dir.path().join("photos")).unwrap();
			fs::write(dir.path().join("photos").join("a.jpg"), b"jpg").unwrap();
			Self {
				dir,
				probes: std::cell::RefCell::default(),
			}
		}

		fn path(&self, relative: &str) -> String {
			relative
				.split('/')
				.fold(self.dir.path().to_path_buf(), |path, part| path.join(part))
				.display()
				.to_string()
		}

		fn validate(&self, source: &str, destination: &str, name: &str) -> Validation {
			validate(
				&self.path(source),
				&self.path(destination),
				name,
				&mut self.probes.borrow_mut(),
			)
		}

		fn probes_made(&self) -> usize {
			self.probes.borrow().made()
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

	fn invalid(check: &Check) -> String {
		match check {
			Check::Invalid(reason) => reason.clone(),
			other => panic!("expected a refusal, got {other:?}"),
		}
	}

	#[test]
	fn a_valid_form_passes_and_leaves_nothing_behind() {
		let folders = Folders::new();

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
	}

	#[test]
	fn the_source_must_be_a_folder_that_exists() {
		let folders = Folders::new();

		assert!(invalid(&folders.validate("missing", "archive", "a").source).contains("doesn't exist"));
		assert!(invalid(&folders.validate("photos/a.jpg", "archive", "a").source).contains("not a folder"));
		assert!(
			!validate("", &folders.path("archive"), "a", &mut Probes::default())
				.source
				.is_valid()
		);
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
		assert_eq!(folders.probes_made(), 0);
	}

	#[test]
	fn nothing_is_probed_while_the_source_does_not_check_out() {
		let folders = Folders::new();
		fs::create_dir(folders.path("photos2")).unwrap();

		for source in ["photo", "photos/a.jpg", "missing/deeper"] {
			let validation = folders.validate(source, "photos2/archive", "a");

			assert_eq!(validation.destination, Check::Pending(NOT_CHECKED.into()), "{source}");
			assert!(!validation.passes());
		}
		let mut probes = Probes::default();
		let unfilled = validate("", &folders.path("photos2/archive"), "a", &mut probes);
		assert_eq!(unfilled.destination, Check::Pending(NOT_CHECKED.into()));
		assert_eq!(probes.made(), 0);
		assert_eq!(folders.probes_made(), 0);
		assert_eq!(fs::read_dir(folders.path("photos2")).unwrap().count(), 0);

		assert_eq!(
			folders.validate("photos", "photos2/archive", "a").destination,
			Check::Valid
		);
		assert_eq!(folders.probes_made(), 1);
	}

	#[test]
	fn editing_other_fields_does_not_probe_again() {
		let folders = Folders::new();

		for name in ["P", "Ph", "Photos"] {
			assert!(folders.validate("photos", "archive", name).passes());
		}
		assert_eq!(folders.probes_made(), 1);

		folders.validate("photos", "archive2", "Photos");
		assert_eq!(folders.probes_made(), 2);
	}

	#[test]
	fn the_destination_must_be_a_folder_that_can_be_written() {
		let folders = Folders::new();
		fs::write(folders.path("file"), b"").unwrap();

		assert!(invalid(&folders.validate("photos", "file", "a").destination).contains("not a folder"));
		assert!(!folders.validate("photos", "file/archive", "a").destination.is_valid());
		assert!(
			!validate(&folders.path("photos"), "", "a", &mut Probes::default())
				.destination
				.is_valid()
		);
	}

	#[test]
	fn a_probe_that_cannot_be_removed_is_a_warning_naming_it() {
		let folders = Folders::new();
		let archive = folders.dir.path().join("archive");
		fs::create_dir(&archive).unwrap();
		let source = fs::canonicalize(folders.path("photos")).unwrap();
		let held = |_: &Path| Err(io::Error::other("held open"));

		let written = probe(&fs::canonicalize(&archive).unwrap(), true, &source, &held);
		let created = probe(
			&fs::canonicalize(folders.dir.path()).unwrap().join("new"),
			false,
			&source,
			&held,
		);

		for (probe, exists) in [(&written, true), (&created, false)] {
			let Check::Warning(text) = destination_check(probe, exists, &archive) else {
				panic!("{probe:?}");
			};
			assert!(text.starts_with("writable"), "{text}");
			assert!(text.contains(PROBE_PREFIX), "{text}");
			let Probe::Leftover { path, .. } = probe else {
				panic!("{probe:?}");
			};
			if exists {
				fs::remove_file(path).unwrap();
			} else {
				fs::remove_dir_all(path).unwrap();
			}
		}
		assert_eq!(folders.entries(), ["archive", "photos"]);
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
		assert_eq!(validation.destination, Check::Pending(NOT_CHECKED.into()));
		assert_eq!(folders.probes_made(), 0);
	}

	#[test]
	fn a_destination_that_refuses_new_entries_fails_the_probe() {
		let folders = Folders::new();
		let archive = folders.dir.path().join("archive");
		fs::create_dir(&archive).unwrap();
		let attempt = archive.join("attempt");

		let Some(denied) = Denied::apply(&archive, NO_NEW_ENTRIES, || {
			let refused = create_file(&attempt).is_err();
			let _ = fs::remove_file(&attempt);
			refused
		}) else {
			eprintln!("skipped: new entries are still allowed for this user");
			return;
		};
		let existing = folders.validate("photos", "archive", "a");
		let missing = folders.validate("photos", "archive/new", "a");
		drop(denied);

		assert!(
			invalid(&existing.destination).contains("can't be written"),
			"{existing:?}"
		);
		assert!(
			invalid(&missing.destination).contains("can't be created"),
			"{missing:?}"
		);
		assert_eq!(fs::read_dir(&archive).unwrap().count(), 0);
	}
}
