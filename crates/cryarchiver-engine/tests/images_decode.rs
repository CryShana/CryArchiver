mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cryarchiver_engine::images::decode::{
	Decoded, IMAGE_DECODE_ERRORS, Undecodable, decode, deep_pix_fmt, header_dimensions,
};
use cryarchiver_engine::images::props::effective;
use cryarchiver_engine::metadata::extract;
use cryarchiver_engine::metadata::icc::Cicp;
use cryarchiver_engine::outcome::{ErrorKind, KeepReason};
use cryarchiver_engine::probe::probe;
use image::GenericImageView;
use support::heif::{HeifFile, HeifItem, aux_c, heic_grid, heic_still, irot, ispe};
use tokio_util::sync::CancellationToken;

async fn decoded(path: &Path) -> Result<Decoded, Undecodable> {
	let tools = support::toolchain().await;
	let cancel = CancellationToken::new();
	let bytes = Arc::new(std::fs::read(path).unwrap());
	let metadata = extract(&bytes);
	let info = probe(&tools, path, &cancel).await.ok();
	let pix_fmt = info
		.as_ref()
		.and_then(|info| info.streams.iter().find_map(|stream| stream.pix_fmt.as_deref()));

	decode(&tools, path, bytes, &metadata, pix_fmt, &cancel).await
}

fn written(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
	let path = dir.join(name);
	std::fs::write(&path, bytes).unwrap();
	path
}

fn png(dir: &Path, name: &str) -> Vec<u8> {
	let path = dir.join(name);
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=64x48",
		"-frames:v",
		"1",
		path.to_str().unwrap(),
	]);
	std::fs::read(path).unwrap()
}

fn chunk(kind: [u8; 4], body: &[u8]) -> Vec<u8> {
	let mut bytes = u32::try_from(body.len()).unwrap().to_be_bytes().to_vec();
	bytes.extend_from_slice(&kind);
	bytes.extend_from_slice(body);
	bytes.extend_from_slice(&crc32fast::hash(&[kind.as_slice(), body].concat()).to_be_bytes());
	bytes
}

fn with_chunk_before_idat(png: &[u8], extra: &[u8]) -> Vec<u8> {
	let idat = png.windows(4).position(|w| w == b"IDAT").unwrap() - 4;
	[&png[..idat], extra, &png[idat..]].concat()
}

#[tokio::test]
async fn exif_orientation_is_applied_to_the_pixels() {
	let dir = tempfile::tempdir().unwrap();
	let exif = chunk(*b"eXIf", &support::exif_block(6, 64, 48));
	let rotated = written(
		dir.path(),
		"o6.png",
		&with_chunk_before_idat(&png(dir.path(), "plain.png"), &exif),
	);

	let image = decoded(&rotated).await.unwrap().image;

	assert_eq!(image.dimensions(), (48, 64));
}

#[tokio::test]
async fn heic_is_rotated_by_irot_once_and_its_exif_orientation_is_ignored() {
	let dir = tempfile::tempdir().unwrap();
	let heic = heic_still(dir.path(), 64, 48, vec![irot(3)], Some(&support::exif_block(6, 64, 48)));

	let image = decoded(&written(dir.path(), "rotated.heic", &heic))
		.await
		.unwrap()
		.image;

	assert_eq!(image.dimensions(), (48, 64));
}

#[tokio::test]
async fn tiled_heic_is_decoded_whole() {
	let dir = tempfile::tempdir().unwrap();
	let grid = heic_grid(dir.path(), 64, 48);

	let image = decoded(&written(dir.path(), "grid.heic", &grid)).await.unwrap().image;

	assert_eq!(image.dimensions(), (128, 96));
}

#[tokio::test]
async fn heif_with_an_alpha_item_keeps_the_original() {
	let dir = tempfile::tempdir().unwrap();
	let heif = HeifFile {
		brand: *b"heic",
		primary: 1,
		items: vec![
			HeifItem::new(1, *b"hvc1", vec![0; 8]).with(false, ispe(8, 8)),
			HeifItem::new(2, *b"hvc1", vec![0; 8]).with(true, aux_c("urn:mpeg:hevc:2015:auxid:1")),
		],
		references: vec![(*b"auxl", 2, vec![1])],
	}
	.build();

	let outcome = decoded(&written(dir.path(), "alpha.heic", &heif)).await;

	assert!(
		matches!(outcome, Err(Undecodable::Keep(KeepReason::Unsupported(reason))) if reason.contains("transparency"))
	);
}

#[tokio::test]
async fn avif_alpha_survives_the_ffmpeg_decode() {
	let dir = tempfile::tempdir().unwrap();
	let avif = dir.path().join("alpha.avif");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=64x48,format=rgba,geq=r='r(X,Y)':g='g(X,Y)':b='b(X,Y)':a='if(lt(X,32),255,0)'",
		"-filter_complex",
		"[0:v]split[c][a];[c]format=yuv444p10le[co];[a]alphaextract,format=gray10le[ao]",
		"-map",
		"[co]",
		"-map",
		"[ao]",
		"-c:v",
		"libaom-av1",
		"-still-picture",
		"1",
		"-cpu-used",
		"8",
		"-frames:v",
		"1",
		avif.to_str().unwrap(),
	]);

	let image = decoded(&avif).await.unwrap().image;

	assert_eq!(image.dimensions(), (64, 48));
	assert!(effective(&image).alpha);
}

#[tokio::test]
async fn a_png_crc_mismatch_falls_back_to_ffmpeg_and_corrupt_data_is_damage() {
	let dir = tempfile::tempdir().unwrap();
	let original = png(dir.path(), "plain.png");
	let idat = original.windows(4).position(|w| w == b"IDAT").unwrap();
	let len = usize::try_from(u32::from_be_bytes(original[idat - 4..idat].try_into().unwrap())).unwrap();
	let mut bad_crc = original.clone();
	bad_crc[idat + 4 + len] ^= 0xFF;
	let mut scrambled = original.clone();
	scrambled[idat + 6..idat + 4 + len].fill(0x55);

	let recovered = decoded(&written(dir.path(), "crc.png", &bad_crc)).await.unwrap();
	let broken = decoded(&written(dir.path(), "broken.png", &scrambled)).await;

	assert_eq!(recovered.image.dimensions(), (64, 48));
	assert!(
		matches!(&broken, Err(Undecodable::Failed(err)) if matches!(err.kind, ErrorKind::SourceDamaged(_))),
		"{broken:?}"
	);
}

#[tokio::test]
async fn concealed_jpeg_errors_are_counted() {
	let dir = tempfile::tempdir().unwrap();
	let path = dir.path().join("a.jpg");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=256x192",
		"-frames:v",
		"1",
		path.to_str().unwrap(),
	]);
	let mut jpeg = std::fs::read(&path).unwrap();
	let middle = jpeg.len() / 2;
	jpeg[middle..middle + 200].fill(0x55);

	let decoded = decoded(&written(dir.path(), "damaged.jpg", &jpeg)).await.unwrap();

	assert!(decoded.decode_errors > 0);
}

#[tokio::test]
async fn jpeg_xl_sources_are_decoded_with_their_colour() {
	let dir = tempfile::tempdir().unwrap();
	let jxl = dir.path().join("lossless.jxl");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=64x48",
		"-frames:v",
		"1",
		"-c:v",
		"libjxl",
		"-distance",
		"0",
		"-update",
		"1",
		jxl.to_str().unwrap(),
	]);

	let decoded = decoded(&jxl).await.unwrap();

	assert_eq!(decoded.image.dimensions(), (64, 48));
	assert_eq!(decoded.cicp, Some(Cicp::SRGB));
	assert!(!effective(&decoded.image).high_depth);
}

#[test]
fn exif_parser_messages_are_not_counted_as_decode_errors() {
	let exif_lines = [
		"[mjpeg @ 0000] not enough bytes remaining in EXIF buffer. entries: 18761",
		"[mjpeg @ 0000] error decoding EXIF data: Invalid data found when processing input",
	];
	let pixel_lines = ["[mjpeg @ 0000] error count: 64", "[mjpeg @ 0000] error y=96 x=207"];
	let counted = |line: &str| IMAGE_DECODE_ERRORS.iter().any(|pattern| line.contains(pattern));

	assert!(!exif_lines.iter().any(|line| counted(line)));
	assert!(pixel_lines.iter().all(|line| counted(line)));
}

#[test]
fn deep_pixel_formats_are_those_with_an_endianness() {
	for deep in [
		"yuv420p10le",
		"gray10le",
		"rgb48be",
		"rgba64le",
		"gbrpf32le",
		"yuv444p12le",
	] {
		assert!(deep_pix_fmt(deep), "{deep}");
	}
	for shallow in [
		"yuv420p", "yuvj444p", "rgb24", "rgba", "pal8", "gray", "yuv410p", "rgb565le",
	] {
		assert!(!deep_pix_fmt(shallow), "{shallow}");
	}
}

#[test]
fn header_dimensions_come_without_decoding() {
	let mut bomb = b"\x89PNG\r\n\x1a\n".to_vec();
	let mut header = 100_000u32.to_be_bytes().to_vec();
	header.extend_from_slice(&100_000u32.to_be_bytes());
	header.extend_from_slice(&[8, 2, 0, 0, 0]);
	bomb.extend(chunk(*b"IHDR", &header));
	bomb.extend(chunk(*b"IDAT", &[0; 8]));
	bomb.extend(chunk(*b"IEND", &[]));
	let heif = HeifFile {
		brand: *b"heic",
		primary: 1,
		items: vec![HeifItem::new(1, *b"hvc1", vec![0; 4]).with(false, ispe(4032, 3024))],
		references: Vec::new(),
	}
	.build();

	assert_eq!(header_dimensions(&bomb, None), Some((100_000, 100_000)));
	assert_eq!(header_dimensions(&heif, None), Some((4032, 3024)));
	assert_eq!(header_dimensions(b"unknown", Some((800, 600))), Some((800, 600)));
	assert_eq!(header_dimensions(b"unknown", Some((0, 0))), None);
}

#[tokio::test]
#[ignore = "needs the local sample corpus (set CRY_CORPUS, default C:\\media-corpus)"]
async fn corpus_images_decode_with_the_expected_properties() {
	let corpus = std::env::var_os("CRY_CORPUS").map_or_else(|| PathBuf::from(r"C:\media-corpus"), PathBuf::from);
	let cases = [
		(
			"generated-valid/jpeg_exif_orientation6_gps.jpg",
			(800, 1200),
			false,
			false,
		),
		("generated-valid/tiff_16bit_rgb.tif", (800, 600), false, false),
		("generated-valid/png16_real_gradient.png", (512, 256), true, false),
		("generated-valid/png_alpha_all-opaque.png", (800, 600), false, false),
		("generated-valid/png_alpha_real.png", (800, 600), false, true),
		("generated-valid/jpeg2000.jp2", (800, 600), false, false),
		("generated-valid/avif_existing_aom.avif", (800, 600), false, false),
		("generated-valid/jxl_lossless_existing.jxl", (800, 600), false, false),
		("screenshot_with_ICC.avif", (1637, 954), false, false),
	];

	for (name, dimensions, high_depth, alpha) in cases {
		let decoded = decoded(&corpus.join(name)).await.unwrap();
		let properties = effective(&decoded.image);
		assert_eq!(decoded.image.dimensions(), dimensions, "{name}");
		assert_eq!((properties.high_depth, properties.alpha), (high_depth, alpha), "{name}");
	}
}
