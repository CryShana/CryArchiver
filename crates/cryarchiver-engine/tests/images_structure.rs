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
#[ignore = "needs the local sample corpus (set CRY_CORPUS, default C:\\media-corpus)"]
fn corpus_structures() {
	let corpus =
		std::env::var_os("CRY_CORPUS").map_or_else(|| Path::new(r"C:\media-corpus").to_path_buf(), Into::into);
	let read = |name: &str| std::fs::read(corpus.join(name)).unwrap();

	for healthy in [
		"DSC_0001.JPG",
		"large-map.jpg",
		"comic-veryTallImage.jpg",
		"company-logo.png",
		"animatedImage.gif",
		"screenshot_with_ICC.avif",
		"generated-valid/apng_alpha_anim.apng",
		"generated-valid/gif_loop3_variable-delays.gif",
		"generated-valid/avif_existing_aom.avif",
		"generated-corrupted/exif_broken_ifd0-offset.jpg",
		"generated-corrupted/bitflips_entropy-data.jpg",
		"generated-corrupted/icc_broken_profile-size.jpg",
		"generated-corrupted/png_bomb_header-100000x100000.png",
	] {
		assert_eq!(check(&read(healthy)).damage, None, "{healthy}");
	}
	for damaged in [
		"generated-corrupted/truncated_half.jpg",
		"generated-corrupted/truncated_70pct.png",
		"generated-corrupted/truncated_50pct_anim.gif",
		"generated-corrupted/truncated_60pct.avif",
	] {
		assert!(check(&read(damaged)).damage.is_some(), "{damaged}");
	}
	assert_eq!(
		check(&read("generated-corrupted/bad_crc_only_data-intact.png")).warnings,
		["CRC mismatch in the IDAT chunk"]
	);
}
