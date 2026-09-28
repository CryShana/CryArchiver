use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeInclusive;
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

use crate::images::plan::{JXL_DISTANCE_RANGE, RAVIF_QUALITY_RANGE};
use crate::quality::keys::CacheKey;
use crate::toolchain::{LIBAOM, LIBJXL, LIBVPX_VP9, SVT_AV1};
use crate::video::quality::CRF_RANGE;

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

	/// Why stats read back for `key` can't be used, if they can't: stored values may be damaged, hand-edited or from
	/// another build, and are checked before they reach an encoder.
	pub fn problem(&self, key: &str) -> Option<String> {
		let encoder = key.split('|').nth(1).unwrap_or_default();
		let Some(range) = setting_range(encoder) else {
			return Some(format!("{encoder:?} is not an encoder this version knows"));
		};

		if !range.contains(&self.setting) {
			return Some(format!(
				"the setting {} is outside {}–{}",
				self.setting,
				range.start(),
				range.end()
			));
		}

		let positive = |value: Option<f64>| value.is_none_or(|value| value.is_finite() && value > 0.0);
		if !positive(self.slope) {
			return Some(format!(
				"the slope {:?} is not a positive number",
				self.slope.unwrap_or_default()
			));
		}

		if !positive(self.bpp) {
			return Some(format!(
				"the bits per pixel {:?} are not a positive number",
				self.bpp.unwrap_or_default()
			));
		}
		None
	}
}

fn setting_range(encoder: &str) -> Option<RangeInclusive<f64>> {
	if [SVT_AV1, LIBAOM, LIBVPX_VP9]
		.iter()
		.any(|video| encoder.starts_with(video))
	{
		Some(CRF_RANGE)
	} else if encoder.starts_with("ravif") {
		Some(RAVIF_QUALITY_RANGE)
	} else if encoder.starts_with(LIBJXL) {
		Some(JXL_DISTANCE_RANGE)
	} else {
		None
	}
}

pub trait QualityMemory: Send + Sync {
	fn lookup(&self, key: &CacheKey) -> Option<GroupStats>;

	fn record(&self, key: &CacheKey, observation: Observation);

	/// Drops what the group learned, so that its next file starts afresh.
	fn forget(&self, key: &CacheKey);
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

	/// Every group changed since the last call, with its stats now, or `None` once it was forgotten.
	pub fn take_changed(&self) -> Vec<(String, Option<GroupStats>)> {
		let mut entries = self.lock();
		let changed = std::mem::take(&mut entries.changed);

		changed
			.into_iter()
			.map(|key| {
				let stats = entries.stats.get(&key).copied();
				(key, stats)
			})
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

	fn forget(&self, key: &CacheKey) {
		let key = key.to_string();
		let mut entries = self.lock();

		if entries.stats.remove(&key).is_some() {
			entries.changed.insert(key);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::images::plan::RAVIF_ID;
	use crate::metadata::ImageFormat;
	use crate::quality::keys::{Group, ImageGroup};

	fn key(format: ImageFormat) -> CacheKey {
		CacheKey {
			group: Group::Image(ImageGroup::new(format, false, false, false, 4000, 3000)),
			encoder: RAVIF_ID.into(),
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

	#[test]
	fn a_forgotten_group_is_handed_out_as_removed() {
		let memory = SharedMemory::default();
		memory.record(&key(ImageFormat::Jpeg), observation(90.0));
		memory.take_changed();

		memory.forget(&key(ImageFormat::Jpeg));
		memory.forget(&key(ImageFormat::Png));

		assert_eq!(memory.lookup(&key(ImageFormat::Jpeg)), None);
		assert_eq!(memory.take_changed(), vec![(key(ImageFormat::Jpeg).to_string(), None)]);
	}

	#[test]
	fn stats_outside_their_encoders_range_are_refused() {
		let stats = |setting: f64, slope: Option<f64>, bpp: Option<f64>| GroupStats {
			setting,
			slope,
			bpp,
			observations: 1,
		};
		let video = "video/h264/1080/fps31/sdr8/progressive/clean/opaque|svt-av1-4.2.0|p5|t93.0/89.0";
		let photo = "image/jpeg/photo/opaque/8bit/mp12|ravif-0.13.0|p6|t85.0";
		let tall = "image/png/photo/opaque/deep/mp4|libjxl-ffmpeg-n9.0.1|p7|t85.0";

		assert_eq!(stats(29.5, Some(0.4), Some(0.03)).problem(video), None);
		assert_eq!(stats(88.0, None, None).problem(photo), None);
		assert_eq!(stats(1.2, None, None).problem(tall), None);
		for bad in [
			stats(70.0, Some(0.4), None),
			stats(f64::NAN, Some(0.4), None),
			stats(29.5, Some(f64::NAN), None),
			stats(29.5, Some(0.0), None),
			stats(29.5, None, Some(f64::INFINITY)),
			stats(29.5, None, Some(-0.1)),
		] {
			assert!(bad.problem(video).is_some(), "{bad:?}");
		}
		assert!(stats(120.0, None, None).problem(photo).is_some());
		assert!(stats(40.0, None, None).problem(tall).is_some());
		assert!(stats(29.5, None, None).problem("video/x|x264|p5|t93.0").is_some());
	}
}
