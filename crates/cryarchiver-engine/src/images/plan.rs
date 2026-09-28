use crate::images::props::Effective;
use crate::images::structure::Structure;
use crate::metadata::{MetadataMode, SourceMetadata, exif, icc};
use crate::outcome::KeepReason;
use crate::preset::{Overrides, Speed};

pub const MAX_AVIF_SIDE: u32 = 16_384;
pub const MAX_AVIF_PIXELS: u64 = 64_000_000;
pub const BOMB_PIXELS: u64 = 500_000_000;
pub const MAX_VERIFY_PIXELS: u64 = fast_ssim2::MAX_IMAGE_PIXELS as u64;
pub const MAX_RETRIES: u32 = 2;

const RAVIF_POINTS_PER_QUALITY: f64 = 1.25;
const JXL_POINTS_PER_DISTANCE: f64 = 8.0;
const LEARNED_HEADROOM: f64 = 2.0;
const RAISE_QUALITY_MARGIN: f64 = 1.0;
const RAISE_DISTANCE_MARGIN: f64 = 0.1;
const MAX_QUALITY: f64 = 100.0;
const MIN_DISTANCE: f64 = 0.1;
const LEARNED_QUALITY_BELOW_TABLE: f64 = 8.0;
const LEARNED_DISTANCE_ABOVE_TABLE: f64 = 1.0;

const CMYK_COMPONENTS: u8 = 4;
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

	#[must_use]
	#[allow(clippy::cast_possible_truncation)]
	pub fn with_setting(self, speed: Speed, setting: f64) -> Self {
		match self {
			Self::Ravif(settings) => {
				let table = f64::from(ravif_settings(speed).quality);
				Self::Ravif(RavifSettings {
					quality: setting.clamp(table - LEARNED_QUALITY_BELOW_TABLE, MAX_QUALITY) as f32,
					..settings
				})
			}
			Self::JxlLossy(settings) => {
				let table = f64::from(jxl_lossy_settings(speed).distance);
				Self::JxlLossy(JxlSettings {
					distance: setting.clamp(MIN_DISTANCE, table + LEARNED_DISTANCE_ABOVE_TABLE) as f32,
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
				let raised = (quality + deficit / RAVIF_POINTS_PER_QUALITY + RAISE_QUALITY_MARGIN).min(MAX_QUALITY);
				(raised > quality).then_some(Self::Ravif(RavifSettings {
					quality: raised as f32,
					..settings
				}))
			}
			Self::JxlLossy(settings) => {
				let distance = f64::from(settings.distance);
				let lowered = (distance - deficit / JXL_POINTS_PER_DISTANCE - RAISE_DISTANCE_MARGIN).max(MIN_DISTANCE);
				(lowered < distance).then_some(Self::JxlLossy(JxlSettings {
					distance: lowered as f32,
					..settings
				}))
			}
			Self::JxlLossless(_) => None,
		}
	}

	pub fn needed_setting(self, score: f64, target: f64, speed: Speed) -> Option<f64> {
		let spare = score - target - LEARNED_HEADROOM;
		let needed = match self {
			Self::Ravif(settings) => f64::from(settings.quality) - spare / RAVIF_POINTS_PER_QUALITY,
			Self::JxlLossy(settings) => f64::from(settings.distance) + spare / JXL_POINTS_PER_DISTANCE,
			Self::JxlLossless(_) => return None,
		};

		self.with_setting(speed, needed).setting()
	}

	pub fn finer_than(self, other: Self) -> bool {
		match (self, other) {
			(Self::Ravif(this), Self::Ravif(other)) => this.quality > other.quality,
			(Self::JxlLossy(this), Self::JxlLossy(other)) => this.distance < other.distance,
			_ => false,
		}
	}

	pub fn encoder_id(self) -> &'static str {
		match self {
			Self::Ravif(_) => "ravif-0.13",
			Self::JxlLossy(_) => "libjxl",
			Self::JxlLossless(_) => "libjxl-lossless",
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

pub fn precheck(input: &PrecheckInput<'_>) -> Result<(), Gate> {
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

	if !metadata.aux.is_empty() && !input.drop_aux {
		let kinds: Vec<String> = metadata.aux.iter().map(ToString::to_string).collect();
		return Err(keep(format!(
			"it has auxiliary images or video ({}); --drop-aux converts it without them",
			kinds.join(", ")
		)));
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
	Ok(())
}

fn keep(reason: String) -> Gate {
	Gate::Keep(KeepReason::Unsupported(reason))
}

pub fn image_target(speed: Speed, overrides: &Overrides) -> f64 {
	overrides.normalized().image.unwrap_or(ssim_target(speed))
}

pub fn plan_still(
	facts: &StillFacts,
	speed: Speed,
	overrides: &Overrides,
	libjxl: bool,
) -> Result<StillPlan, KeepReason> {
	let pixels = u64::from(facts.width) * u64::from(facts.height);
	let fits_avif = facts.width <= MAX_AVIF_SIDE
		&& facts.height <= MAX_AVIF_SIDE
		&& pixels <= MAX_AVIF_PIXELS
		&& !facts.effective.high_depth
		&& !facts.grey_icc;
	let mut candidates = Vec::new();
	if fits_avif {
		candidates.push(Candidate::Ravif(ravif_settings(speed)));
	} else if libjxl {
		candidates.push(Candidate::JxlLossy(jxl_lossy_settings(speed)));
	} else {
		return Err(KeepReason::Unsupported(
			"this image needs JPEG XL, and ffmpeg has no libjxl".into(),
		));
	}
	if facts.graphic && libjxl {
		candidates.push(Candidate::JxlLossless(JXL_LOSSLESS));
	}

	Ok(StillPlan {
		memory_bytes: memory_estimate(facts.width, facts.height, &candidates),
		candidates,
		target: image_target(speed, overrides),
	})
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
			Candidate::Ravif(_) => pixels * RAVIF_BYTES_PER_PIXEL,
			Candidate::JxlLossy(_) | Candidate::JxlLossless(_) => LIBJXL_BASE_BYTES + pixels * LIBJXL_BYTES_PER_PIXEL,
		})
		.max()
		.unwrap_or(0);
	pixels * (DECODE_BYTES_PER_PIXEL + VERIFY_BYTES_PER_PIXEL) + encoder
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
			kinds(&plan_still(&facts(6000, 4000), Speed::Moderate, &Overrides::default(), true).unwrap()),
			["ravif"]
		);
		assert_eq!(
			kinds(&plan_still(&facts(1293, 48277), Speed::Moderate, &Overrides::default(), true).unwrap()),
			["jxl lossy"]
		);
		assert_eq!(
			kinds(&plan_still(&facts(20000, 800), Speed::Moderate, &Overrides::default(), true).unwrap()),
			["jxl lossy"]
		);
		assert_eq!(
			kinds(&plan_still(&deep, Speed::Moderate, &Overrides::default(), true).unwrap()),
			["jxl lossy"]
		);
		assert_eq!(
			kinds(&plan_still(&grey, Speed::Moderate, &Overrides::default(), true).unwrap()),
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
			kinds(&plan_still(&graphic, Speed::Fast, &Overrides::default(), true).unwrap()),
			["ravif", "jxl lossless"]
		);
		assert_eq!(
			kinds(&plan_still(&graphic, Speed::Fast, &Overrides::default(), false).unwrap()),
			["ravif"]
		);
		assert!(matches!(
			plan_still(&facts(1293, 48277), Speed::Fast, &Overrides::default(), false),
			Err(KeepReason::Unsupported(reason)) if reason.contains("libjxl")
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
			Ok(())
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
			let target = |overrides: &Overrides| plan_still(&facts(64, 48), speed, overrides, true).unwrap().target;

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
		assert_eq!(worst_case_memory(100, 100), 10_000 * (8 + 36 + 7) + (250 << 20));
	}

	#[test]
	fn learned_settings_stay_near_the_table() {
		let ravif = Candidate::Ravif(ravif_settings(Speed::Moderate));
		let jxl = Candidate::JxlLossy(jxl_lossy_settings(Speed::Moderate));
		let lossless = Candidate::JxlLossless(JXL_LOSSLESS);

		assert_eq!(ravif.with_setting(Speed::Moderate, 60.0).setting(), Some(84.0));
		assert_eq!(ravif.with_setting(Speed::Moderate, 120.0).setting(), Some(100.0));
		assert_eq!(ravif.with_setting(Speed::Moderate, 90.0).setting(), Some(90.0));
		assert_eq!(jxl.with_setting(Speed::Moderate, 5.0).setting(), Some(2.0));
		assert_eq!(
			jxl.with_setting(Speed::Moderate, 0.0)
				.setting()
				.map(|distance| (distance * 10.0).round()),
			Some(1.0)
		);
		assert_eq!(lossless.with_setting(Speed::Moderate, 3.0), lossless);
		assert_eq!(lossless.setting(), None);
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
	fn a_pass_teaches_the_setting_that_would_have_cleared_the_floor_by_two() {
		let ravif = Candidate::Ravif(ravif_settings(Speed::Moderate));

		let generous = ravif.needed_setting(90.0, 85.0, Speed::Moderate).unwrap();
		let narrow = ravif.needed_setting(85.5, 85.0, Speed::Moderate).unwrap();

		assert!((generous - 89.6).abs() < 1e-4, "{generous}");
		assert!((narrow - 93.2).abs() < 1e-4, "{narrow}");
		assert_eq!(
			Candidate::JxlLossless(JXL_LOSSLESS).needed_setting(99.0, 85.0, Speed::Moderate),
			None
		);
		assert_eq!((ravif.encoder_id(), ravif.preset()), ("ravif-0.13", 6));
	}

	#[test]
	fn only_a_finer_setting_of_the_same_encoder_is_finer() {
		let ravif = Candidate::Ravif(ravif_settings(Speed::Fast));
		let jxl = Candidate::JxlLossy(jxl_lossy_settings(Speed::Fast));

		assert!(ravif.with_setting(Speed::Fast, 96.0).finer_than(ravif));
		assert!(!ravif.with_setting(Speed::Fast, 84.0).finer_than(ravif));
		assert!(!ravif.finer_than(ravif));
		assert!(jxl.with_setting(Speed::Fast, 1.0).finer_than(jxl));
		assert!(!jxl.with_setting(Speed::Fast, 2.0).finer_than(jxl));
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
		let unsupported = |gate: Result<(), Gate>| match gate {
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
		assert_eq!(precheck(&input(&healthy, &plain)), Ok(()));
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
			matches!(precheck(&input(&healthy, &gain_map)), Err(Gate::Keep(KeepReason::Unsupported(reason))) if reason.contains("--drop-aux"))
		);
		assert_eq!(
			precheck(&PrecheckInput {
				drop_aux: true,
				..input(&healthy, &gain_map)
			}),
			Ok(())
		);
		assert!(
			matches!(precheck(&input(&healthy, &r03)), Err(Gate::Keep(KeepReason::Unsupported(reason))) if reason.contains("only in EXIF"))
		);
		assert_eq!(
			precheck(&PrecheckInput {
				mode: MetadataMode::Full,
				..input(&healthy, &r03)
			}),
			Ok(())
		);
	}
}
