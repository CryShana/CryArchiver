use std::ops::RangeInclusive;

use crate::calibration::tables;
use crate::images::props::Effective;
use crate::images::ssim::padded_size;
use crate::images::structure::Structure;
use crate::metadata::{AuxKind, MetadataMode, SourceMetadata, exif, icc};
use crate::outcome::{KeepReason, Refusal};
use crate::preset::{Overrides, Speed};
use crate::toolchain::{LIBJXL, Toolchain};

/// The ravif version in `Cargo.lock`; a test keeps the two equal.
pub const RAVIF_ID: &str = "ravif-0.13.0";
pub const MAX_AVIF_SIDE: u32 = 16_384;
pub const MAX_AVIF_PIXELS: u64 = 64_000_000;
pub const BOMB_PIXELS: u64 = 500_000_000;
pub const MAX_VERIFY_PIXELS: u64 = fast_ssim2::MAX_IMAGE_PIXELS as u64;
pub const MAX_RETRIES: u32 = 2;
pub const GAIN_MAP_NOT_IN_JXL: &str = "the HDR gain map was dropped: JPEG XL output can't carry it";

const LEARNED_HEADROOM: f64 = 1.0;
const RAISE_QUALITY_MARGIN: f64 = 1.0;
const RAISE_DISTANCE_MARGIN: f64 = 0.1;
const MAX_QUALITY: f64 = 100.0;
const MIN_START_QUALITY: f64 = 40.0;
const MIN_DISTANCE: f64 = 0.1;
const MAX_START_DISTANCE: f64 = 6.0;
const MAX_DISTANCE: f64 = 25.0;
pub const RAVIF_QUALITY_RANGE: RangeInclusive<f64> = 1.0..=MAX_QUALITY;
pub const JXL_DISTANCE_RANGE: RangeInclusive<f64> = MIN_DISTANCE..=MAX_DISTANCE;
const LEARNED_QUALITY_BELOW_START: f64 = 8.0;
const LEARNED_DISTANCE_ABOVE_START: f64 = 1.0;
const STEP_DOWN_ABOVE: f64 = 2.0;
const STEP_DOWN_AIM: f64 = 0.75;
const STEP_DOWN_MIN_QUALITY: f64 = 0.5;
const STEP_DOWN_MIN_DISTANCE: f64 = 0.05;
/// How far past the largest size that saves enough a first encode may land and still be verified for a step-down.
pub const STEP_DOWN_SIZE_REACH: f64 = 1.25;

pub(crate) const CMYK_COMPONENTS: u8 = 4;
const YCCK_TRANSFORM: u8 = 2;
const RAVIF_BYTES_PER_PIXEL: u64 = 34;
const LIBJXL_BASE_BYTES: u64 = 250 << 20;
const LIBJXL_BYTES_PER_PIXEL: u64 = 7;
const DECODE_BYTES_PER_PIXEL: u64 = 8;
const VERIFY_BYTES_PER_PIXEL: u64 = 36;
const JXL_LOSSLESS: JxlSettings = JxlSettings {
	distance: 0.0,
	effort: 8,
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RavifSettings {
	pub quality: f32,
	pub speed: u8,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JxlSettings {
	pub distance: f32,
	pub effort: u8,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Candidate {
	Ravif(RavifSettings),
	JxlLossy(JxlSettings),
	JxlLossless(JxlSettings),
}

#[derive(Debug, Clone, PartialEq)]
pub struct StillPlan {
	pub candidates: Vec<Candidate>,
	pub target: f64,
	pub memory_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct StillFacts {
	pub width: u32,
	pub height: u32,
	pub effective: Effective,
	pub graphic: bool,
	pub grey_icc: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
	Damaged(String),
	Keep(KeepReason),
}

#[derive(Debug, Clone, Copy)]
pub struct PrecheckInput<'a> {
	pub structure: &'a Structure,
	pub metadata: &'a SourceMetadata,
	pub tiff_pages: Option<u32>,
	pub dimensions: Option<(u32, u32)>,
	pub drop_aux: bool,
	pub mode: MetadataMode,
}

impl Candidate {
	pub fn extension(self) -> &'static str {
		match self {
			Self::Ravif(_) => "avif",
			Self::JxlLossy(_) | Self::JxlLossless(_) => "jxl",
		}
	}

	pub fn setting(self) -> Option<f64> {
		match self {
			Self::Ravif(settings) => Some(f64::from(settings.quality)),
			Self::JxlLossy(settings) => Some(f64::from(settings.distance)),
			Self::JxlLossless(_) => None,
		}
	}

	/// This candidate at `setting`, kept near the start that `speed` and `target` plan.
	#[must_use]
	#[allow(clippy::cast_possible_truncation)]
	pub fn with_setting(self, speed: Speed, target: f64, setting: f64) -> Self {
		match self {
			Self::Ravif(settings) => {
				let start = f64::from(ravif_start(speed, target).quality);
				Self::Ravif(RavifSettings {
					quality: setting.clamp(start - LEARNED_QUALITY_BELOW_START, MAX_QUALITY) as f32,
					..settings
				})
			}
			Self::JxlLossy(settings) => {
				let start = f64::from(jxl_lossy_start(speed, target).distance);
				Self::JxlLossy(JxlSettings {
					distance: setting.clamp(MIN_DISTANCE, start + LEARNED_DISTANCE_ABOVE_START) as f32,
					..settings
				})
			}
			Self::JxlLossless(_) => self,
		}
	}

	#[allow(clippy::cast_possible_truncation)]
	pub fn raised(self, score: f64, target: f64) -> Option<Self> {
		let deficit = target - score;

		match self {
			Self::Ravif(settings) => {
				let quality = f64::from(settings.quality);
				let raised = (quality + deficit / tables().images.ravif_points_per_quality + RAISE_QUALITY_MARGIN)
					.min(MAX_QUALITY);
				(raised > quality).then_some(Self::Ravif(RavifSettings {
					quality: raised as f32,
					..settings
				}))
			}
			Self::JxlLossy(settings) => {
				let distance = f64::from(settings.distance);
				let lowered = (distance - deficit / tables().images.jxl_points_per_distance - RAISE_DISTANCE_MARGIN)
					.max(MIN_DISTANCE);
				(lowered < distance).then_some(Self::JxlLossy(JxlSettings {
					distance: lowered as f32,
					..settings
				}))
			}
			Self::JxlLossless(_) => None,
		}
	}

	pub fn needed_setting(self, score: f64, target: f64, speed: Speed) -> Option<f64> {
		let needed = self.aimed_setting(score - target - LEARNED_HEADROOM)?;
		self.with_setting(speed, target, needed).setting()
	}

	/// The one lower encode for a pass that cleared `target` by more than `STEP_DOWN_ABOVE`, aimed `STEP_DOWN_AIM`
	/// above it; `None` when it would hardly differ from this one.
	pub fn stepped_down(self, score: f64, target: f64, speed: Speed) -> Option<Self> {
		if score - target <= STEP_DOWN_ABOVE {
			return None;
		}

		let aimed = self.aimed_setting(score - target - STEP_DOWN_AIM)?;
		let lowered = self.with_setting(speed, target, aimed);
		let change = match (self, lowered) {
			(Self::Ravif(current), Self::Ravif(lowered)) => {
				f64::from(current.quality - lowered.quality) >= STEP_DOWN_MIN_QUALITY
			}
			(Self::JxlLossy(current), Self::JxlLossy(lowered)) => {
				f64::from(lowered.distance - current.distance) >= STEP_DOWN_MIN_DISTANCE
			}
			_ => false,
		};
		change.then_some(lowered)
	}

	/// A `setting` this passing candidate teaches, kept a raise margin finer than `missed`, its lower encode that fell
	/// below the target, but never finer than this candidate itself.
	pub fn learned_past_miss(self, missed: Self, setting: f64) -> f64 {
		match (self, missed) {
			(Self::Ravif(kept), Self::Ravif(missed)) => {
				setting.max((f64::from(missed.quality) + RAISE_QUALITY_MARGIN).min(f64::from(kept.quality)))
			}
			(Self::JxlLossy(kept), Self::JxlLossy(missed)) => {
				setting.min((f64::from(missed.distance) - RAISE_DISTANCE_MARGIN).max(f64::from(kept.distance)))
			}
			_ => setting,
		}
	}

	/// The setting expected to score `spare` points lower than this one did.
	fn aimed_setting(self, spare: f64) -> Option<f64> {
		match self {
			Self::Ravif(settings) => {
				Some(f64::from(settings.quality) - spare / tables().images.ravif_points_per_quality)
			}
			Self::JxlLossy(settings) => {
				Some(f64::from(settings.distance) + spare / tables().images.jxl_points_per_distance)
			}
			Self::JxlLossless(_) => None,
		}
	}

	pub fn finer_than(self, other: Self) -> bool {
		match (self, other) {
			(Self::Ravif(this), Self::Ravif(other)) => this.quality > other.quality,
			(Self::JxlLossy(this), Self::JxlLossy(other)) => this.distance < other.distance,
			_ => false,
		}
	}

	pub fn encoder_id(self, tools: &Toolchain) -> String {
		match self {
			Self::Ravif(_) => RAVIF_ID.to_owned(),
			Self::JxlLossy(_) => tools.encoder_id(LIBJXL),
			Self::JxlLossless(_) => tools.encoder_id("libjxl-lossless"),
		}
	}

	pub fn preset(self) -> u8 {
		match self {
			Self::Ravif(settings) => settings.speed,
			Self::JxlLossy(settings) | Self::JxlLossless(settings) => settings.effort,
		}
	}
}

pub fn ssim_target(speed: Speed) -> f64 {
	match speed {
		Speed::Fast => 80.0,
		Speed::Moderate => 85.0,
		Speed::Slow => 90.0,
	}
}

pub fn ravif_settings(speed: Speed) -> RavifSettings {
	let (quality, speed) = match speed {
		Speed::Fast => (88.0, 9),
		Speed::Moderate => (92.0, 6),
		Speed::Slow => (95.0, 4),
	};
	RavifSettings { quality, speed }
}

pub fn jxl_lossy_settings(speed: Speed) -> JxlSettings {
	let (distance, effort) = match speed {
		Speed::Fast => (1.6, 5),
		Speed::Moderate => (1.0, 7),
		Speed::Slow => (0.5, 7),
	};
	JxlSettings { distance, effort }
}

/// The speed's table quality moved by how far `target` is from the speed's own target.
#[allow(clippy::cast_possible_truncation)]
pub fn ravif_start(speed: Speed, target: f64) -> RavifSettings {
	let table = ravif_settings(speed);
	let shift = (target - ssim_target(speed)) / tables().images.ravif_points_per_quality;
	RavifSettings {
		quality: (f64::from(table.quality) + shift).clamp(MIN_START_QUALITY, MAX_QUALITY) as f32,
		..table
	}
}

/// The speed's table distance moved by how far `target` is from the speed's own target.
#[allow(clippy::cast_possible_truncation)]
pub fn jxl_lossy_start(speed: Speed, target: f64) -> JxlSettings {
	let table = jxl_lossy_settings(speed);
	let shift = (target - ssim_target(speed)) / tables().images.jxl_points_per_distance;
	JxlSettings {
		distance: (f64::from(table.distance) - shift).clamp(MIN_DISTANCE, MAX_START_DISTANCE) as f32,
		..table
	}
}

/// Passes a source that may be converted, with the warnings for what converting it drops.
pub fn precheck(input: &PrecheckInput<'_>) -> Result<Vec<String>, Gate> {
	if let Some(damage) = &input.structure.damage {
		return Err(Gate::Damaged(damage.clone()));
	}

	let metadata = input.metadata;
	if metadata.jpeg_components == Some(CMYK_COMPONENTS) {
		let kind = if metadata.adobe_transform == Some(YCCK_TRANSFORM) {
			"YCCK"
		} else {
			"CMYK"
		};
		return Err(keep(format!("{kind} JPEG files are never converted")));
	}
	if let Some(pages) = input.tiff_pages.filter(|pages| *pages > 1) {
		return Err(keep(format!(
			"multi-page TIFF files ({pages} pages) are never converted"
		)));
	}

	if let Some(problem) = &metadata.icc_problem {
		return Err(Gate::Keep(KeepReason::InvalidIcc(problem.clone())));
	}
	if let Some(space) = metadata.icc.as_deref().and_then(icc::colour_space)
		&& &space != b"RGB "
		&& &space != b"GRAY"
	{
		let space = String::from_utf8_lossy(&space).trim().to_owned();
		return Err(Gate::Keep(KeepReason::InvalidIcc(format!(
			"it describes {space} data, not RGB or grey"
		))));
	}

	let mut warnings = Vec::new();
	if !input.drop_aux {
		let (depth, kept): (Vec<&AuxKind>, Vec<&AuxKind>) = metadata
			.aux_without_gain_map()
			.into_iter()
			.partition(|aux| matches!(aux, AuxKind::Depth(_)));
		if !kept.is_empty() {
			let kinds: Vec<String> = kept.iter().map(ToString::to_string).collect();
			return Err(keep(format!(
				"it has {}, which converting would drop",
				kinds.join(", ")
			)));
		}

		warnings.extend(
			depth
				.into_iter()
				.map(|aux| format!("the depth map was dropped ({})", aux.detail())),
		);
	}

	if input.mode == MetadataMode::IccOnly
		&& metadata.icc.is_none()
		&& metadata.exif.as_deref().is_some_and(exif::adobe_rgb_only)
	{
		return Err(keep("colour space defined only in EXIF metadata".into()));
	}

	let (width, height) = input
		.dimensions
		.ok_or_else(|| Gate::Damaged("the image dimensions are unreadable".into()))?;
	let pixels = u64::from(width) * u64::from(height);
	if pixels > BOMB_PIXELS {
		return Err(Gate::Keep(KeepReason::DecodeBomb {
			megapixels: pixels / 1_000_000,
		}));
	}
	if pixels > MAX_VERIFY_PIXELS {
		return Err(keep(format!("{} MP is too large to verify", pixels / 1_000_000)));
	}
	Ok(warnings)
}

fn keep(reason: String) -> Gate {
	Gate::Keep(KeepReason::Unsupported(reason))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
pub enum ImageRoute {
	Avif,
	Jxl,
	JxlLossless,
}

pub fn image_target(speed: Speed, overrides: &Overrides) -> f64 {
	overrides.normalized().image.unwrap_or(ssim_target(speed))
}

pub fn plan_still(
	facts: &StillFacts,
	speed: Speed,
	overrides: &Overrides,
	libjxl: bool,
	route: Option<ImageRoute>,
) -> Result<StillPlan, Refusal> {
	let pixels = u64::from(facts.width) * u64::from(facts.height);
	let fits_avif = facts.width <= MAX_AVIF_SIDE
		&& facts.height <= MAX_AVIF_SIDE
		&& pixels <= MAX_AVIF_PIXELS
		&& !facts.effective.high_depth
		&& !facts.grey_icc;
	let target = image_target(speed, overrides);
	let avif = Candidate::Ravif(ravif_start(speed, target));
	let lossy_jxl = Candidate::JxlLossy(jxl_lossy_start(speed, target));
	let lossless_jxl = Candidate::JxlLossless(JXL_LOSSLESS);

	let candidates = match route {
		Some(ImageRoute::Avif) if !fits_avif => return Err(Refusal::Impossible(not_avif(facts))),
		Some(ImageRoute::Avif) => vec![avif],
		Some(ImageRoute::Jxl | ImageRoute::JxlLossless) if !libjxl => {
			return Err(Refusal::Impossible("JPEG XL output needs ffmpeg with libjxl".into()));
		}
		Some(ImageRoute::Jxl) if facts.graphic => vec![lossy_jxl, lossless_jxl],
		Some(ImageRoute::Jxl) => vec![lossy_jxl],
		Some(ImageRoute::JxlLossless) => vec![lossless_jxl],
		None => {
			let mut candidates = Vec::new();
			if fits_avif {
				candidates.push(avif);
			} else if libjxl {
				candidates.push(lossy_jxl);
			} else {
				return Err(Refusal::Keep(KeepReason::Unsupported(
					"this image needs JPEG XL, and ffmpeg has no libjxl".into(),
				)));
			}

			if facts.graphic && libjxl {
				candidates.push(lossless_jxl);
			}
			candidates
		}
	};

	Ok(StillPlan {
		memory_bytes: memory_estimate(facts.width, facts.height, &candidates),
		candidates,
		target,
	})
}

/// Narrows `plan` to the candidates that carry the source's HDR gain map, which only AVIF keeps. `Ok(false)` leaves a
/// requested JPEG XL route as it is, encoded without the gain map.
pub fn carry_gain_map(plan: &mut StillPlan, facts: &StillFacts, route: Option<ImageRoute>) -> Result<bool, KeepReason> {
	if matches!(route, Some(ImageRoute::Jxl | ImageRoute::JxlLossless)) {
		return Ok(false);
	}

	let avif: Vec<Candidate> = plan
		.candidates
		.iter()
		.copied()
		.filter(|candidate| matches!(candidate, Candidate::Ravif(_)))
		.collect();
	if avif.is_empty() {
		return Err(KeepReason::Unsupported(format!(
			"it has an HDR gain map, which only AVIF keeps, and {}",
			not_avif(facts)
		)));
	}

	plan.memory_bytes = memory_estimate(facts.width, facts.height, &avif);
	plan.candidates = avif;
	Ok(true)
}

/// The warning when `carry_gain_map` refuses and the gain map is dropped instead.
pub fn gain_map_not_in_avif(facts: &StillFacts) -> String {
	format!(
		"the HDR gain map was dropped: only AVIF keeps it, and {}",
		not_avif(facts)
	)
}

fn not_avif(facts: &StillFacts) -> String {
	let why = if facts.effective.high_depth {
		"it has more than 8 bits per channel"
	} else if facts.grey_icc {
		"it has a grey colour profile"
	} else {
		"it is larger than 16384 pixels on a side or 64 MP"
	};
	format!("this image can't be stored as AVIF: {why}")
}

/// The estimate before decoding, while the route isn't known yet: the larger of the ravif and libjxl estimates.
pub fn worst_case_memory(width: u32, height: u32) -> u64 {
	memory_estimate(
		width,
		height,
		&[
			Candidate::Ravif(ravif_settings(Speed::Slow)),
			Candidate::JxlLossy(jxl_lossy_settings(Speed::Slow)),
		],
	)
}

pub fn memory_estimate(width: u32, height: u32, candidates: &[Candidate]) -> u64 {
	let pixels = u64::from(width) * u64::from(height);
	let encoder = candidates
		.iter()
		.map(|candidate| match candidate {
			Candidate::Ravif(_) => pixels.saturating_mul(RAVIF_BYTES_PER_PIXEL),
			Candidate::JxlLossy(_) | Candidate::JxlLossless(_) => pixels
				.saturating_mul(LIBJXL_BYTES_PER_PIXEL)
				.saturating_add(LIBJXL_BASE_BYTES),
		})
		.max()
		.unwrap_or(0);
	let (verify_width, verify_height) = padded_size((width, height));
	let verify_pixels = u64::from(verify_width) * u64::from(verify_height);

	pixels
		.saturating_mul(DECODE_BYTES_PER_PIXEL)
		.saturating_add(verify_pixels.saturating_mul(VERIFY_BYTES_PER_PIXEL))
		.saturating_add(encoder)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::metadata::AuxKind;

	const OPAQUE_8_BIT: Effective = Effective {
		high_depth: false,
		alpha: false,
	};

	fn facts(width: u32, height: u32) -> StillFacts {
		StillFacts {
			width,
			height,
			effective: OPAQUE_8_BIT,
			graphic: false,
			grey_icc: false,
		}
	}

	fn kinds(plan: &StillPlan) -> Vec<&'static str> {
		plan.candidates
			.iter()
			.map(|candidate| match candidate {
				Candidate::Ravif(_) => "ravif",
				Candidate::JxlLossy(_) => "jxl lossy",
				Candidate::JxlLossless(_) => "jxl lossless",
			})
			.collect()
	}

	#[test]
	fn photos_within_avif_limits_use_ravif_and_the_rest_use_jxl() {
		let deep = StillFacts {
			effective: Effective {
				high_depth: true,
				alpha: false,
			},
			..facts(800, 600)
		};
		let grey = StillFacts {
			grey_icc: true,
			..facts(800, 600)
		};

		assert_eq!(
			kinds(&plan_still(&facts(6000, 4000), Speed::Moderate, &Overrides::default(), true, None).unwrap()),
			["ravif"]
		);
		assert_eq!(
			kinds(&plan_still(&facts(1293, 48277), Speed::Moderate, &Overrides::default(), true, None).unwrap()),
			["jxl lossy"]
		);
		assert_eq!(
			kinds(&plan_still(&facts(20000, 800), Speed::Moderate, &Overrides::default(), true, None).unwrap()),
			["jxl lossy"]
		);
		assert_eq!(
			kinds(&plan_still(&deep, Speed::Moderate, &Overrides::default(), true, None).unwrap()),
			["jxl lossy"]
		);
		assert_eq!(
			kinds(&plan_still(&grey, Speed::Moderate, &Overrides::default(), true, None).unwrap()),
			["jxl lossy"]
		);
	}

	#[test]
	fn graphics_also_try_lossless_jxl_when_libjxl_exists() {
		let graphic = StillFacts {
			graphic: true,
			..facts(800, 600)
		};

		assert_eq!(
			kinds(&plan_still(&graphic, Speed::Fast, &Overrides::default(), true, None).unwrap()),
			["ravif", "jxl lossless"]
		);
		assert_eq!(
			kinds(&plan_still(&graphic, Speed::Fast, &Overrides::default(), false, None).unwrap()),
			["ravif"]
		);
		assert!(matches!(
			plan_still(&facts(1293, 48277), Speed::Fast, &Overrides::default(), false, None),
			Err(Refusal::Keep(KeepReason::Unsupported(reason))) if reason.contains("libjxl")
		));
	}

	#[test]
	fn a_requested_route_replaces_the_automatic_choice() {
		let graphic = StillFacts {
			graphic: true,
			..facts(800, 600)
		};
		let deep = StillFacts {
			effective: Effective {
				high_depth: true,
				alpha: false,
			},
			..facts(800, 600)
		};

		assert_eq!(
			kinds(
				&plan_still(
					&facts(800, 600),
					Speed::Fast,
					&Overrides::default(),
					true,
					Some(ImageRoute::Jxl)
				)
				.unwrap()
			),
			["jxl lossy"]
		);
		assert_eq!(
			kinds(
				&plan_still(
					&graphic,
					Speed::Fast,
					&Overrides::default(),
					true,
					Some(ImageRoute::Jxl)
				)
				.unwrap()
			),
			["jxl lossy", "jxl lossless"]
		);
		assert_eq!(
			kinds(
				&plan_still(
					&graphic,
					Speed::Fast,
					&Overrides::default(),
					true,
					Some(ImageRoute::Avif)
				)
				.unwrap()
			),
			["ravif"]
		);
		assert_eq!(
			kinds(
				&plan_still(
					&graphic,
					Speed::Fast,
					&Overrides::default(),
					true,
					Some(ImageRoute::JxlLossless)
				)
				.unwrap()
			),
			["jxl lossless"]
		);
		assert!(matches!(
			plan_still(&deep, Speed::Fast, &Overrides::default(), true, Some(ImageRoute::Avif)),
			Err(Refusal::Impossible(reason)) if reason.contains("more than 8 bits")
		));
		assert!(matches!(
			plan_still(&facts(800, 600), Speed::Fast, &Overrides::default(), false, Some(ImageRoute::Jxl)),
			Err(Refusal::Impossible(reason)) if reason.contains("libjxl")
		));
	}

	#[test]
	fn images_too_large_for_the_quality_metric_keep_the_original_before_decoding() {
		let healthy = Structure::default();
		let plain = SourceMetadata::default();

		assert!(matches!(
			precheck(&PrecheckInput {
				dimensions: Some((20_000, 15_000)),
				..input(&healthy, &plain)
			}),
			Err(Gate::Keep(KeepReason::Unsupported(reason))) if reason.contains("300 MP is too large to verify")
		));
		assert_eq!(
			precheck(&PrecheckInput {
				dimensions: Some((16_000, 16_000)),
				..input(&healthy, &plain)
			}),
			Ok(Vec::new())
		);
	}

	#[test]
	fn speed_tables_follow_the_rulings() {
		assert_eq!(
			ravif_settings(Speed::Fast),
			RavifSettings {
				quality: 88.0,
				speed: 9
			}
		);
		assert_eq!(
			ravif_settings(Speed::Moderate),
			RavifSettings {
				quality: 92.0,
				speed: 6
			}
		);
		assert_eq!(
			ravif_settings(Speed::Slow),
			RavifSettings {
				quality: 95.0,
				speed: 4
			}
		);
		assert_eq!(
			jxl_lossy_settings(Speed::Fast),
			JxlSettings {
				distance: 1.6,
				effort: 5
			}
		);
		assert_eq!(
			jxl_lossy_settings(Speed::Moderate),
			JxlSettings {
				distance: 1.0,
				effort: 7
			}
		);
		assert_eq!(
			jxl_lossy_settings(Speed::Slow),
			JxlSettings {
				distance: 0.5,
				effort: 7
			}
		);
		assert_eq!(
			(
				ssim_target(Speed::Fast),
				ssim_target(Speed::Moderate),
				ssim_target(Speed::Slow)
			),
			(80.0, 85.0, 90.0)
		);
	}

	#[test]
	fn an_image_override_replaces_the_target_at_every_speed() {
		let custom = Overrides {
			image: Some(60.0),
			..Overrides::default()
		};

		for speed in [Speed::Fast, Speed::Moderate, Speed::Slow] {
			let target =
				|overrides: &Overrides| plan_still(&facts(64, 48), speed, overrides, true, None).unwrap().target;

			assert_eq!(
				(target(&custom), target(&Overrides::default())),
				(60.0, ssim_target(speed))
			);
		}
	}

	#[test]
	fn the_image_target_reads_the_normalized_override() {
		let image = |target: f64| Overrides {
			image: Some(target),
			..Overrides::default()
		};

		assert!((image_target(Speed::Fast, &image(200.0)) - 95.0).abs() < 1e-9);
		assert!((image_target(Speed::Fast, &image(10.0)) - 50.0).abs() < 1e-9);
		assert!((image_target(Speed::Moderate, &image(80.04)) - 80.0).abs() < 1e-9);
		assert!((image_target(Speed::Slow, &image(f64::NAN)) - ssim_target(Speed::Slow)).abs() < 1e-9);
	}

	#[test]
	fn memory_is_estimated_per_encoder() {
		let pixels = 10_800 * 5_400;

		assert_eq!(
			memory_estimate(10_800, 5_400, &[Candidate::Ravif(ravif_settings(Speed::Fast))]),
			pixels * (8 + 36 + 34)
		);
		assert_eq!(
			memory_estimate(10_800, 5_400, &[Candidate::JxlLossy(jxl_lossy_settings(Speed::Fast))]),
			pixels * (8 + 36 + 7) + (250 << 20)
		);
	}

	#[test]
	fn the_worst_case_covers_both_encoders() {
		assert_eq!(worst_case_memory(4000, 3000), 12_000_000 * (8 + 36 + 34));
		assert_eq!(
			worst_case_memory(100, 100),
			10_000 * (8 + 7) + 256 * 256 * 36 + (250 << 20)
		);
		assert_eq!(
			worst_case_memory(48, 16_500),
			48 * 16_500 * (8 + 7) + 256 * 16_500 * 36 + (250 << 20)
		);
	}

	#[test]
	fn the_start_moves_with_the_target() {
		for speed in [Speed::Fast, Speed::Moderate, Speed::Slow] {
			assert_eq!(ravif_start(speed, ssim_target(speed)), ravif_settings(speed));
			assert_eq!(jxl_lossy_start(speed, ssim_target(speed)), jxl_lossy_settings(speed));
		}

		assert_eq!(
			ravif_start(Speed::Moderate, 70.0),
			RavifSettings {
				quality: 80.0,
				speed: 6
			}
		);
		assert_eq!(ravif_start(Speed::Moderate, 95.0).quality, 100.0);
		assert_eq!(ravif_start(Speed::Fast, -100.0).quality, 40.0);
		assert_eq!(
			jxl_lossy_start(Speed::Moderate, 70.0),
			JxlSettings {
				distance: 2.875,
				effort: 7
			}
		);
		assert!((jxl_lossy_start(Speed::Moderate, 95.0).distance - 0.1).abs() < 1e-6);
		assert_eq!(jxl_lossy_start(Speed::Fast, 0.0).distance, 6.0);
	}

	#[test]
	fn planned_candidates_start_from_the_target() {
		let target = |image: f64| Overrides {
			image: Some(image),
			..Overrides::default()
		};
		let planned = |overrides: &Overrides, route| {
			plan_still(&facts(800, 600), Speed::Moderate, overrides, true, route)
				.unwrap()
				.candidates[0]
				.setting()
				.unwrap()
		};

		assert_eq!(planned(&target(70.0), None), 80.0);
		assert_eq!(planned(&target(70.0), Some(ImageRoute::Avif)), 80.0);
		assert_eq!(planned(&Overrides::default(), None), 92.0);
		assert_eq!(planned(&target(70.0), Some(ImageRoute::Jxl)), 2.875);
		assert_eq!(planned(&Overrides::default(), Some(ImageRoute::Jxl)), 1.0);
	}

	#[test]
	fn learned_settings_stay_near_the_start() {
		let ravif = Candidate::Ravif(ravif_settings(Speed::Moderate));
		let jxl = Candidate::JxlLossy(jxl_lossy_settings(Speed::Moderate));
		let lossless = Candidate::JxlLossless(JXL_LOSSLESS);

		assert_eq!(ravif.with_setting(Speed::Moderate, 85.0, 60.0).setting(), Some(84.0));
		assert_eq!(ravif.with_setting(Speed::Moderate, 85.0, 120.0).setting(), Some(100.0));
		assert_eq!(ravif.with_setting(Speed::Moderate, 85.0, 90.0).setting(), Some(90.0));
		assert_eq!(ravif.with_setting(Speed::Moderate, 70.0, 70.0).setting(), Some(72.0));
		assert_eq!(ravif.with_setting(Speed::Moderate, 70.0, 75.0).setting(), Some(75.0));
		assert_eq!(jxl.with_setting(Speed::Moderate, 85.0, 5.0).setting(), Some(2.0));
		assert_eq!(
			jxl.with_setting(Speed::Moderate, 85.0, 0.0)
				.setting()
				.map(|distance| (distance * 10.0).round()),
			Some(1.0)
		);
		assert_eq!(jxl.with_setting(Speed::Moderate, 70.0, 5.0).setting(), Some(3.875));
		assert_eq!(jxl.with_setting(Speed::Moderate, 70.0, 3.5).setting(), Some(3.5));
		assert_eq!(lossless.with_setting(Speed::Moderate, 85.0, 3.0), lossless);
		assert_eq!(lossless.setting(), None);
	}

	#[test]
	fn only_a_pass_well_above_the_target_steps_down_and_only_by_a_real_change() {
		let ravif = Candidate::Ravif(ravif_settings(Speed::Moderate));
		let jxl = Candidate::JxlLossy(jxl_lossy_settings(Speed::Moderate));
		let at_floor = ravif.with_setting(Speed::Moderate, 85.0, 0.0);
		let at_ceiling = jxl.with_setting(Speed::Moderate, 85.0, 25.0);
		let stepped = |candidate: Candidate, score: f64| {
			candidate
				.stepped_down(score, 85.0, Speed::Moderate)
				.and_then(Candidate::setting)
		};

		let lowered = stepped(ravif, 89.0).unwrap();
		assert!((lowered - 89.4).abs() < 1e-4, "{lowered}");
		assert_eq!(stepped(ravif, 87.0), None);
		assert_eq!(stepped(ravif, 86.0), None);
		assert_eq!(at_floor.setting(), Some(84.0));
		assert_eq!(stepped(at_floor, 95.0), None);
		assert_eq!(
			stepped(
				Candidate::Ravif(RavifSettings {
					quality: 84.4,
					speed: 6
				}),
				95.0
			),
			None
		);

		let lowered = stepped(jxl, 89.0).unwrap();
		assert!((lowered - 1.40625).abs() < 1e-6, "{lowered}");
		assert_eq!(stepped(jxl, 87.0), None);
		assert_eq!(at_ceiling.setting(), Some(2.0));
		assert_eq!(stepped(at_ceiling, 95.0), None);
		assert_eq!(
			Candidate::JxlLossless(JXL_LOSSLESS).stepped_down(99.0, 85.0, Speed::Moderate),
			None
		);
	}

	#[test]
	fn after_a_missed_step_down_the_group_learns_a_raise_margin_finer_than_the_miss() {
		let ravif = |quality| Candidate::Ravif(RavifSettings { quality, speed: 9 });
		let jxl = |distance| Candidate::JxlLossy(JxlSettings { distance, effort: 5 });
		let near = |learned: f64, expected: f64| (learned - expected).abs() < 1e-4;

		assert!(near(ravif(88.0).learned_past_miss(ravif(83.125), 83.3), 84.125));
		assert!(near(ravif(88.0).learned_past_miss(ravif(83.125), 86.0), 86.0));
		assert!(near(ravif(88.0).learned_past_miss(ravif(87.5), 83.3), 88.0));
		assert!(near(jxl(1.0).learned_past_miss(jxl(1.625), 1.75), 1.525));
		assert!(near(jxl(1.0).learned_past_miss(jxl(1.625), 1.25), 1.25));
		assert!(near(jxl(1.0).learned_past_miss(jxl(1.0625), 1.75), 1.0));
		assert!(near(ravif(88.0).learned_past_miss(jxl(1.0), 83.3), 83.3));
	}

	#[test]
	fn a_step_down_stays_near_the_start_of_a_custom_target() {
		let start = Candidate::Ravif(ravif_start(Speed::Moderate, 70.0));

		assert_eq!(
			start
				.stepped_down(95.0, 70.0, Speed::Moderate)
				.and_then(Candidate::setting),
			Some(72.0)
		);
	}

	#[test]
	fn a_miss_raises_the_quality_by_the_deficit() {
		let ravif = Candidate::Ravif(RavifSettings {
			quality: 88.0,
			speed: 9,
		});
		let jxl = Candidate::JxlLossy(JxlSettings {
			distance: 1.6,
			effort: 5,
		});

		assert_eq!(ravif.raised(75.0, 80.0).and_then(Candidate::setting), Some(93.0));
		assert_eq!(ravif.raised(99.0, 80.0), None);
		assert_eq!(
			Candidate::Ravif(RavifSettings {
				quality: 100.0,
				speed: 9
			})
			.raised(70.0, 80.0),
			None
		);
		let lowered = jxl.raised(76.0, 80.0).and_then(Candidate::setting).unwrap();
		assert!((lowered - 1.0).abs() < 1e-6, "{lowered}");
		assert_eq!(Candidate::JxlLossless(JXL_LOSSLESS).raised(10.0, 80.0), None);
	}

	#[test]
	fn a_pass_teaches_the_setting_that_would_have_cleared_the_floor_by_one() {
		let ravif = Candidate::Ravif(ravif_settings(Speed::Moderate));

		let generous = ravif.needed_setting(90.0, 85.0, Speed::Moderate).unwrap();
		let narrow = ravif.needed_setting(85.5, 85.0, Speed::Moderate).unwrap();
		let custom = Candidate::Ravif(ravif_start(Speed::Moderate, 70.0))
			.needed_setting(95.0, 70.0, Speed::Moderate)
			.unwrap();

		assert!((generous - 88.8).abs() < 1e-4, "{generous}");
		assert!((narrow - 92.4).abs() < 1e-4, "{narrow}");
		assert!((custom - 72.0).abs() < 1e-4, "{custom}");
		assert_eq!(
			Candidate::JxlLossless(JXL_LOSSLESS).needed_setting(99.0, 85.0, Speed::Moderate),
			None
		);
		assert_eq!(ravif.preset(), 6);
	}

	#[test]
	fn the_ravif_id_names_the_locked_ravif_version() {
		let lock = include_str!("../../../../Cargo.lock");
		let locked = lock
			.split("[[package]]")
			.find(|package| package.lines().any(|line| line.trim_end() == "name = \"ravif\""))
			.and_then(|package| package.lines().find_map(|line| line.strip_prefix("version = ")))
			.map(|version| version.trim_end().trim_matches('"'))
			.unwrap();

		assert_eq!(RAVIF_ID, format!("ravif-{locked}"));
	}

	#[test]
	fn only_a_finer_setting_of_the_same_encoder_is_finer() {
		let ravif = Candidate::Ravif(ravif_settings(Speed::Fast));
		let jxl = Candidate::JxlLossy(jxl_lossy_settings(Speed::Fast));

		assert!(ravif.with_setting(Speed::Fast, 80.0, 96.0).finer_than(ravif));
		assert!(!ravif.with_setting(Speed::Fast, 80.0, 84.0).finer_than(ravif));
		assert!(!ravif.finer_than(ravif));
		assert!(jxl.with_setting(Speed::Fast, 80.0, 1.0).finer_than(jxl));
		assert!(!jxl.with_setting(Speed::Fast, 80.0, 2.0).finer_than(jxl));
		assert!(!Candidate::JxlLossless(JXL_LOSSLESS).finer_than(jxl));
	}

	fn input<'a>(structure: &'a Structure, metadata: &'a SourceMetadata) -> PrecheckInput<'a> {
		PrecheckInput {
			structure,
			metadata,
			tiff_pages: None,
			dimensions: Some((800, 600)),
			drop_aux: false,
			mode: MetadataMode::IccOnly,
		}
	}

	#[test]
	fn prechecks_keep_the_original_for_never_converted_or_unsafe_sources() {
		let healthy = Structure::default();
		let damaged = Structure {
			damage: Some("truncated".into()),
			warnings: Vec::new(),
		};
		let ycck = SourceMetadata {
			jpeg_components: Some(4),
			adobe_transform: Some(2),
			..SourceMetadata::default()
		};
		let bad_icc = SourceMetadata {
			icc_problem: Some("declares 4294967295 bytes".into()),
			..SourceMetadata::default()
		};
		let plain = SourceMetadata::default();
		let unsupported = |gate: Result<Vec<String>, Gate>| match gate {
			Err(Gate::Keep(KeepReason::Unsupported(reason))) => reason,
			other => panic!("{other:?}"),
		};

		assert_eq!(
			precheck(&input(&damaged, &plain)),
			Err(Gate::Damaged("truncated".into()))
		);
		assert_eq!(
			unsupported(precheck(&input(&healthy, &ycck))),
			"YCCK JPEG files are never converted"
		);
		assert!(
			unsupported(precheck(&PrecheckInput {
				tiff_pages: Some(3),
				..input(&healthy, &plain)
			}))
			.contains("3 pages")
		);
		assert!(matches!(
			precheck(&input(&healthy, &bad_icc)),
			Err(Gate::Keep(KeepReason::InvalidIcc(_)))
		));
		assert_eq!(
			precheck(&PrecheckInput {
				dimensions: Some((100_000, 100_000)),
				..input(&healthy, &plain)
			}),
			Err(Gate::Keep(KeepReason::DecodeBomb { megapixels: 10_000 }))
		);
		assert!(matches!(
			precheck(&PrecheckInput {
				dimensions: None,
				..input(&healthy, &plain)
			}),
			Err(Gate::Damaged(_))
		));
		assert_eq!(precheck(&input(&healthy, &plain)), Ok(Vec::new()));
	}

	#[test]
	fn auxiliary_images_and_exif_only_colour_depend_on_the_options() {
		let healthy = Structure::default();
		let gain_map = SourceMetadata {
			aux: vec![AuxKind::GainMap("Ultra HDR".into())],
			..SourceMetadata::default()
		};
		let mut adobe_exif = b"MM\0*\0\0\0\x08\0\x01\x87\x69\0\x04\0\0\0\x01\0\0\0\x1a\0\0\0\0".to_vec();
		adobe_exif
			.extend_from_slice(b"\0\x02\xa0\x01\0\x03\0\0\0\x01\xff\xff\0\0\xa0\x05\0\x04\0\0\0\x01\0\0\0\x38\0\0\0\0");
		adobe_exif.extend_from_slice(b"\0\x01\0\x01\0\x02\0\0\0\x04R03\0\0\0\0\0");
		let r03 = SourceMetadata {
			exif: Some(adobe_exif),
			..SourceMetadata::default()
		};

		assert!(
			matches!(precheck(&input(&healthy, &gain_map)), Err(Gate::Keep(KeepReason::Unsupported(reason))) if reason == "it has HDR gain map (Ultra HDR), which converting would drop")
		);
		assert_eq!(
			precheck(&PrecheckInput {
				drop_aux: true,
				..input(&healthy, &gain_map)
			}),
			Ok(Vec::new())
		);
		assert!(
			matches!(precheck(&input(&healthy, &r03)), Err(Gate::Keep(KeepReason::Unsupported(reason))) if reason.contains("only in EXIF"))
		);
		assert_eq!(
			precheck(&PrecheckInput {
				mode: MetadataMode::Full,
				..input(&healthy, &r03)
			}),
			Ok(Vec::new())
		);
	}

	#[test]
	fn depth_maps_are_dropped_with_a_warning_and_motion_photos_keep_the_original() {
		let healthy = Structure::default();
		let depth = SourceMetadata {
			aux: vec![
				AuxKind::Depth("Google depth map".into()),
				AuxKind::Depth("MPF disparity image".into()),
				AuxKind::Depth("urn:mpeg:hevc:2015:auxid:2".into()),
			],
			..SourceMetadata::default()
		};
		let motion = SourceMetadata {
			aux: vec![
				AuxKind::Depth("container item".into()),
				AuxKind::MotionVideo("Google motion photo".into()),
				AuxKind::Other("container item 'Panorama'".into()),
			],
			..SourceMetadata::default()
		};

		assert_eq!(
			precheck(&input(&healthy, &depth)),
			Ok(vec![
				"the depth map was dropped (Google depth map)".to_owned(),
				"the depth map was dropped (MPF disparity image)".to_owned(),
				"the depth map was dropped (HEIF auxiliary image)".to_owned(),
			])
		);
		assert_eq!(
			precheck(&PrecheckInput {
				drop_aux: true,
				..input(&healthy, &depth)
			}),
			Ok(Vec::new())
		);
		assert_eq!(
			precheck(&input(&healthy, &motion)),
			Err(keep(
				"it has motion photo video (Google motion photo), auxiliary image (container item 'Panorama'), which \
				 converting would drop"
					.into()
			))
		);
	}

	#[test]
	fn heif_auxiliary_type_urns_read_as_words() {
		let heic = SourceMetadata {
			aux: vec![
				AuxKind::GainMap("urn:com:apple:photo:2020:aux:hdrgainmap".into()),
				AuxKind::Other("urn:com:apple:photo:2019:aux:semanticskinmatte".into()),
				AuxKind::GainMap("tmap item".into()),
			],
			..SourceMetadata::default()
		};

		assert_eq!(
			precheck(&input(&Structure::default(), &heic)),
			Err(keep(
				"it has HDR gain map (HEIF auxiliary image), HEIF auxiliary image, HDR gain map (tmap item), which \
				 converting would drop"
					.into()
			))
		);
	}

	#[test]
	fn a_carried_gain_map_keeps_only_avif_candidates() {
		let graphic = StillFacts {
			graphic: true,
			..facts(800, 600)
		};
		let mut photo = plan_still(&graphic, Speed::Fast, &Overrides::default(), true, None).unwrap();
		let mut requested_jxl = plan_still(
			&graphic,
			Speed::Fast,
			&Overrides::default(),
			true,
			Some(ImageRoute::Jxl),
		)
		.unwrap();
		let mut too_large = plan_still(&facts(20_000, 800), Speed::Fast, &Overrides::default(), true, None).unwrap();
		let untouched = requested_jxl.clone();

		assert_eq!(carry_gain_map(&mut photo, &graphic, None), Ok(true));
		assert_eq!(kinds(&photo), ["ravif"]);
		assert_eq!(photo.memory_bytes, memory_estimate(800, 600, &photo.candidates));
		assert_eq!(
			carry_gain_map(&mut requested_jxl, &graphic, Some(ImageRoute::Jxl)),
			Ok(false)
		);
		assert_eq!(requested_jxl, untouched);
		assert_eq!(
			carry_gain_map(&mut too_large, &facts(20_000, 800), None),
			Err(KeepReason::Unsupported(
				"it has an HDR gain map, which only AVIF keeps, and this image can't be stored as AVIF: it is larger \
				 than 16384 pixels on a side or 64 MP"
					.into()
			))
		);
		assert_eq!(
			gain_map_not_in_avif(&facts(20_000, 800)),
			"the HDR gain map was dropped: only AVIF keeps it, and this image can't be stored as AVIF: it is larger \
			 than 16384 pixels on a side or 64 MP"
		);
	}
}
