mod support;

use std::sync::Arc;

use cryarchiver_engine::images::avif::{AvifColour, encode as encode_avif, finish};
use cryarchiver_engine::images::decode::SourceLayout;
use cryarchiver_engine::images::jxl::{JxlJob, JxlPixels, encode as encode_jxl};
use cryarchiver_engine::images::plan::{JxlSettings, RavifSettings};
use cryarchiver_engine::images::ssim::{render_jxl_srgb, score, to_srgb};
use cryarchiver_engine::images::verify::{Expected, check_jxl, verify_avif, verify_jxl};
use cryarchiver_engine::metadata::OutputMetadata;
use cryarchiver_engine::metadata::icc::Cicp;
use cryarchiver_engine::outcome::ErrorKind;
use image::{DynamicImage, GenericImageView, ImageBuffer, Rgb, Rgba};
use moxcms::ColorProfile;
use tokio_util::sync::CancellationToken;

const UPRIGHT: SourceLayout = SourceLayout {
	grid: false,
	alpha_item: false,
	orientation: 1,
	fancy_upsampling: false,
};

fn photo_like(width: u32, height: u32) -> DynamicImage {
	DynamicImage::ImageRgb8(ImageBuffer::from_fn(width, height, |x, y| {
		Rgb([
			u8::try_from((x * 2) % 256).unwrap(),
			u8::try_from((y * 3) % 256).unwrap(),
			u8::try_from((x / 8 + y / 8) % 2 * 100 + 50).unwrap(),
		])
	}))
}

fn expected(metadata: &OutputMetadata) -> Expected<'_> {
	Expected {
		width: 96,
		height: 64,
		alpha: false,
		icc: None,
		cicp: Cicp::SRGB,
		metadata,
	}
}

#[test]
fn identical_images_score_100_and_damage_lowers_the_score() {
	let original = photo_like(96, 64);
	let mut damaged = original.to_rgb8();
	for pixel in damaged.pixels_mut().step_by(3) {
		pixel.0 = [255 - pixel.0[0], pixel.0[1], 0];
	}

	assert!(score(&original, &original).unwrap() > 99.9);
	assert!(score(&original, &DynamicImage::ImageRgb8(damaged)).unwrap() < 50.0);
	assert!(score(&original, &photo_like(64, 96)).unwrap_err().contains("output is"));
}

fn noisy(width: u32, height: u32, seed: u32) -> image::RgbImage {
	let mut state = seed;
	ImageBuffer::from_fn(width, height, |x, y| {
		state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
		let noise = i32::try_from((state >> 16) % 13).unwrap() - 6;
		let level = |base: u32| u8::try_from((i32::try_from(base % 200 + 20).unwrap() + noise).clamp(0, 255)).unwrap();
		Rgb([level(x + y / 4), level(y / 3), level(x * 2)])
	})
}

/// Copies side by side, every other one mirrored so that no seam adds an edge.
fn side_by_side(image: &image::RgbImage, copies: u32) -> DynamicImage {
	let (width, height) = image.dimensions();
	DynamicImage::ImageRgb8(ImageBuffer::from_fn(width * copies, height, |x, y| {
		let within = x % width;
		let column = if (x / width) % 2 == 0 {
			within
		} else {
			width - 1 - within
		};
		*image.get_pixel(column, y)
	}))
}

#[test]
fn a_narrow_tall_noisy_image_scores_like_the_same_content_at_full_width() {
	let reference = noisy(48, 16_500, 7);
	let mut distorted = reference.clone();
	for pixel in distorted.pixels_mut() {
		pixel.0 = pixel.0.map(|value| value & 0xF0 | 0x08);
	}

	let narrow = score(
		&DynamicImage::ImageRgb8(reference.clone()),
		&DynamicImage::ImageRgb8(distorted.clone()),
	)
	.unwrap();
	let wide = score(&side_by_side(&reference, 6), &side_by_side(&distorted, 6)).unwrap();

	assert!(
		(narrow - wide).abs() < 2.0,
		"48 px wide: {narrow:.2}, 288 px wide: {wide:.2}"
	);
}

#[test]
fn tiny_images_are_scored_too() {
	let tiny = photo_like(3, 3);

	assert!(score(&tiny, &tiny).unwrap() > 99.0);
}

#[test]
fn transparency_is_scored_over_dark_and_light_backgrounds() {
	let base = |alpha_right: u8, hidden: u8| {
		DynamicImage::ImageRgba8(ImageBuffer::from_fn(64, 64, |x, y| {
			if x < 32 {
				Rgba([u8::try_from(x * 4).unwrap(), u8::try_from(y * 4).unwrap(), 90, 255])
			} else {
				Rgba([hidden, hidden, hidden, alpha_right])
			}
		}))
	};

	let hidden_change = score(&base(0, 10), &base(0, 250)).unwrap();
	let lost_transparency = score(&base(0, 10), &base(255, 10)).unwrap();

	assert!(hidden_change > 99.0, "{hidden_change}");
	assert!(lost_transparency < 60.0, "{lost_transparency}");
}

#[test]
fn references_are_converted_to_srgb_from_their_profile() {
	let image = photo_like(16, 16);
	let srgb = ColorProfile::new_srgb().encode().unwrap();
	let adobe = ColorProfile::new_adobe_rgb().encode().unwrap();
	let grey = ColorProfile::new_gray_with_gamma(2.2).encode().unwrap();

	let grey_image = DynamicImage::ImageLuma8(image.to_luma8());
	let unchanged = to_srgb(&image, Some(&srgb)).unwrap();
	let converted = to_srgb(&image, Some(&adobe)).unwrap();
	let from_grey = to_srgb(&grey_image, Some(&grey)).unwrap();

	assert_eq!(*unchanged, image);
	assert_ne!(converted.to_rgb16(), image.to_rgb16());
	assert_eq!(from_grey.dimensions(), (16, 16));
}

#[tokio::test]
async fn a_good_avif_passes_its_checks_and_scores_well() {
	let dir = tempfile::tempdir().unwrap();
	let tools = support::toolchain().await;
	let source = photo_like(96, 64);
	let metadata = OutputMetadata::default();
	let settings = RavifSettings {
		quality: 92.0,
		speed: 10,
	};
	let bytes = finish(
		&encode_avif(&source, false, settings, 2).unwrap(),
		AvifColour { icc: None, cicp: None },
		&metadata,
	)
	.unwrap();
	let output = dir.path().join("out.avif");
	std::fs::write(&output, &bytes).unwrap();
	let cancel = CancellationToken::new();

	let passed = verify_avif(
		&tools,
		&output,
		&bytes,
		Arc::new(source.clone()),
		&expected(&metadata),
		&cancel,
	)
	.await
	.unwrap();
	let wrong = Expected {
		alpha: true,
		..expected(&metadata)
	};
	let failed = verify_avif(&tools, &output, &bytes, Arc::new(source), &wrong, &cancel)
		.await
		.unwrap_err();

	assert!(passed > 80.0, "{passed}");
	assert!(
		matches!(&failed.kind, ErrorKind::EncodeFailed(reason) if reason.contains("alpha")),
		"{failed:?}"
	);
}

#[tokio::test]
async fn a_lossless_jxl_is_rendered_to_srgb_and_scores_as_identical() {
	let dir = tempfile::tempdir().unwrap();
	let tools = support::toolchain().await;
	let source = dir.path().join("source.png");
	photo_like(96, 64).save(&source).unwrap();
	let metadata = OutputMetadata {
		exif: Some(support::exif_block(1, 96, 64)),
		xmp: None,
		warnings: Vec::new(),
	};
	let job = JxlJob {
		source: &source,
		layout: UPRIGHT,
		settings: JxlSettings {
			distance: 0.0,
			effort: 3,
		},
		pixels: JxlPixels::Rgb8,
	};
	let cancel = CancellationToken::new();
	let bytes = encode_jxl(&tools.ffmpeg, &job, &dir.path().join("c.tmp"), &metadata, &cancel)
		.await
		.unwrap();
	let reference = Arc::new(photo_like(96, 64));

	let rendered = render_jxl_srgb(&bytes).unwrap();
	let passed = verify_jxl(Arc::new(bytes.clone()), reference, &expected(&metadata), true, &cancel)
		.await
		.unwrap();

	assert_eq!(rendered.to_rgb8(), photo_like(96, 64).to_rgb8());
	assert!(passed > 99.0, "{passed}");
	assert!(
		check_jxl(&bytes, &expected(&OutputMetadata::default()), true)
			.unwrap_err()
			.contains("Exif")
	);
}

async fn lossless_jxl(dir: &std::path::Path, name: &str, image: &DynamicImage) -> Vec<u8> {
	let tools = support::toolchain().await;
	let source = dir.join(format!("{name}.png"));
	image.save(&source).unwrap();
	let job = JxlJob {
		source: &source,
		layout: UPRIGHT,
		settings: JxlSettings {
			distance: 0.0,
			effort: 3,
		},
		pixels: JxlPixels::Rgb8,
	};
	encode_jxl(
		&tools.ffmpeg,
		&job,
		&dir.join(format!("{name}.tmp")),
		&OutputMetadata::default(),
		&CancellationToken::new(),
	)
	.await
	.unwrap()
}

#[tokio::test]
async fn jxl_outputs_are_verified_against_our_own_decoded_pixels() {
	let dir = tempfile::tempdir().unwrap();
	let cancel = CancellationToken::new();
	let metadata = OutputMetadata::default();
	let upright = photo_like(96, 64);
	let transparent = DynamicImage::ImageRgba8(ImageBuffer::from_fn(96, 64, |x, y| {
		let [red, green, blue] = upright.to_rgb8().get_pixel(x, y).0;
		Rgba([red, green, blue, if x < 48 { 255 } else { 0 }])
	}));
	let opaque = lossless_jxl(dir.path(), "opaque", &upright).await;
	let turned = lossless_jxl(dir.path(), "turned", &upright.rotate180()).await;
	let sideways = lossless_jxl(dir.path(), "sideways", &upright.rotate90()).await;
	let with_alpha = Expected {
		alpha: true,
		..expected(&metadata)
	};

	let dropped_alpha = verify_jxl(Arc::new(opaque), Arc::new(transparent), &with_alpha, true, &cancel)
		.await
		.unwrap_err();
	let turned_score = verify_jxl(
		Arc::new(turned),
		Arc::new(upright.clone()),
		&expected(&metadata),
		true,
		&cancel,
	)
	.await
	.unwrap();
	let sideways_err = verify_jxl(
		Arc::new(sideways),
		Arc::new(upright),
		&expected(&metadata),
		true,
		&cancel,
	)
	.await
	.unwrap_err();

	assert!(
		matches!(&dropped_alpha.kind, ErrorKind::EncodeFailed(reason) if reason.contains("alpha")),
		"{dropped_alpha:?}"
	);
	assert!(turned_score < 50.0, "{turned_score}");
	assert!(
		matches!(&sideways_err.kind, ErrorKind::EncodeFailed(reason) if reason.contains("64×96")),
		"{sideways_err:?}"
	);
}
