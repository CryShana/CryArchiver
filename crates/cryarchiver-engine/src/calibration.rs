use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::preset::Speed;
use crate::video::quality::CRF_RANGE;

pub const TABLES_VERSION: u32 = 1;

const SHIPPED: &str = include_str!("../../../calibration/tables.toml");

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Tables {
	pub version: u32,
	pub video: VideoTables,
	pub images: ImageTables,
	#[serde(default)]
	pub report: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct VideoTables {
	pub svt_crf_slow: [f64; 6],
	pub svt_crf_moderate: [f64; 6],
	pub svt_crf_fast: [f64; 6],
	pub vp9_crf: [u8; 6],
	pub expected_bpp: [f64; 6],
	/// The noise deviation, in 8-bit code values, from which a source counts as noisy.
	pub noisy_sigma: f64,
	pub low_detail_si: f64,
	pub grain_allowance_mean: f64,
	pub grain_allowance_floor: f64,
	pub grain_from_sigma: [f64; 3],
	pub grain_levels: [u8; 3],
	pub default_slope: f64,
	pub margin_from_s: f64,
	pub full_margin_s: f64,
	pub full_margin_mean: f64,
	pub full_margin_floor: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ImageTables {
	pub ravif_points_per_quality: f64,
	pub jxl_points_per_distance: f64,
}

impl VideoTables {
	pub fn svt_crf(&self, speed: Speed) -> &[f64; 6] {
		match speed {
			Speed::Slow => &self.svt_crf_slow,
			Speed::Moderate => &self.svt_crf_moderate,
			Speed::Fast => &self.svt_crf_fast,
		}
	}
}

impl Tables {
	fn check(&self) -> Result<(), String> {
		let video = &self.video;
		let starts = video
			.svt_crf_slow
			.iter()
			.chain(&video.svt_crf_moderate)
			.chain(&video.svt_crf_fast);
		if let Some(crf) = starts.copied().find(|crf| !CRF_RANGE.contains(crf)) {
			return Err(format!(
				"the start CRF {crf} is outside {}–{}",
				CRF_RANGE.start(),
				CRF_RANGE.end()
			));
		}

		if let Some(crf) = video.vp9_crf.iter().find(|crf| !CRF_RANGE.contains(&f64::from(**crf))) {
			return Err(format!("the VP9 CRF {crf} is outside the search range"));
		}

		let positive = [
			video.default_slope,
			self.images.ravif_points_per_quality,
			self.images.jxl_points_per_distance,
		];
		if video
			.expected_bpp
			.iter()
			.chain(&positive)
			.any(|value| !(value.is_finite() && *value > 0.0))
		{
			return Err("bits per pixel and slopes must be positive numbers".into());
		}

		if !video.grain_from_sigma.windows(2).all(|pair| pair[0] > pair[1]) {
			return Err("grain-from-sigma must fall from the noisiest level".into());
		}

		if video.grain_levels.iter().any(|level| !(1..=50).contains(level)) {
			return Err("grain levels must be 1–50".into());
		}

		if !(video.grain_allowance_mean >= 0.0 && video.grain_allowance_floor >= 0.0) {
			return Err("grain allowances can't be negative".into());
		}

		if !(video.margin_from_s > 0.0 && video.full_margin_s > video.margin_from_s && video.full_margin_s.is_finite())
		{
			return Err("full-margin-s must come after a positive margin-from-s".into());
		}

		if !(video.full_margin_mean >= 0.0 && video.full_margin_floor >= 0.0) {
			return Err("the full margin can't be negative".into());
		}
		Ok(())
	}
}

pub fn parse(text: &str) -> Result<Tables, String> {
	let tables: Tables = toml::from_str(text).map_err(|err| err.to_string())?;
	if tables.version != TABLES_VERSION {
		return Err(format!(
			"calibration tables version {} can't be read (expected {TABLES_VERSION})",
			tables.version
		));
	}

	tables.check()?;
	Ok(tables)
}

pub fn tables() -> &'static Tables {
	static TABLES: OnceLock<Tables> = OnceLock::new();
	TABLES.get_or_init(|| parse(SHIPPED).expect("calibration/tables.toml is valid"))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	#[allow(clippy::float_cmp)]
	fn the_shipped_tables_hold_the_tuned_values() {
		let tables = tables();

		assert_eq!(
			tables.video.svt_crf(Speed::Moderate),
			&[28.0, 29.0, 29.0, 30.0, 32.0, 34.0]
		);
		assert_eq!(tables.video.svt_crf(Speed::Fast), &[35.0, 35.0, 35.0, 37.0, 36.0, 38.0]);
		assert_eq!(tables.video.vp9_crf, [33, 32, 31, 30, 28, 28]);
		assert_eq!(tables.video.expected_bpp, [0.08, 0.06, 0.045, 0.035, 0.025, 0.02]);
		assert!((tables.video.noisy_sigma - 0.29).abs() < 1e-9);
		assert!((tables.images.jxl_points_per_distance - 8.0).abs() < 1e-9);
		assert!(tables.report.is_empty());
		assert_eq!(tables.video.grain_levels, [10, 8, 6]);
		assert_eq!(tables.video.grain_from_sigma, [1.8, 1.0, 0.29]);
		assert!((tables.video.default_slope - 0.5).abs() < 1e-9);
		assert!((tables.video.margin_from_s - 900.0).abs() < 1e-9);
		assert!((tables.video.full_margin_s - 14_400.0).abs() < 1e-9);
	}

	#[test]
	fn tables_round_trip_and_unknown_keys_are_refused() {
		let text = toml::to_string_pretty(tables()).unwrap();

		assert_eq!(parse(&text).as_ref(), Ok(tables()));
		assert!(parse(&text.replace("noisy-sigma", "noisy")).is_err());
		assert!(
			parse(&text.replace("version = 1", "version = 2"))
				.unwrap_err()
				.contains("version")
		);
	}

	#[test]
	fn out_of_range_tables_are_refused() {
		let text = toml::to_string_pretty(tables()).unwrap();

		assert!(
			parse(&text.replace("svt-crf-fast = [\n    35.0", "svt-crf-fast = [\n    60.0"))
				.unwrap_err()
				.contains("60")
		);
		assert!(parse(&text.replace("grain-from-sigma = [\n    1.8", "grain-from-sigma = [\n    0.9")).is_err());
		assert!(parse(&text.replace("default-slope = 0.5", "default-slope = 0.0")).is_err());
		assert!(parse(&text.replace("full-margin-s = 14400.0", "full-margin-s = 900.0")).is_err());
	}
}
