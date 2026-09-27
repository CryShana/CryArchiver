use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

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

/// The roots must exist and differ, and neither may lie inside the other; checked on canonical paths.
pub fn check_roots(source: &Path, destination: &Path) -> Result<(PathBuf, PathBuf), SettingsError> {
	let canonical = |path: &Path| {
		dunce_canonicalize(path).map_err(|err| SettingsError::Roots(format!("{}: {err}", display_path(path))))
	};
	let (source, destination) = (canonical(source)?, canonical(destination)?);

	if source == destination {
		return Err(SettingsError::Roots(
			"the source and the destination are the same folder".into(),
		));
	}
	if destination.starts_with(&source) {
		return Err(SettingsError::Roots("the destination is inside the source".into()));
	}
	if source.starts_with(&destination) {
		return Err(SettingsError::Roots("the source is inside the destination".into()));
	}
	Ok((source, destination))
}

// `fs::canonicalize` returns `\\?\C:\...` on Windows; keep that form out of stored settings and messages when a
// plain path means the same.
fn dunce_canonicalize(path: &Path) -> io::Result<PathBuf> {
	let canonical = std::fs::canonicalize(path)?;

	#[cfg(windows)]
	if let Some(plain) = canonical.to_str().and_then(|text| text.strip_prefix(r"\\?\"))
		&& !plain.starts_with("UNC\\")
		&& plain.len() < 260
	{
		return Ok(PathBuf::from(plain));
	}
	Ok(canonical)
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
}
