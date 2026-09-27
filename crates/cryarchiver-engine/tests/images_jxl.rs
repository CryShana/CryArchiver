mod support;

use std::path::Path;

use cryarchiver_engine::container::jxl::read_boxes;
use cryarchiver_engine::images::jxl::{JxlJob, JxlPixels, encode, reference};
use cryarchiver_engine::images::plan::JxlSettings;
use cryarchiver_engine::images::props::Effective;
use cryarchiver_engine::metadata::OutputMetadata;
use image::codecs::png::PngEncoder;
use image::{GenericImageView, ImageBuffer, ImageEncoder, Luma, Rgb};
use jxl_oxide::{JxlImage, PixelFormat};
use moxcms::ColorProfile;
use tokio_util::sync::CancellationToken;

const QUICK_LOSSY: JxlSettings = JxlSettings {
	distance: 1.0,
	effort: 3,
};
const LOSSLESS: JxlSettings = JxlSettings {
	distance: 0.0,
	effort: 3,
};

fn job(source: &Path, settings: JxlSettings, pixels: JxlPixels) -> JxlJob<'_> {
	JxlJob {
		source,
		source_is_heif: false,
		settings,
		pixels,
	}
}

async fn encoded(dir: &Path, job: &JxlJob<'_>, metadata: &OutputMetadata) -> Vec<u8> {
	let tools = support::toolchain().await;
	encode(
		&tools.ffmpeg,
		job,
		&dir.join("candidate.jxl.tmp"),
		metadata,
		&CancellationToken::new(),
	)
	.await
	.unwrap()
}

#[tokio::test]
async fn a_rotated_jpeg_becomes_an_upright_jxl_with_our_metadata_only() {
	let dir = tempfile::tempdir().unwrap();
	let jpeg = support::jpeg_with_exif(dir.path(), "o6.jpg", 64, 48, 6);
	let patched = support::exif_block(1, 48, 64);
	let metadata = OutputMetadata {
		exif: Some(patched.clone()),
		xmp: Some(b"<x:xmpmeta/>".to_vec()),
		warnings: Vec::new(),
	};

	let bytes = encoded(dir.path(), &job(&jpeg, QUICK_LOSSY, JxlPixels::Rgb8), &metadata).await;

	let boxes = read_boxes(&bytes).unwrap();
	assert_eq!(boxes.exif, Some(patched));
	assert_eq!(boxes.xmp.as_deref(), Some(b"<x:xmpmeta/>".as_slice()));
	let image = JxlImage::builder().read(bytes.as_slice()).unwrap();
	assert_eq!((image.width(), image.height()), (48, 64));
	assert_eq!(image.image_header().metadata.orientation, 1);
}

#[tokio::test]
async fn deep_images_are_encoded_losslessly_with_their_level() {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("deep.png");
	ImageBuffer::from_fn(64, 48, |x, y| Rgb([u16::try_from(x * 1000 + y).unwrap(), 1234, 40_000]))
		.save(&source)
		.unwrap();
	let pixels = JxlPixels::for_source(
		Effective {
			high_depth: true,
			alpha: false,
		},
		false,
	);

	let bytes = encoded(dir.path(), &job(&source, LOSSLESS, pixels), &OutputMetadata::default()).await;
	let tools = support::toolchain().await;
	let reference = reference(
		&tools.ffmpeg,
		&job(&source, LOSSLESS, pixels),
		&CancellationToken::new(),
	)
	.await
	.unwrap();

	assert_eq!(pixels, JxlPixels::Rgb16);
	assert_eq!(read_boxes(&bytes).unwrap().level, Some(10));
	assert_eq!(reference.dimensions(), (64, 48));
	assert_eq!(reference.to_rgb16().get_pixel(3, 2).0, [3002, 1234, 40_000]);
}

#[tokio::test]
async fn tiled_heic_goes_to_jxl_without_the_side_data_filter() {
	let dir = tempfile::tempdir().unwrap();
	let heic = dir.path().join("grid.heic");
	std::fs::write(&heic, support::heif::heic_grid(dir.path(), 64, 48)).unwrap();
	let heif_job = JxlJob {
		source_is_heif: true,
		..job(&heic, QUICK_LOSSY, JxlPixels::Rgb8)
	};

	let bytes = encoded(dir.path(), &heif_job, &OutputMetadata::default()).await;

	let image = JxlImage::builder().read(bytes.as_slice()).unwrap();
	assert_eq!((image.width(), image.height()), (128, 96));
}

#[tokio::test]
async fn grey_profiled_images_stay_grey_and_keep_their_profile() {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("scan.png");
	let icc = ColorProfile::new_gray_with_gamma(2.2).encode().unwrap();
	let mut encoder = PngEncoder::new(std::fs::File::create(&source).unwrap());
	encoder.set_icc_profile(icc.clone()).unwrap();
	ImageBuffer::from_fn(64, 48, |x, y| Luma([u8::try_from((x * 3 + y * 2) % 256).unwrap()]))
		.write_with_encoder(encoder)
		.unwrap();
	let pixels = JxlPixels::for_source(
		Effective {
			high_depth: false,
			alpha: false,
		},
		true,
	);

	let bytes = encoded(dir.path(), &job(&source, LOSSLESS, pixels), &OutputMetadata::default()).await;

	let image = JxlImage::builder().read(bytes.as_slice()).unwrap();
	assert_eq!(pixels, JxlPixels::Grey8);
	assert!(matches!(image.pixel_format(), PixelFormat::Gray));
	assert_eq!(image.original_icc(), Some(icc.as_slice()));
}
