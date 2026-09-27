//! The checks of the new project form, run again on every change. Each one only looks at the file system, except the
//! destination probe, which creates and removes one entry outside the source.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use cryarchiver_engine::commit::free_space;
use cryarchiver_engine::naming::is_reserved_device_name;
use cryarchiver_project::PROJECT_EXTENSION;
use cryarchiver_project::settings::root_conflict;

pub const MAX_NAME_CHARS: usize = 100;
const INVALID_CHARS: &str = "<>:\"/\\|?*";
pub const NOT_EMPTY: &str = "files already there will be listed as unknown and never touched";

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
	/// `<destination>/<name>.cryarchive`, absolute, once both are filled in.
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
	let project_file = (!destination.is_empty() && !name.is_empty()).then(|| {
		let folder = std::path::absolute(destination).unwrap_or_else(|_| PathBuf::from(destination));
		folder.join(format!("{name}.{PROJECT_EXTENSION}"))
	});
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

	match fs::metadata(path) {
		Ok(meta) if !meta.is_dir() => Check::Invalid("is a file, not a folder".into()),
		Ok(_) => match probe_file(path) {
			Err(err) => Check::Invalid(format!("can't be written: {err}")),
			Ok(()) if has_entries(path) => Check::Warning(NOT_EMPTY.into()),
			Ok(()) => Check::Valid,
		},
		Err(err) if err.kind() == io::ErrorKind::NotFound => match probe_folder(path) {
			Ok(()) => Check::Valid,
			Err(err) => Check::Invalid(format!("can't be created: {err}")),
		},
		Err(err) => Check::Invalid(format!("can't be read: {err}")),
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

/// Creates and removes a file in the destination, which exists and was checked to lie outside the source.
fn probe_file(folder: &Path) -> io::Result<()> {
	let probe = folder.join(probe_name());
	let file = fs::OpenOptions::new().write(true).create_new(true).open(&probe)?;
	drop(file);
	fs::remove_file(&probe)
}

/// A missing destination is created later with all its missing parents, so a folder and a file inside it are tried
/// in its nearest existing ancestor: some folders allow new subfolders but not new files.
fn probe_folder(destination: &Path) -> io::Result<()> {
	let absolute = std::path::absolute(destination)?;
	let Some(ancestor) = absolute.ancestors().skip(1).find(|dir| dir.exists()) else {
		return Err(io::Error::new(io::ErrorKind::NotFound, "no part of the path exists"));
	};

	let probe = ancestor.join(probe_name());
	fs::create_dir(&probe)?;
	let inside = probe_file(&probe);
	let removed = fs::remove_dir(&probe);
	inside.and(removed)
}

fn probe_name() -> String {
	use std::sync::atomic::{AtomicU64, Ordering};

	static NEXT: AtomicU64 = AtomicU64::new(0);
	format!(
		".cryarchiver-probe-{}-{}",
		std::process::id(),
		NEXT.fetch_add(1, Ordering::Relaxed)
	)
}

#[cfg(test)]
mod tests {
	use std::fs;

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
			PathBuf::from(folders.path("archive")).join("Photos.cryarchive")
		);
		assert!(validation.free_space.is_some_and(|free| free > 0));
		assert_eq!(folders.entries(), ["photos"]);
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
	fn the_destination_must_be_a_folder_that_can_be_written() {
		let folders = Folders::new();
		fs::write(folders.path("file"), b"").unwrap();

		assert!(invalid(&folders.validate("photos", "file", "a").destination).contains("not a folder"));
		assert!(!folders.validate("photos", "file/archive", "a").destination.is_valid());
		assert!(!validate(&folders.path("photos"), "", "a").destination.is_valid());
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
}
