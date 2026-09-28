use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

use crate::quality::keys::CacheKey;

const HISTORY: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GroupStats {
	pub setting: f64,
	#[serde(default)]
	pub slope: Option<f64>,
	#[serde(default)]
	pub bpp: Option<f64>,
	pub observations: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Observation {
	pub setting: f64,
	pub slope: Option<f64>,
	pub bpp: Option<f64>,
}

impl GroupStats {
	pub fn first(observation: Observation) -> Self {
		Self {
			setting: observation.setting,
			slope: observation.slope,
			bpp: observation.bpp,
			observations: 1,
		}
	}

	#[must_use]
	pub fn updated(self, observation: Observation) -> Self {
		let weight = f64::from(self.observations.min(HISTORY - 1));
		let blend = |old: f64, new: f64| (old * weight + new) / (weight + 1.0);
		let merge = |old: Option<f64>, new: Option<f64>| match (old, new) {
			(Some(old), Some(new)) => Some(blend(old, new)),
			(old, new) => new.or(old),
		};

		Self {
			setting: blend(self.setting, observation.setting),
			slope: merge(self.slope, observation.slope),
			bpp: merge(self.bpp, observation.bpp),
			observations: self.observations.saturating_add(1),
		}
	}
}

pub trait QualityMemory: Send + Sync {
	fn lookup(&self, key: &CacheKey) -> Option<GroupStats>;

	fn record(&self, key: &CacheKey, observation: Observation);
}

#[derive(Debug, Default)]
struct Entries {
	stats: BTreeMap<String, GroupStats>,
	changed: BTreeSet<String>,
}

#[derive(Debug, Default)]
pub struct SharedMemory {
	entries: Mutex<Entries>,
}

impl SharedMemory {
	pub fn with_entries(entries: impl IntoIterator<Item = (String, GroupStats)>) -> Self {
		Self {
			entries: Mutex::new(Entries {
				stats: entries.into_iter().collect(),
				changed: BTreeSet::new(),
			}),
		}
	}

	pub fn entries(&self) -> Vec<(String, GroupStats)> {
		self.lock()
			.stats
			.iter()
			.map(|(key, stats)| (key.clone(), *stats))
			.collect()
	}

	pub fn take_changed(&self) -> Vec<(String, GroupStats)> {
		let mut entries = self.lock();
		let changed = std::mem::take(&mut entries.changed);

		changed
			.into_iter()
			.filter_map(|key| entries.stats.get(&key).map(|stats| (key, *stats)))
			.collect()
	}

	fn lock(&self) -> MutexGuard<'_, Entries> {
		self.entries.lock().unwrap_or_else(PoisonError::into_inner)
	}
}

impl QualityMemory for SharedMemory {
	fn lookup(&self, key: &CacheKey) -> Option<GroupStats> {
		self.lock().stats.get(&key.to_string()).copied()
	}

	fn record(&self, key: &CacheKey, observation: Observation) {
		let key = key.to_string();
		let mut entries = self.lock();
		let stats = match entries.stats.get(&key) {
			Some(stats) => stats.updated(observation),
			None => GroupStats::first(observation),
		};

		entries.stats.insert(key.clone(), stats);
		entries.changed.insert(key);
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::metadata::ImageFormat;
	use crate::quality::keys::{Group, ImageGroup};

	fn key(format: ImageFormat) -> CacheKey {
		CacheKey {
			group: Group::Image(ImageGroup::new(format, false, false, false, 4000, 3000)),
			encoder: "ravif-0.13".into(),
			preset: 6,
			target: 85.0,
			floor: None,
		}
	}

	fn observation(setting: f64) -> Observation {
		Observation {
			setting,
			slope: Some(0.4),
			bpp: None,
		}
	}

	#[test]
	fn a_group_remembers_the_average_of_its_settings() {
		let memory = SharedMemory::default();

		assert_eq!(memory.lookup(&key(ImageFormat::Jpeg)), None);
		memory.record(&key(ImageFormat::Jpeg), observation(90.0));
		memory.record(&key(ImageFormat::Jpeg), observation(92.0));

		let stats = memory.lookup(&key(ImageFormat::Jpeg)).unwrap();
		assert!((stats.setting - 91.0).abs() < 1e-9, "{stats:?}");
		assert_eq!(stats.observations, 2);
		assert_eq!(stats.slope, Some(0.4));
		assert_eq!(memory.lookup(&key(ImageFormat::Png)), None);
	}

	#[test]
	fn old_observations_fade() {
		let mut stats = GroupStats::first(observation(80.0));
		for _ in 0..40 {
			stats = stats.updated(observation(90.0));
		}

		assert!(stats.setting > 89.0, "{stats:?}");
		assert_eq!(stats.observations, 41);
	}

	#[test]
	fn changes_are_handed_out_once() {
		let memory = SharedMemory::default();
		memory.record(&key(ImageFormat::Jpeg), observation(90.0));

		assert_eq!(memory.take_changed().len(), 1);
		assert!(memory.take_changed().is_empty());
		assert_eq!(memory.entries().len(), 1);
	}
}
