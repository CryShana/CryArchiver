mod support;

use std::path::Path;

use cryarchiver_engine::container::jxl::{JxlMetadata, extract_codestream, read_boxes, write_container};

fn encode(dir: &Path, input: &Path, name: &str, extra: &[&str]) -> Vec<u8> {
	let output = dir.join(name);
	let mut args = vec!["-i", input.to_str().unwrap()];
	args.extend_from_slice(extra);
	args.extend_from_slice(&[
		"-c:v",
		"libjxl",
		"-effort",
		"3",
		"-update",
		"1",
		"-frames:v",
		"1",
		output.to_str().unwrap(),
	]);
	support::ffmpeg(&args);
	std::fs::read(output).unwrap()
}

fn decodes(dir: &Path, name: &str, bytes: &[u8]) -> bool {
	let path = dir.join(name);
	std::fs::write(&path, bytes).unwrap();
	std::process::Command::new("ffmpeg")
		.args(["-v", "error", "-i"])
		.arg(&path)
		.args(["-f", "null", "-"])
		.status()
		.unwrap()
		.success()
}

#[test]
fn bare_codestreams_pass_through() {
	let dir = tempfile::tempdir().unwrap();
	let png = dir.path().join("in.png");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=64x48",
		"-frames:v",
		"1",
		png.to_str().unwrap(),
	]);

	let bare = encode(dir.path(), &png, "bare.jxl", &[]);

	let codestream = extract_codestream(&bare).unwrap();
	assert_eq!(&bare[..2], [0xFF, 0x0A]);
	assert_eq!(codestream.bytes, bare);
	assert_eq!(codestream.level, None);
	assert!(!read_boxes(&bare).unwrap().container);
}

#[test]
fn ffmpeg_containers_give_up_their_codestream_and_brotli_exif() {
	let dir = tempfile::tempdir().unwrap();
	let jpeg = support::jpeg_with_exif(dir.path(), "exif.jpg", 64, 48, 1);

	let container = encode(dir.path(), &jpeg, "exif.jxl", &[]);

	let boxes = read_boxes(&container).unwrap();
	assert!(boxes.container);
	assert!(container.windows(4).any(|w| w == b"brob"), "ffmpeg wrote no brob box");
	assert!(boxes.exif.unwrap().starts_with(b"II*\0"));
	let codestream = extract_codestream(&container).unwrap();
	assert!(decodes(dir.path(), "extracted.jxl", &codestream.bytes));
}

#[test]
fn our_container_carries_exif_xmp_and_the_level() {
	let dir = tempfile::tempdir().unwrap();
	let png = dir.path().join("in16.png");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=64x48",
		"-frames:v",
		"1",
		"-pix_fmt",
		"rgb48be",
		png.to_str().unwrap(),
	]);
	let lossless = encode(dir.path(), &png, "deep.jxl", &["-pix_fmt", "rgb48le", "-distance", "0"]);
	let codestream = extract_codestream(&lossless).unwrap();
	let tiff = support::exif_block(1, 64, 48);

	let written = write_container(
		&codestream,
		&JxlMetadata {
			exif: Some(&tiff),
			xmp: Some(b"<x:xmpmeta/>"),
		},
	);

	assert_eq!(codestream.level, Some(10));
	assert!(written.starts_with(&[0, 0, 0, 12, b'J', b'X', b'L', b' ', 0x0D, 0x0A, 0x87, 0x0A]));
	let boxes = read_boxes(&written).unwrap();
	assert_eq!(boxes.exif.as_deref(), Some(tiff.as_slice()));
	assert_eq!(boxes.xmp.as_deref(), Some(b"<x:xmpmeta/>".as_slice()));
	assert_eq!(boxes.level, Some(10));
	assert_eq!(extract_codestream(&written).unwrap(), codestream);
	assert!(decodes(dir.path(), "written.jxl", &written));
}

#[test]
fn broken_containers_are_rejected() {
	let part = |index: u32, data: &[u8]| {
		let mut payload = index.to_be_bytes().to_vec();
		payload.extend_from_slice(data);
		support::heif::boxed(*b"jxlp", &payload)
	};
	let out_of_order = [part(1, &[0xFF, 0x0A]), part(0x8000_0000, &[1])].concat();
	let no_codestream = support::heif::boxed(*b"ftyp", b"jxl \0\0\0\0jxl ");

	assert!(
		extract_codestream(&out_of_order)
			.unwrap_err()
			.to_string()
			.contains("out of order")
	);
	assert!(extract_codestream(&no_codestream).is_err());
	assert!(read_boxes(&support::heif::boxed(*b"brob", b"Exif\x01\x02")).is_err());
}
