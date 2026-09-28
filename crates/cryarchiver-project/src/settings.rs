use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use cryarchiver_engine::commit::{FileIdentity, file_id};
use cryarchiver_engine::preset::Speed as EngineSpeed;
use cryarchiver_engine::text::display_path;
use serde::{Deserialize, Serialize};

pub const DEFAULT_CPU_CAP: u8 = 25;
pub const DELETE_CONFIRMATION: &str = "DELETE";

const MAX_MIN_SAVINGS: u8 = 90;
const MAX_STABILITY_WINDOW: Duration = Duration::from_hours(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Speed {
	Slow,
	#[default]
	Moderate,
	Fast,
}

impl From<Speed> for EngineSpeed {
	fn from(speed: Speed) -> Self {
		match speed {
			Speed::Slow => Self::Slow,
			Speed::Moderate => Self::Moderate,
			Speed::Fast => Self::Fast,
		}
	}
}

/// The project settings of the project layer spec §4, stored in the `meta` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
	#[serde(with = "crate::keys::path_bytes")]
	pub source: PathBuf,
	#[serde(with = "crate::keys::path_bytes")]
	pub destination: PathBuf,
	pub speed: Speed,
	/// Percent.
	pub min_savings: u8,
	pub watch: bool,
	/// The CPU cap in percent while the performance limit is on.
	pub performance_limit: Option<u8>,
	pub stability_window: Duration,
	pub keep_auxiliary_images: bool,
	pub delete_originals: bool,
}

impl Default for Settings {
	fn default() -> Self {
		Self {
			source: PathBuf::new(),
			destination: PathBuf::new(),
			speed: Speed::Moderate,
			min_savings: 10,
			watch: false,
			performance_limit: None,
			stability_window: Duration::from_secs(30),
			keep_auxiliary_images: true,
			delete_originals: false,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsChange {
	Speed(Speed),
	MinSavings(u8),
	Watch(bool),
	PerformanceLimit(Option<u8>),
	StabilityWindow(Duration),
	KeepAuxiliaryImages(bool),
	/// Turning deletion on needs `confirmation` to read `DELETE`; turning it off needs nothing.
	DeleteOriginals {
		enabled: bool,
		confirmation: String,
	},
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SettingsError {
	#[error("deleting originals is only turned on by typing DELETE")]
	DeleteNotConfirmed,
	#[error("minimum savings must be between 0 and {MAX_MIN_SAVINGS} %")]
	MinSavings,
	#[error("the CPU cap must be between 1 and 100 %")]
	CpuCap,
	#[error("the stability window must be at most an hour")]
	StabilityWindow,
	#[error("{0}")]
	Roots(String),
}

impl Settings {
	pub fn new(source: &Path, destination: &Path) -> Self {
		Self {
			source: source.to_path_buf(),
			destination: destination.to_path_buf(),
			..Self::default()
		}
	}

	/// The one way settings change, whatever the front end (project layer spec §4 and §9).
	pub fn apply(&mut self, change: SettingsChange) -> Result<(), SettingsError> {
		match change {
			SettingsChange::Speed(speed) => self.speed = speed,
			SettingsChange::MinSavings(percent) if percent <= MAX_MIN_SAVINGS => self.min_savings = percent,
			SettingsChange::MinSavings(_) => return Err(SettingsError::MinSavings),
			SettingsChange::Watch(watch) => self.watch = watch,
			SettingsChange::PerformanceLimit(Some(cap)) if !(1..=100).contains(&cap) => {
				return Err(SettingsError::CpuCap);
			}
			SettingsChange::PerformanceLimit(cap) => self.performance_limit = cap,
			SettingsChange::StabilityWindow(window) if window <= MAX_STABILITY_WINDOW => {
				self.stability_window = window;
			}
			SettingsChange::StabilityWindow(_) => return Err(SettingsError::StabilityWindow),
			SettingsChange::KeepAuxiliaryImages(keep) => self.keep_auxiliary_images = keep,
			SettingsChange::DeleteOriginals {
				enabled: true,
				confirmation,
			} if confirmation != DELETE_CONFIRMATION => return Err(SettingsError::DeleteNotConfirmed),
			SettingsChange::DeleteOriginals { enabled, .. } => self.delete_originals = enabled,
		}
		Ok(())
	}

	pub fn min_savings_fraction(&self) -> f64 {
		f64::from(self.min_savings) / 100.0
	}
}

/// Why a destination can't hold a source's archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootConflict {
	Same,
	DestinationInsideSource,
	SourceInsideDestination,
}

impl RootConflict {
	/// Compares paths as they are; both must be in the same form, canonical or planned.
	pub fn between(source: &Path, destination: &Path) -> Option<Self> {
		if source == destination {
			Some(Self::Same)
		} else if destination.starts_with(source) {
			Some(Self::DestinationInsideSource)
		} else if source.starts_with(destination) {
			Some(Self::SourceInsideDestination)
		} else {
			None
		}
	}

	pub const fn message(self) -> &'static str {
		match self {
			Self::Same => "the source and the destination are the same folder",
			Self::DestinationInsideSource => "the destination is inside the source",
			Self::SourceInsideDestination => "the source is inside the destination",
		}
	}
}

/// Where `path` will be once it exists: its nearest existing ancestor, canonical, followed by the missing names with
/// `.` and `..` resolved on the names alone. A path with no existing ancestor at all is resolved the same way and made
/// absolute.
pub fn planned_path(path: &Path) -> io::Result<PathBuf> {
	let components: Vec<Component> = path.components().collect();
	let mut existing = components.len();
	let ancestor = loop {
		let prefix: PathBuf = components[..existing].iter().collect();
		let prefix = if prefix.as_os_str().is_empty() {
			PathBuf::from(".")
		} else {
			prefix
		};
		if prefix.exists() {
			break Some(prefix);
		}

		if existing == 0 {
			break None;
		}
		existing -= 1;
	};

	let (mut planned, missing) = match &ancestor {
		Some(ancestor) => (
			std::fs::canonicalize(ancestor)
				.map_err(|err| io::Error::new(err.kind(), format!("{}: {err}", display_path(ancestor))))?,
			&components[existing..],
		),
		None => (PathBuf::new(), &components[..]),
	};

	for component in missing {
		match component {
			Component::CurDir => {}
			Component::ParentDir => {
				if planned.file_name().is_some() {
					planned.pop();
				}
			}
			other => planned.push(other),
		}
	}

	if ancestor.is_none() {
		return std::path::absolute(&planned);
	}
	Ok(planned)
}

/// Checks the roots before either needs to exist, on their planned paths, by text and by folder identity.
pub fn root_conflict(source: &Path, destination: &Path) -> io::Result<Option<RootConflict>> {
	let (source, destination) = (planned_path(source)?, planned_path(destination)?);
	Ok(RootConflict::between(&source, &destination).or_else(|| identity_conflict(&source, &destination)))
}

/// Compares folder identities, which catch what the text can't: on Linux, a bind mount, or a case-insensitive volume
/// where the path keeps the typed case, can name the source with other text. `source` is canonical and `destination`
/// planned; a source that doesn't exist has no identity, and gives no conflict here.
pub fn identity_conflict(source: &Path, destination: &Path) -> Option<RootConflict> {
	let source_id = file_id(source).ok()?;
	let existing = destination.ancestors().find(|dir| dir.exists())?;
	let existing_id = file_id(existing).ok()?;
	let complete = existing == destination;
	let is = |id: FileIdentity| move |dir: &Path| file_id(dir).is_ok_and(|found| found == id);

	if existing_id == source_id {
		return Some(if complete {
			RootConflict::Same
		} else {
			RootConflict::DestinationInsideSource
		});
	}

	if existing.ancestors().skip(1).any(is(source_id)) {
		return Some(RootConflict::DestinationInsideSource);
	}

	if complete && source.ancestors().skip(1).any(is(existing_id)) {
		return Some(RootConflict::SourceInsideDestination);
	}
	None
}

/// Whether the current user may create entries in `folder`, asked without writing anything.
#[cfg(unix)]
pub fn may_create_in(folder: &Path) -> bool {
	use std::os::unix::ffi::OsStrExt;

	let Ok(path) = std::ffi::CString::new(folder.as_os_str().as_bytes()) else {
		return false;
	};
	// SAFETY: `path` is a valid NUL-terminated string that outlives the call.
	unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
}

/// Windows can't be asked without trying; the probe when the project is created decides.
#[cfg(not(unix))]
pub fn may_create_in(_: &Path) -> bool {
	true
}

/// The roots must exist and differ, and neither may lie inside the other; checked on canonical paths and on folder
/// identities.
pub fn check_roots(source: &Path, destination: &Path) -> Result<(PathBuf, PathBuf), SettingsError> {
	let canonical = |path: &Path| {
		dunce_canonicalize(path).map_err(|err| SettingsError::Roots(format!("{}: {err}", display_path(path))))
	};
	let (source, destination) = (canonical(source)?, canonical(destination)?);

	let conflict = RootConflict::between(&source, &destination).or_else(|| identity_conflict(&source, &destination));
	if let Some(conflict) = conflict {
		return Err(SettingsError::Roots(conflict.message().into()));
	}
	Ok((source, destination))
}

fn dunce_canonicalize(path: &Path) -> io::Result<PathBuf> {
	std::fs::canonicalize(path).map(plain_path)
}

/// `fs::canonicalize` returns `\\?\C:\...` on Windows; this keeps that form out of stored settings, messages and
/// lists when a plain path means the same. Other paths are returned as they are.
pub fn plain_path(path: PathBuf) -> PathBuf {
	#[cfg(windows)]
	{
		use std::os::windows::ffi::{OsStrExt, OsStringExt};

		let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
		let verbatim: Vec<u16> = r"\\?\".encode_utf16().collect();
		let unc: Vec<u16> = "UNC\\".encode_utf16().collect();
		if let Some(plain) = wide.strip_prefix(verbatim.as_slice())
			&& !plain.starts_with(&unc)
			&& plain.len() < 260
		{
			return PathBuf::from(std::ffi::OsString::from_wide(plain));
		}
	}
	path
}

#[cfg(test)]
mod tests {
	use std::fs;

	use super::*;

	#[test]
	fn defaults_follow_the_spec() {
		let settings = Settings::default();

		assert_eq!(settings.speed, Speed::Moderate);
		assert_eq!(settings.min_savings, 10);
		assert!(!settings.watch);
		assert_eq!(settings.performance_limit, None);
		assert_eq!(settings.stability_window, Duration::from_secs(30));
		assert!(settings.keep_auxiliary_images);
		assert!(!settings.delete_originals);
	}

	#[test]
	fn deletion_needs_the_typed_confirmation() {
		let mut settings = Settings::default();

		assert_eq!(
			settings.apply(SettingsChange::DeleteOriginals {
				enabled: true,
				confirmation: "delete".into()
			}),
			Err(SettingsError::DeleteNotConfirmed)
		);
		assert!(!settings.delete_originals);

		settings
			.apply(SettingsChange::DeleteOriginals {
				enabled: true,
				confirmation: DELETE_CONFIRMATION.into(),
			})
			.unwrap();
		assert!(settings.delete_originals);

		settings
			.apply(SettingsChange::DeleteOriginals {
				enabled: false,
				confirmation: String::new(),
			})
			.unwrap();
		assert!(!settings.delete_originals);
	}

	#[test]
	fn out_of_range_values_are_refused() {
		let mut settings = Settings::default();

		assert_eq!(
			settings.apply(SettingsChange::MinSavings(91)),
			Err(SettingsError::MinSavings)
		);
		assert_eq!(
			settings.apply(SettingsChange::PerformanceLimit(Some(0))),
			Err(SettingsError::CpuCap)
		);
		assert_eq!(
			settings.apply(SettingsChange::StabilityWindow(Duration::from_hours(2))),
			Err(SettingsError::StabilityWindow)
		);
		settings
			.apply(SettingsChange::PerformanceLimit(Some(DEFAULT_CPU_CAP)))
			.unwrap();
		assert_eq!(settings.performance_limit, Some(25));
	}

	#[test]
	fn older_settings_take_defaults_for_missing_fields() {
		#[derive(Serialize)]
		struct Old {
			speed: Speed,
			watch: bool,
		}

		let bytes = rmp_serde::to_vec_named(&Old {
			speed: Speed::Fast,
			watch: true,
		})
		.unwrap();
		let settings: Settings = rmp_serde::from_slice(&bytes).unwrap();

		assert_eq!(settings.speed, Speed::Fast);
		assert!(settings.watch);
		assert_eq!(settings.min_savings, 10);
	}

	#[test]
	fn roots_must_be_separate() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		let inside = source.join("archive");
		let beside = dir.path().join("archive");
		fs::create_dir_all(&inside).unwrap();
		fs::create_dir_all(&beside).unwrap();

		assert!(check_roots(&source, &source).is_err());
		assert!(check_roots(&source, &inside).is_err());
		assert!(check_roots(&inside, &source).is_err());
		assert!(check_roots(&source, &dir.path().join("missing")).is_err());

		let (canonical_source, canonical_destination) = check_roots(&source, &beside).unwrap();
		assert!(canonical_source.ends_with("source"));
		assert!(canonical_destination.ends_with("archive"));
	}

	#[test]
	fn conflicts_are_found_before_the_folders_exist() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("new").join("photos");
		let conflict = |destination: &Path| root_conflict(&source, destination).unwrap();

		assert_eq!(conflict(&source), Some(RootConflict::Same));
		assert_eq!(
			conflict(&source.join("archive").join("deep")),
			Some(RootConflict::DestinationInsideSource)
		);
		assert_eq!(
			conflict(&dir.path().join("new")),
			Some(RootConflict::SourceInsideDestination)
		);
		assert_eq!(conflict(&dir.path().join("archive")), None);
		assert!(!dir.path().join("new").exists());
	}

	#[test]
	fn a_planned_path_starts_at_the_canonical_nearest_ancestor() {
		let dir = tempfile::tempdir().unwrap();
		fs::create_dir(dir.path().join("here")).unwrap();

		let planned = planned_path(&dir.path().join("here").join("..").join("here").join("later")).unwrap();

		assert_eq!(
			planned,
			fs::canonicalize(dir.path().join("here")).unwrap().join("later")
		);
	}

	#[test]
	fn dot_dot_among_the_missing_names_is_resolved_before_comparing() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("photos");
		fs::create_dir(&source).unwrap();
		let conflict = |destination: &Path| root_conflict(&source, destination).unwrap();

		assert_eq!(
			planned_path(&dir.path().join("missing").join("..").join("photos").join("sub")).unwrap(),
			fs::canonicalize(&source).unwrap().join("sub")
		);
		assert_eq!(
			conflict(&source.join("new").join("..").join("x")),
			Some(RootConflict::DestinationInsideSource)
		);
		assert_eq!(
			conflict(&source.join("new").join("deep").join("..")),
			Some(RootConflict::DestinationInsideSource)
		);
		assert_eq!(conflict(&source.join("new").join("..")), Some(RootConflict::Same));
		assert_eq!(conflict(&dir.path().join("new").join("..").join("archive")), None);
		assert!(!source.join("new").exists());
		assert!(!dir.path().join("missing").exists());
	}

	/// A second name for `target`: a junction on Windows, a symbolic link elsewhere. `None` when one can't be made.
	fn alias(target: &Path, alias: &Path) -> Option<PathBuf> {
		#[cfg(windows)]
		let made = std::process::Command::new("cmd")
			.args(["/C", "mklink", "/J"])
			.arg(alias)
			.arg(target)
			.stdout(std::process::Stdio::null())
			.status()
			.is_ok_and(|status| status.success());
		#[cfg(unix)]
		let made = std::os::unix::fs::symlink(target, alias).is_ok();

		made.then(|| alias.to_path_buf())
	}

	#[test]
	fn folder_identity_finds_the_source_behind_another_name() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		fs::create_dir_all(source.join("inner")).unwrap();
		let Some(alias) = alias(&source, &dir.path().join("alias")) else {
			eprintln!("skipped: no junction or link can be made here");
			return;
		};
		let canonical = fs::canonicalize(&source).unwrap();

		assert_eq!(identity_conflict(&canonical, &alias), Some(RootConflict::Same));
		assert_eq!(
			identity_conflict(&canonical, &alias.join("new").join("archive")),
			Some(RootConflict::DestinationInsideSource)
		);
		assert_eq!(
			identity_conflict(&canonical, &alias.join("inner")),
			Some(RootConflict::DestinationInsideSource)
		);
		assert_eq!(
			identity_conflict(&canonical.join("inner"), &alias),
			Some(RootConflict::SourceInsideDestination)
		);
		assert_eq!(identity_conflict(&canonical, &dir.path().join("archive")), None);
		assert_eq!(identity_conflict(&dir.path().join("missing"), &alias), None);
		assert_eq!(
			root_conflict(&source, &alias.join("archive")).unwrap(),
			Some(RootConflict::DestinationInsideSource)
		);
		assert!(check_roots(&source, &alias).is_err());
		assert!(!source.join("new").exists());
		fs::remove_dir(&alias).or_else(|_| fs::remove_file(&alias)).unwrap();
	}

	#[cfg(windows)]
	#[test]
	fn windows_paths_resolve_dot_dot_and_case_among_the_missing_names() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("Photos");
		fs::create_dir(&source).unwrap();
		let typed = |tail: &str| PathBuf::from(format!(r"{}\{tail}", dir.path().display()));

		assert_eq!(
			planned_path(&typed(r"PHOTOS\new\deep\..\.\x")).unwrap(),
			fs::canonicalize(&source).unwrap().join("new").join("x")
		);
		assert_eq!(
			root_conflict(&source, &typed(r"photos\new\deep\..")).unwrap(),
			Some(RootConflict::DestinationInsideSource)
		);
		assert_eq!(root_conflict(&source, &typed(r"new\deep\..\..\archive")).unwrap(), None);
	}

	#[cfg(unix)]
	#[test]
	fn unix_paths_resolve_dot_dot_among_the_missing_names() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("photos");
		fs::create_dir(&source).unwrap();
		let typed = |tail: &str| PathBuf::from(format!("{}/{tail}", dir.path().display()));

		assert_eq!(
			root_conflict(&source, &typed("missing/../photos/sub")).unwrap(),
			Some(RootConflict::DestinationInsideSource)
		);
		assert_eq!(
			planned_path(&typed("missing/deeper/../../photos/./sub")).unwrap(),
			fs::canonicalize(&source).unwrap().join("sub")
		);
		assert!(!dir.path().join("missing").exists());
	}
}
