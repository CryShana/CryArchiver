use crate::images::props::Effective;
use crate::images::structure::Structure;
use crate::metadata::{MetadataMode, SourceMetadata, exif, icc};
use crate::outcome::KeepReason;
use crate::preset::Speed;

pub const MAX_AVIF_SIDE: u32 = 16_384;
pub const MAX_AVIF_PIXELS: u64 = 64_000_000;
pub const BOMB_PIXELS: u64 = 500_000_000;
pub const MAX_VERIFY_PIXELS: u64 = fast_ssim2::MAX_IMAGE_PIXELS as u64;

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

pub fn plan_still(facts: &StillFacts, speed: Speed, libjxl: bool) -> Result<StillPlan, KeepReason> {
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
		target: ssim_target(speed),
	})
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
			kinds(&plan_still(&facts(6000, 4000), Speed::Moderate, true).unwrap()),
			["ravif"]
		);
		assert_eq!(
			kinds(&plan_still(&facts(1293, 48277), Speed::Moderate, true).unwrap()),
			["jxl lossy"]
		);
		assert_eq!(
			kinds(&plan_still(&facts(20000, 800), Speed::Moderate, true).unwrap()),
			["jxl lossy"]
		);
		assert_eq!(kinds(&plan_still(&deep, Speed::Moderate, true).unwrap()), ["jxl lossy"]);
		assert_eq!(kinds(&plan_still(&grey, Speed::Moderate, true).unwrap()), ["jxl lossy"]);
	}

	#[test]
	fn graphics_also_try_lossless_jxl_when_libjxl_exists() {
		let graphic = StillFacts {
			graphic: true,
			..facts(800, 600)
		};

		assert_eq!(
			kinds(&plan_still(&graphic, Speed::Fast, true).unwrap()),
			["ravif", "jxl lossless"]
		);
		assert_eq!(kinds(&plan_still(&graphic, Speed::Fast, false).unwrap()), ["ravif"]);
		assert!(matches!(
			plan_still(&facts(1293, 48277), Speed::Fast, false),
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
