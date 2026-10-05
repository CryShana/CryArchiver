mod support;

use cryarchiver_engine::container::av1::read_sequence_header;
use cryarchiver_engine::container::avif::{AvifExpectations, AvifMetadata, GainMapInput, check, rewrite};
use cryarchiver_engine::container::isobmff::{Meta, Nclx, Property, parse_meta};
use cryarchiver_engine::images::avif::encode as encode_base;
use cryarchiver_engine::images::gain_map::{TARGET, decode, encode, fit, score};
use cryarchiver_engine::images::plan::RavifSettings;
use cryarchiver_engine::metadata::extract;
use cryarchiver_engine::metadata::icc::Cicp;
use cryarchiver_engine::workspace::Workspace;
use image::{DynamicImage, GrayImage, ImageBuffer, Luma, Rgb, RgbImage};
use support::ultra_hdr::{UltraHdrOptions, gain_map_av1, ultra_hdr_jpeg};
use tokio_util::sync::CancellationToken;

const PHOTO_PIXELS: u64 = 64 * 48;

fn gradient(width: u32, height: u32) -> GrayImage {
	GrayImage::from_fn(width, height, |x, y| {
		Luma([u8::try_from((x * 255 / width.max(2)) / 2 + (y * 255 / height.max(2)) / 2).unwrap()])
	})
}

fn primary_av1(avif: &[u8]) -> (Meta, Vec<u8>) {
	let meta = parse_meta(avif).unwrap();
	let data = meta.item_data(avif, meta.primary.unwrap()).unwrap();
	(meta, data)
}

#[test]
fn a_gain_map_is_encoded_as_monochrome_8_bit_full_range_av1() {
	let encoded = encode(&gradient(64, 48), 60.0, 10, 1).unwrap();

	let (meta, av1) = primary_av1(&encoded);
	let header = read_sequence_header(&av1).unwrap();
	let properties: Vec<&Property> = meta.properties_of(meta.primary.unwrap()).collect();

	assert_eq!(meta.primary_size(), Some((64, 48)));
	assert!(header.monochrome);
	assert_eq!(header.bit_depth, 8);
	assert!(header.full_range);
	assert_eq!(header.profile, 0);
	assert_eq!(
		(header.primaries, header.transfer, header.matrix),
		(Some(2), Some(2), Some(6))
	);
	assert!(properties.contains(&&Property::PixelDepths(vec![8])));
	assert!(properties.contains(&&Property::Nclx(Nclx {
		primaries: 2,
		transfer: 2,
		matrix: 6,
		full_range: true,
	})));
}

#[tokio::test]
async fn an_encoded_gain_map_scores_above_the_target_against_its_source() {
	let dir = tempfile::tempdir().unwrap();
	let tools = support::toolchain().await;
	let workspace = Workspace::create(&dir.path().join(".cry-work"), &dir.path().join("photo.jpg")).unwrap();
	let cancel = CancellationToken::new();
	let source = gradient(64, 48);
	let encoded = encode(&source, 60.0, 10, 1).unwrap();

	let passed = score(&tools, &workspace, &encoded, &source, &cancel).await.unwrap();
	let resized = score(&tools, &workspace, &encoded, &gradient(48, 64), &cancel).await;

	assert!(passed >= TARGET, "{passed}");
	assert!(resized.unwrap_err().kind.to_string().contains("verification failed"));
}

#[tokio::test]
async fn single_pixel_and_odd_sized_gain_maps_encode_and_decode_at_their_size() {
	let dir = tempfile::tempdir().unwrap();
	let tools = support::toolchain().await;
	let workspace = Workspace::create(&dir.path().join(".cry-work"), &dir.path().join("photo.jpg")).unwrap();
	let cancel = CancellationToken::new();

	for (width, height) in [(1, 1), (7, 5), (1, 9)] {
		let source = gradient(width, height);
		let encoded = encode(&source, 60.0, 10, 1).unwrap();

		let (meta, av1) = primary_av1(&encoded);
		let header = read_sequence_header(&av1).unwrap();
		let scored = score(&tools, &workspace, &encoded, &source, &cancel).await.unwrap();

		assert_eq!(meta.primary_size(), Some((width, height)));
		assert_eq!((header.width, header.height), (width, height));
		assert!(header.monochrome);
		assert!(scored >= TARGET, "{width}×{height}: {scored}");
	}
}

#[test]
fn a_gain_map_turns_with_the_base_orientation() {
	let gain = GrayImage::from_fn(4, 2, |x, y| Luma([u8::try_from(x + 10 * y).unwrap()]));

	let turned = fit(gain, 6, (2, 4), (2, 4));

	assert_eq!(turned.dimensions(), (2, 4));
	assert_eq!(turned.get_pixel(1, 0).0, [0]);
	assert_eq!(turned.get_pixel(0, 0).0, [10]);
	assert_eq!(turned.get_pixel(1, 3).0, [3]);
	assert_eq!(turned.get_pixel(0, 3).0, [13]);
}

#[test]
fn a_gain_map_is_resized_by_the_base_resize_factors() {
	let same = gradient(16, 12);

	assert_eq!(
		fit(gradient(1000, 750), 1, (4000, 3000), (2000, 1500)).dimensions(),
		(500, 375)
	);
	assert_eq!(
		fit(gradient(1, 100), 1, (4000, 3000), (1000, 750)).dimensions(),
		(1, 25)
	);
	assert_eq!(fit(same.clone(), 1, (64, 48), (64, 48)), same);
}

fn ultra_hdr_gain_map<G: image::GenericImageView>(
	gain: &G,
) -> (Vec<u8>, cryarchiver_engine::metadata::gain_map::GainMapSource)
where
	G::Pixel: image::PixelWithColorType,
{
	let base = RgbImage::from_fn(64, 48, |x, y| {
		Rgb([u8::try_from(x * 4).unwrap(), u8::try_from(y * 5).unwrap(), 90])
	});
	let bytes = ultra_hdr_jpeg(&base, gain, &UltraHdrOptions::default());
	let source = extract(&bytes).gain_map.unwrap();
	(bytes, source)
}

#[test]
fn a_grey_gain_map_decodes_and_a_colour_one_is_refused() {
	let (grey_file, grey) = ultra_hdr_gain_map(&gradient(16, 12));
	let colour_map = RgbImage::from_fn(16, 12, |x, y| {
		Rgb([u8::try_from(x * 15).unwrap(), u8::try_from(y * 20).unwrap(), 40])
	});
	let (colour_file, colour) = ultra_hdr_gain_map(&colour_map);

	assert_eq!(decode(&grey_file, &grey, PHOTO_PIXELS).unwrap().dimensions(), (16, 12));
	assert_eq!(
		decode(&colour_file, &colour, PHOTO_PIXELS).unwrap_err(),
		"it is in colour, which is not supported"
	);
}

#[test]
fn a_gain_map_with_more_pixels_than_the_photo_is_refused_before_it_is_decoded() {
	let (full_file, full) = ultra_hdr_gain_map(&gradient(64, 48));
	let (wider_file, wider) = ultra_hdr_gain_map(&gradient(65, 48));
	let sof = full.jpeg.start
		+ full_file[full.jpeg.clone()]
			.windows(2)
			.position(|marker| marker == [0xFF, 0xC0])
			.unwrap();
	let mut declared_huge = full_file.clone();
	declared_huge[sof + 5..sof + 9].copy_from_slice(&[0x4E, 0x20, 0x4E, 0x20]);

	assert_eq!(decode(&full_file, &full, PHOTO_PIXELS).unwrap().dimensions(), (64, 48));
	assert_eq!(
		decode(&wider_file, &wider, PHOTO_PIXELS).unwrap_err(),
		"it is larger than the photo (65×48)"
	);
	assert_eq!(
		decode(&declared_huge, &full, PHOTO_PIXELS).unwrap_err(),
		"it is larger than the photo (20000×20000)"
	);
}

#[test]
fn a_ravif_base_with_this_gain_map_passes_the_avif_check() {
	let base = DynamicImage::ImageRgb8(ImageBuffer::from_fn(24, 16, |x, y| {
		Rgb([u8::try_from(x * 10).unwrap(), u8::try_from(y * 15).unwrap(), 90])
	}));
	let settings = RavifSettings {
		quality: 80.0,
		speed: 10,
	};
	let metadata = UltraHdrOptions::default().metadata;
	let gain_map = encode(&gradient(6, 4), 60.0, 10, 1).unwrap();
	let input = GainMapInput {
		encoded: &gain_map,
		metadata: &metadata,
	};

	let avif = rewrite(
		&encode_base(&base, false, settings, 1).unwrap(),
		Cicp::SRGB,
		&AvifMetadata::default(),
		Some(&input),
	)
	.unwrap();

	let expected = AvifExpectations {
		width: 24,
		height: 16,
		alpha: false,
		cicp: Cicp::SRGB,
		metadata: AvifMetadata::default(),
		gain_map: Some(&metadata),
		gain_map_coded_cicp: true,
	};
	check(&avif, &expected).unwrap();
	assert_eq!(gain_map_av1(&avif).unwrap(), primary_av1(&gain_map).1);
	let meta = parse_meta(&avif).unwrap();
	let gain_map_item = meta.gain_map().unwrap().gain_map;
	assert!(
		meta.properties_of(gain_map_item)
			.any(|property| *property == Property::PixelDepths(vec![8]))
	);
}
