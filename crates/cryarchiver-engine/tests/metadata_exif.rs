mod support;

use std::path::{Path, PathBuf};
use std::process::Command;

use cryarchiver_engine::metadata::exif::{
	ExifError, adobe_rgb_only, from_tiff_file, ifd0_value, orientation, patch, problem, tiff_pages,
};
use support::tiff::{Entry, ascii, bytes16, bytes32, ifd, long, short, tiff, undefined};

fn camera_block(big: bool) -> Vec<u8> {
	tiff(
		big,
		&[
			ascii(0x010F, "TestCam"),
			short(big, 0x0112, 6),
			long(big, 0x0100, 1200),
			long(big, 0x0101, 800),
		],
		&[
			short(big, 0xA002, 1200),
			short(big, 0xA003, 800),
			undefined(0x927C, b"maker note bytes"),
		],
		&[],
	)
}

#[test]
fn patch_resets_orientation_and_rewrites_dimensions_in_place() {
	for big in [true, false] {
		let original = camera_block(big);
		let mut block = original.clone();

		let result = patch(&mut block, 800, 1200).unwrap();

		assert_eq!(result.orientation, Some(6));
		assert_eq!(
			result.patched,
			[
				(0x0100, 1200, 800),
				(0x0101, 800, 1200),
				(0xA002, 1200, 800),
				(0xA003, 800, 1200)
			]
		);
		assert!(result.skipped.is_empty(), "{:?}", result.skipped);
		assert_eq!(block.len(), original.len());
		assert_eq!(orientation(&block), Some(1));
		let changed = block.iter().zip(&original).filter(|(a, b)| a != b).count();
		assert!(changed <= 10, "{changed} bytes changed");
		let maker_note = original.windows(16).position(|w| w == b"maker note bytes").unwrap();
		assert_eq!(&block[maker_note..maker_note + 16], b"maker note bytes");
	}
}

/// Where the value of the only entry of an IFD0 written by `tiff` starts.
const VALUE_AT: usize = 8 + 2 + 8;

fn orientation_entry(kind: u16, value: Vec<u8>) -> Entry {
	Entry {
		tag: 0x0112,
		kind,
		count: 1,
		value,
	}
}

#[test]
fn orientation_of_any_integer_type_is_read_and_set_upright_in_its_own_type() {
	for big in [true, false] {
		let cases = [
			(1, vec![6], 6, vec![1]),
			(3, bytes16(big, 6).to_vec(), 6, bytes16(big, 1).to_vec()),
			(4, bytes32(big, 6).to_vec(), 6, bytes32(big, 1).to_vec()),
			(8, bytes16(big, 8).to_vec(), 8, bytes16(big, 1).to_vec()),
			(9, bytes32(big, 3).to_vec(), 3, bytes32(big, 1).to_vec()),
		];

		for (kind, value, read, upright) in cases {
			let original = tiff(big, &[orientation_entry(kind, value)], &[], &[]);
			let mut block = original.clone();
			assert_eq!(orientation(&block), Some(read), "type {kind}");

			let result = patch(&mut block, 10, 10).unwrap();

			assert_eq!(result.orientation, Some(read), "type {kind}");
			assert!(result.skipped.is_empty(), "type {kind}: {:?}", result.skipped);
			assert_eq!(orientation(&block), Some(1), "type {kind}");
			assert_eq!(block.len(), original.len());
			assert_eq!(&block[..VALUE_AT], &original[..VALUE_AT], "type {kind} keeps its type");
			assert_eq!(&block[VALUE_AT..VALUE_AT + upright.len()], upright, "type {kind}");
			assert_eq!(
				&block[VALUE_AT + upright.len()..],
				&original[VALUE_AT + upright.len()..],
				"type {kind}"
			);
		}
	}
}

#[test]
fn an_orientation_outside_one_to_eight_reads_as_upright_without_a_warning() {
	for big in [true, false] {
		let values = [
			orientation_entry(4, bytes32(big, 0).to_vec()),
			orientation_entry(3, bytes16(big, 9).to_vec()),
			orientation_entry(1, vec![200]),
			orientation_entry(8, bytes16(big, (-3i16).cast_unsigned()).to_vec()),
			orientation_entry(9, bytes32(big, (-1i32).cast_unsigned()).to_vec()),
			orientation_entry(4, bytes32(big, 65_542).to_vec()),
		];

		for entry in values {
			let mut block = tiff(big, std::slice::from_ref(&entry), &[], &[]);
			assert_eq!(orientation(&block), Some(1), "type {}", entry.kind);

			let result = patch(&mut block, 10, 10).unwrap();

			assert_eq!(result.orientation, Some(1), "type {}", entry.kind);
			assert!(result.skipped.is_empty(), "type {}: {:?}", entry.kind, result.skipped);
			assert_eq!(orientation(&block), Some(1), "type {}", entry.kind);
		}
	}
}

#[test]
fn an_orientation_that_is_not_a_single_number_is_left_with_a_warning() {
	let two_shorts = Entry {
		count: 2,
		..orientation_entry(3, [bytes16(true, 6), bytes16(true, 6)].concat())
	};
	let rational = orientation_entry(5, [bytes32(true, 6), bytes32(true, 1)].concat());
	for entry in [two_shorts, ascii(0x0112, "6"), rational] {
		let original = tiff(true, &[entry], &[], &[]);
		let mut block = original.clone();
		assert_eq!(orientation(&block), None);

		let result = patch(&mut block, 10, 10).unwrap();

		assert_eq!(result.orientation, None);
		assert_eq!(result.skipped, ["Orientation is not a single number"]);
		assert_eq!(block, original);
	}
}

#[test]
fn a_short_too_small_for_the_new_size_is_skipped() {
	let mut block = tiff(true, &[short(true, 0x0112, 1)], &[short(true, 0xA002, 100)], &[]);

	let result = patch(&mut block, 70_000, 10).unwrap();

	assert!(result.patched.is_empty());
	assert!(
		result.skipped[0].contains("does not fit a SHORT"),
		"{:?}",
		result.skipped
	);
}

#[test]
fn broken_ifd_offsets_leave_the_block_untouched() {
	let mut block = camera_block(false);
	block[4..8].copy_from_slice(&0xFFFF_FF00u32.to_le_bytes());
	let before = block.clone();

	let err = patch(&mut block, 800, 1200).unwrap_err();

	assert!(matches!(err, ExifError::IfdOutOfBounds { ifd: "IFD0", .. }), "{err}");
	assert_eq!(block, before);
	assert!(problem(&block).unwrap().contains("IFD0 offset 4294967040"));
	assert_eq!(problem(&camera_block(true)), None);
	assert_eq!(patch(&mut b"JFIF".to_vec(), 1, 1), Err(ExifError::BadHeader));
}

#[test]
fn adobe_rgb_is_recognised_only_from_both_tags() {
	let adobe = tiff(true, &[], &[short(true, 0xA001, 0xFFFF)], &[ascii(0x0001, "R03")]);
	let srgb = tiff(true, &[], &[short(true, 0xA001, 1)], &[ascii(0x0001, "R98")]);
	let uncalibrated = tiff(true, &[], &[short(true, 0xA001, 0xFFFF)], &[]);

	assert!(adobe_rgb_only(&adobe));
	assert!(!adobe_rgb_only(&srgb));
	assert!(!adobe_rgb_only(&uncalibrated));
}

#[test]
fn tiff_files_yield_a_standalone_metadata_block_and_a_page_count() {
	let file = tiff(
		false,
		&[
			long(false, 0x0100, 64),
			long(false, 0x0101, 48),
			long(false, 0x0111, 4096),
			ascii(0x010F, "Scanner Co"),
			short(false, 0x0112, 8),
		],
		&[ascii(0x9003, "2020:01:02 03:04:05"), long(false, 0xA002, 64)],
		&[],
	);

	let block = from_tiff_file(&file).unwrap();

	assert_eq!(orientation(&block), Some(8));
	assert_eq!(problem(&block), None);
	assert!(block.windows(10).any(|w| w == b"Scanner Co"));
	assert!(block.windows(19).any(|w| w == b"2020:01:02 03:04:05"));
	assert!(!block.windows(4).any(|w| w == 4096u32.to_le_bytes()));
	assert_eq!(tiff_pages(&file), Some(1));
	assert_eq!(ifd0_value(&file, 0x010F).as_deref(), Some(b"Scanner Co\0".as_slice()));
	assert_eq!(ifd0_value(&file, 0x02BC), None);
}

#[test]
fn every_full_resolution_page_counts() {
	let mut file = tiff(true, &[long(true, 0x0100, 8)], &[], &[]);
	let second_at = u32::try_from(file.len()).unwrap();
	let third_at = second_at + 18;
	file.extend(ifd(true, 0, &[long(true, 0x0100, 8)], third_at));
	file.extend(ifd(true, 0, &[long(true, 0x00FE, 1)], 0));
	let next = 8 + 2 + 12;
	file[next..next + 4].copy_from_slice(&second_at.to_be_bytes());

	assert_eq!(tiff_pages(&file), Some(2));
}

fn nikon_jpeg(dir: &Path) -> (PathBuf, Vec<u8>) {
	let maker_note = [
		b"Nikon\0\x02\x10\0\0".as_slice(),
		&tiff(
			true,
			&[
				undefined(0x0001, b"0210"),
				ascii(0x0004, "FINE   "),
				short(true, 0x0012, 7),
			],
			&[],
			&[],
		),
	]
	.concat();
	let block = tiff(
		true,
		&[
			ascii(0x010F, "NIKON CORPORATION"),
			ascii(0x0110, "NIKON TEST"),
			short(true, 0x0112, 6),
		],
		&[
			short(true, 0xA002, 1200),
			short(true, 0xA003, 800),
			undefined(0x927C, &maker_note),
		],
		&[],
	);

	let path = dir.join("nikon.jpg");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=64x48",
		"-frames:v",
		"1",
		path.to_str().unwrap(),
	]);
	let jpeg = std::fs::read(&path).unwrap();
	let exif = [b"Exif\0\0".as_slice(), &block].concat();
	std::fs::write(&path, support::with_jpeg_segment(&jpeg, 0xE1, &exif)).unwrap();
	(path, maker_note)
}

#[test]
fn nikon_style_maker_notes_survive_the_patch_of_a_jpeg() {
	let dir = tempfile::tempdir().unwrap();
	let (source, maker_note) = nikon_jpeg(dir.path());
	let original = std::fs::read(&source).unwrap();
	let start = original.windows(6).position(|w| w == b"Exif\0\0").unwrap() + 6;
	let len = usize::from(u16::from_be_bytes([original[start - 8], original[start - 7]])) - 8;
	let mut patched = original.clone();

	let result = patch(&mut patched[start..start + len], 800, 1200).unwrap();

	let out = dir.path().join("patched.jpg");
	std::fs::write(&out, &patched).unwrap();
	assert_eq!(result.orientation, Some(6));
	assert_eq!(patched.len(), original.len());
	assert!(patched.windows(maker_note.len()).any(|window| window == maker_note));

	if Command::new("exiftool").arg("-ver").output().is_err() {
		eprintln!("exiftool is not installed; skipping its read-back");
		return;
	}

	let exiftool = |args: &[&str], path: &Path| Command::new("exiftool").args(args).arg(path).output().unwrap().stdout;
	let maker_notes = exiftool(&["-b", "-MakerNotes"], &source);
	assert!(!maker_notes.is_empty());
	assert_eq!(exiftool(&["-b", "-MakerNotes"], &out), maker_notes);
	let tags = String::from_utf8(exiftool(
		&["-s", "-s", "-s", "-Orientation#", "-ExifImageWidth", "-ExifImageHeight"],
		&out,
	))
	.unwrap();
	assert_eq!(tags.split_whitespace().collect::<Vec<_>>(), ["1", "800", "1200"]);
}
