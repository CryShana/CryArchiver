mod support;

use std::path::Path;

use cryarchiver_engine::container::av1::{patch_cicp, read_sequence_header};

fn obus(dir: &Path, pix_fmt: &str, encoder: &[&str]) -> Vec<u8> {
	let path = dir.join(format!("{pix_fmt}.obu"));
	let filter = format!("format={pix_fmt},setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709:range=pc");
	let mut args = vec![
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=64x48",
		"-frames:v",
		"1",
		"-vf",
		filter.as_str(),
	];
	args.extend_from_slice(encoder);
	args.extend_from_slice(&["-f", "obu", path.to_str().unwrap()]);
	support::ffmpeg(&args);
	std::fs::read(path).unwrap()
}

#[test]
fn reads_libaom_444_svt_420_and_monochrome_headers() {
	let dir = tempfile::tempdir().unwrap();
	let still = ["-c:v", "libaom-av1", "-still-picture", "1"];

	let full = read_sequence_header(&obus(dir.path(), "yuv444p10le", &still)).unwrap();
	let svt = read_sequence_header(&obus(dir.path(), "yuv420p10le", &["-c:v", "libsvtav1"])).unwrap();
	let gray = read_sequence_header(&obus(dir.path(), "gray10le", &still)).unwrap();

	assert_eq!((full.width, full.height, full.bit_depth), (64, 48, 10));
	assert_eq!(
		(full.profile, full.subsampling, full.monochrome),
		(1, (false, false), false)
	);
	assert_eq!(
		(full.primaries, full.transfer, full.matrix),
		(Some(1), Some(1), Some(1))
	);
	assert!(full.full_range);
	assert_eq!((svt.profile, svt.subsampling), (0, (true, true)));
	assert!(gray.monochrome);
}

#[test]
fn cicp_is_patched_in_place() {
	let dir = tempfile::tempdir().unwrap();
	let mut data = obus(
		dir.path(),
		"yuv444p10le",
		&["-c:v", "libaom-av1", "-still-picture", "1"],
	);
	let before = read_sequence_header(&data).unwrap();
	let len = data.len();

	assert!(patch_cicp(&mut data, 2, 2));

	let after = read_sequence_header(&data).unwrap();
	assert_eq!(data.len(), len);
	assert_eq!((after.primaries, after.transfer), (Some(2), Some(2)));
	assert_eq!(after.matrix, before.matrix);
	assert_eq!(
		(after.width, after.height, after.full_range),
		(before.width, before.height, before.full_range)
	);
}

#[test]
fn data_without_a_sequence_header_is_rejected() {
	let mut temporal_delimiter_only = vec![0x12, 0x00];

	assert_eq!(read_sequence_header(&temporal_delimiter_only), None);
	assert!(!patch_cicp(&mut temporal_delimiter_only, 1, 13));
}
