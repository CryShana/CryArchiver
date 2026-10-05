use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeInclusive;
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

use crate::images::plan::{JXL_DISTANCE_RANGE, RAVIF_QUALITY_RANGE};
use crate::quality::keys::CacheKey;
use crate::toolchain::{LIBAOM, LIBJXL, LIBVPX_VP9, SVT_AV1};
use crate::video::quality::CRF_RANGE;

const HISTORY: u32 = 8;
const MAX_STORED_OFFSET: f64 = 20.0;

/// How far a full check landed from the search samples' prediction: the full score minus the predicted one.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Offset {
	pub mean: f64,
	pub p5: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GroupStats {
	pub setting: f64,
	#[serde(default)]
	pub slope: Option<f64>,
	#[serde(default)]
	pub bpp: Option<f64>,
	#[serde(default)]
	pub offset: Option<Offset>,
	/// 0 once the group's setting was forgotten: the entry then keeps only its offset, which says how its samples
	/// predict its full checks whatever CRF they are encoded at.
	pub observations: u32,
	/// A noisy group whose files came out smaller without film grain than with it; the setting is for no grain.
	#[serde(default)]
	pub without_grain: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Observation {
	pub setting: f64,
	pub slope: Option<f64>,
	pub bpp: Option<f64>,
	pub offset: Option<Offset>,
	pub without_grain: bool,
}

impl GroupStats {
	pub fn first(observation: Observation) -> Self {
		Self {
			setting: observation.setting,
			slope: observation.slope,
			bpp: observation.bpp,
			offset: observation.offset,
			observations: 1,
			without_grain: observation.without_grain,
		}
	}

	/// Whether the group has a setting to use.
	pub fn learned(&self) -> bool {
		self.observations > 0
	}

	/// What is kept once the setting is forgotten: the offset alone, if there is one.
	#[must_use]
	pub fn forgotten(self) -> Option<Self> {
		self.offset.map(|offset| Self {
			slope: None,
			bpp: None,
			offset: Some(offset),
			observations: 0,
			without_grain: false,
			..self
		})
	}

	/// Settings for and without grain don't mix: an observation of the other kind starts the group afresh. The offset
	/// carries over either way.
	#[must_use]
	pub fn updated(self, observation: Observation) -> Self {
		if observation.without_grain != self.without_grain || !self.learned() {
			return Self {
				offset: combine_offsets(self.offset, observation.offset, f64::midpoint),
				..Self::first(observation)
			};
		}

		let weight = f64::from(self.observations.min(HISTORY - 1));
		let blend = |old: f64, new: f64| (old * weight + new) / (weight + 1.0);

		Self {
			setting: blend(self.setting, observation.setting),
			slope: combine(self.slope, observation.slope, blend),
			bpp: combine(self.bpp, observation.bpp, blend),
			offset: combine_offsets(self.offset, observation.offset, blend),
			observations: self.observations.saturating_add(1),
			without_grain: self.without_grain,
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

		let plausible = |gap: f64| (-MAX_STORED_OFFSET..=MAX_STORED_OFFSET).contains(&gap);
		if let Some(offset) = self.offset
			&& !(plausible(offset.mean) && plausible(offset.p5))
		{
			return Some(format!(
				"the sample offset {offset:?} is not a plausible VMAF difference"
			));
		}
		None
	}
}

/// `both` of the two values when there are two, else the one there is.
fn combine<T>(old: Option<T>, new: Option<T>, both: impl FnOnce(T, T) -> T) -> Option<T> {
	match (old, new) {
		(Some(old), Some(new)) => Some(both(old, new)),
		(old, new) => new.or(old),
	}
}

fn combine_offsets(old: Option<Offset>, new: Option<Offset>, both: impl Fn(f64, f64) -> f64) -> Option<Offset> {
	combine(old, new, |old, new| Offset {
		mean: both(old.mean, new.mean),
		p5: both(old.p5, new.p5),
	})
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
	/// The group's stats, when it has a setting to use.
	fn lookup(&self, key: &CacheKey) -> Option<GroupStats>;

	/// How far the group's full checks land from its search samples, kept even once its setting is forgotten.
	fn offset(&self, key: &CacheKey) -> Option<Offset>;

	fn record(&self, key: &CacheKey, observation: Observation);

	/// Takes in `observation`'s offset as `record` does, but leaves the group without a setting: the observation's
	/// output landed just below its target.
	fn record_offset(&self, key: &CacheKey, observation: Observation);

	/// Drops the group's setting, so that its next file starts afresh. Its offset stays.
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

	/// Replaces `key`'s stats with what `change` makes of them, `None` dropping them, and marks the group changed
	/// unless it had none and still has none.
	fn apply(&self, key: &CacheKey, change: impl FnOnce(Option<GroupStats>) -> Option<GroupStats>) {
		let key = key.to_string();
		let mut entries = self.lock();
		let old = entries.stats.get(&key).copied();
		let new = change(old);
		if old.is_none() && new.is_none() {
			return;
		}

		match new {
			Some(stats) => entries.stats.insert(key.clone(), stats),
			None => entries.stats.remove(&key),
		};
		entries.changed.insert(key);
	}
}

fn observed(stats: Option<GroupStats>, observation: Observation) -> GroupStats {
	stats.map_or_else(|| GroupStats::first(observation), |stats| stats.updated(observation))
}

impl QualityMemory for SharedMemory {
	fn lookup(&self, key: &CacheKey) -> Option<GroupStats> {
		self.lock()
			.stats
			.get(&key.to_string())
			.copied()
			.filter(GroupStats::learned)
	}

	fn offset(&self, key: &CacheKey) -> Option<Offset> {
		self.lock().stats.get(&key.to_string()).and_then(|stats| stats.offset)
	}

	fn record(&self, key: &CacheKey, observation: Observation) {
		self.apply(key, |stats| Some(observed(stats, observation)));
	}

	fn record_offset(&self, key: &CacheKey, observation: Observation) {
		self.apply(key, |stats| observed(stats, observation).forgotten());
	}

	fn forget(&self, key: &CacheKey) {
		self.apply(key, |stats| stats.and_then(GroupStats::forgotten));
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
			offset: None,
			without_grain: false,
		}
	}

	#[test]
	fn offsets_blend_like_the_other_values_and_survive_observations_without_one() {
		let observed = |mean: f64| Observation {
			setting: 30.0,
			slope: None,
			bpp: None,
			offset: Some(Offset { mean, p5: mean * 2.0 }),
			without_grain: false,
		};

		let first = GroupStats::first(observed(-1.0));
		let second = first.updated(observed(-3.0));
		let third = second.updated(Observation {
			offset: None,
			..observed(0.0)
		});

		assert_eq!(first.offset, Some(Offset { mean: -1.0, p5: -2.0 }));
		assert_eq!(second.offset, Some(Offset { mean: -2.0, p5: -4.0 }));
		assert_eq!(third.offset, second.offset);
	}

	#[test]
	fn a_near_miss_teaches_its_offset_but_no_setting() {
		let memory = SharedMemory::default();
		let near = |mean: f64| Observation {
			offset: Some(Offset { mean, p5: mean }),
			..observation(90.0)
		};
		let key = key(ImageFormat::Jpeg);

		memory.record_offset(&key, near(-1.0));

		assert_eq!(memory.lookup(&key), None, "no setting to use");
		assert_eq!(memory.offset(&key), Some(Offset { mean: -1.0, p5: -1.0 }));

		memory.record(&key, near(-3.0));
		memory.record_offset(&key, near(-3.0));

		assert_eq!(memory.lookup(&key), None, "a learned setting is forgotten");
		assert_eq!(
			memory.offset(&key),
			Some(Offset { mean: -2.5, p5: -2.5 }),
			"blended as a check that passed would"
		);

		let fresh = SharedMemory::default();
		fresh.record_offset(&key, observation(90.0));
		assert!(fresh.entries().is_empty() && fresh.take_changed().is_empty());
	}

	#[test]
	fn a_group_that_switches_to_no_grain_starts_afresh() {
		let mut stats = GroupStats::first(observation(30.0)).updated(observation(32.0));
		let plain = Observation {
			without_grain: true,
			..observation(24.0)
		};

		stats = stats.updated(plain);

		assert!(stats.without_grain);
		assert!((stats.setting - 24.0).abs() < 1e-9, "{stats:?}");
		assert_eq!(stats.observations, 1);
		assert!(stats.updated(plain).without_grain);
		assert_eq!(stats.updated(plain).observations, 2);
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
	fn a_forgotten_group_keeps_its_offset_and_learns_its_setting_afresh() {
		let memory = SharedMemory::default();
		let measured = |setting: f64, mean: f64| Observation {
			offset: Some(Offset { mean, p5: mean - 1.0 }),
			..observation(setting)
		};
		memory.record(&key(ImageFormat::Jpeg), measured(30.0, -2.0));
		memory.take_changed();

		memory.forget(&key(ImageFormat::Jpeg));

		assert_eq!(memory.lookup(&key(ImageFormat::Jpeg)), None);
		assert_eq!(
			memory.offset(&key(ImageFormat::Jpeg)),
			Some(Offset { mean: -2.0, p5: -3.0 })
		);
		let changed = memory.take_changed();
		assert!(
			matches!(changed.as_slice(), [(_, Some(stats))] if !stats.learned() && stats.offset.is_some()),
			"{changed:?}"
		);

		memory.record(&key(ImageFormat::Jpeg), measured(40.0, -1.0));

		let relearned = memory.lookup(&key(ImageFormat::Jpeg)).unwrap();
		assert_eq!((relearned.setting, relearned.observations), (40.0, 1));
		assert_eq!(relearned.offset, Some(Offset { mean: -1.5, p5: -2.5 }));
	}

	#[test]
	fn stats_outside_their_encoders_range_are_refused() {
		let stats = |setting: f64, slope: Option<f64>, bpp: Option<f64>| GroupStats {
			setting,
			slope,
			bpp,
			offset: None,
			observations: 1,
			without_grain: false,
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

	#[test]
	fn implausible_sample_offsets_are_refused() {
		let video = "video/h264/1080/fps31/sdr8/progressive/clean/opaque|svt-av1-4.2.0|p5|t93.0/89.0";
		let offset = |mean: f64, p5: f64| GroupStats {
			setting: 29.5,
			slope: Some(0.4),
			bpp: None,
			offset: Some(Offset { mean, p5 }),
			observations: 1,
			without_grain: false,
		};

		assert_eq!(offset(-0.4, -2.7).problem(video), None);
		assert_eq!(offset(20.0, -20.0).problem(video), None);
		for bad in [
			offset(-20.5, 0.0),
			offset(0.0, 35.0),
			offset(f64::NAN, 0.0),
			offset(0.0, f64::NEG_INFINITY),
		] {
			assert!(bad.problem(video).is_some(), "{bad:?}");
		}
	}
}
