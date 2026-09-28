mod support;

use std::path::Path;

use cryarchiver_engine::images::structure::check;
use support::heif::{HeifFile, HeifItem, ispe};

fn generated(dir: &Path, name: &str, args: &[&str]) -> Vec<u8> {
	let path = dir.join(name);
	let mut all = vec!["-f", "lavfi", "-i", "testsrc2=s=64x48:d=0.3:r=10"];
	all.extend_from_slice(args);
	all.push(path.to_str().unwrap());
	support::ffmpeg(&all);
	std::fs::read(path).unwrap()
}

#[test]
fn a_heif_still_without_meta_is_damaged() {
	let mut bytes = Vec::new();
	bytes.extend_from_slice(&20u32.to_be_bytes());
	bytes.extend_from_slice(b"ftypheic");
	bytes.extend_from_slice(&0u32.to_be_bytes());
	bytes.extend_from_slice(b"mif1");
	bytes.extend_from_slice(&16u32.to_be_bytes());
	bytes.extend_from_slice(b"mdat\0\0\0\0\0\0\0\0");

	assert!(check(&bytes).damage.is_some());
}

#[test]
fn complete_images_pass_and_trailing_data_after_the_jpeg_is_allowed() {
	let dir = tempfile::tempdir().unwrap();
	let jpeg = generated(dir.path(), "a.jpg", &["-frames:v", "1"]);
	let png = generated(dir.path(), "a.png", &["-frames:v", "1"]);
	let gif = generated(dir.path(), "a.gif", &[]);
	let avif = generated(
		dir.path(),
		"a.avif",
		&[
			"-frames:v",
			"1",
			"-c:v",
			"libaom-av1",
			"-still-picture",
			"1",
			"-cpu-used",
			"8",
		],
	);
	let mut with_trailer = jpeg.clone();
	with_trailer.extend_from_slice(b"MPF secondary image bytes");

	for bytes in [&jpeg, &png, &gif, &avif, &with_trailer] {
		assert_eq!(check(bytes).damage, None);
	}
	assert_eq!(check(b"ID3\x03\0\0\0\0\0\x21").damage, None);
}

#[test]
fn truncated_images_are_damaged() {
	let dir = tempfile::tempdir().unwrap();
	let jpeg = generated(dir.path(), "a.jpg", &["-frames:v", "1"]);
	let png = generated(dir.path(), "a.png", &["-frames:v", "1"]);
	let gif = generated(dir.path(), "a.gif", &[]);
	let avif = generated(
		dir.path(),
		"a.avif",
		&[
			"-frames:v",
			"1",
			"-c:v",
			"libaom-av1",
			"-still-picture",
			"1",
			"-cpu-used",
			"8",
		],
	);
	let half = |bytes: &[u8]| bytes[..bytes.len() / 2].to_vec();

	assert!(check(&half(&jpeg)).damage.unwrap().contains("no end-of-image marker"));
	assert!(check(&half(&png)).damage.unwrap().contains("runs past the end"));
	assert!(check(&half(&gif)).damage.unwrap().contains("truncated"));
	assert!(check(&avif[..avif.len() - 10]).damage.unwrap().contains("past the end"));
}

#[test]
fn a_png_crc_mismatch_is_only_a_warning() {
	let dir = tempfile::tempdir().unwrap();
	let mut png = generated(dir.path(), "a.png", &["-frames:v", "1"]);
	let idat = png.windows(4).position(|w| w == b"IDAT").unwrap();
	png[idat + 6] ^= 0xFF;

	let structure = check(&png);

	assert_eq!(structure.damage, None);
	assert_eq!(structure.warnings, ["CRC mismatch in the IDAT chunk"]);
}

#[test]
fn an_image_sequence_without_a_movie_box_is_damaged() {
	let sequence = HeifFile {
		brand: *b"avis",
		primary: 1,
		items: vec![HeifItem::new(1, *b"av01", vec![0; 8]).with(false, ispe(8, 8))],
		references: Vec::new(),
	}
	.build();

	assert!(check(&sequence).damage.unwrap().contains("'moov'"));
}

#[test]
fn damaged_metadata_scrambled_pixels_and_odd_shapes_are_not_structural_damage() {
	let dir = tempfile::tempdir().unwrap();
	let jpeg = generated(dir.path(), "a.jpg", &["-frames:v", "1"]);
	let apng = generated(dir.path(), "a.apng", &["-plays", "3", "-f", "apng"]);
	let tall = dir.path().join("tall.jpg");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=64x17000",
		"-frames:v",
		"1",
		tall.to_str().unwrap(),
	]);
	let tall = std::fs::read(tall).unwrap();

	let mut scrambled = jpeg.clone();
	let middle = scrambled.len() / 2;
	scrambled[middle..middle + 64].fill(0x55);
	let mut broken_ifd = support::exif_block(1, 64, 48);
	broken_ifd[4..8].copy_from_slice(&0xFFFF_FF00u32.to_le_bytes());
	let broken_exif = support::with_jpeg_segment(&jpeg, 0xE1, &[b"Exif\0\0".as_slice(), &broken_ifd].concat());
	let mut icc = b"ICC_PROFILE\0\x01\x01".to_vec();
	icc.extend_from_slice(&u32::MAX.to_be_bytes());
	icc.extend_from_slice(&[0; 124]);
	let broken_icc = support::with_jpeg_segment(&jpeg, 0xE2, &icc);
	let header = [
		100_000u32.to_be_bytes().as_slice(),
		&100_000u32.to_be_bytes(),
		&[8, 2, 0, 0, 0],
	]
	.concat();
	let bomb = [
		b"\x89PNG\r\n\x1a\n".as_slice(),
		&support::png_chunk(*b"IHDR", &header),
		&support::png_chunk(*b"IDAT", &[0; 8]),
		&support::png_chunk(*b"IEND", &[]),
	]
	.concat();

	for (name, bytes) in [
		("apng", &apng),
		("tall", &tall),
		("scrambled", &scrambled),
		("broken exif", &broken_exif),
		("broken icc", &broken_icc),
		("bomb", &bomb),
	] {
		assert_eq!(check(bytes).damage, None, "{name}");
	}
	assert!(check(&apng[..apng.len() * 7 / 10]).damage.is_some());
}
