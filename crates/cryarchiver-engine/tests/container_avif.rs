mod support;

use std::path::{Path, PathBuf};
use std::process::Command;

use cryarchiver_engine::container::av1::read_sequence_header;
use cryarchiver_engine::container::avif::{AvifExpectations, AvifMetadata, check, rewrite};
use cryarchiver_engine::container::isobmff::{Nclx, Property, parse_meta};
use cryarchiver_engine::metadata::icc::Cicp;
use moxcms::ColorProfile;

fn libaom_still(dir: &Path, alpha: bool) -> Vec<u8> {
	let path = dir.join(if alpha { "alpha.avif" } else { "opaque.avif" });
	let mut args = vec!["-f", "lavfi", "-i"];
	if alpha {
		args.extend_from_slice(&[
			"testsrc2=s=64x48,format=rgba,geq=r='r(X,Y)':g='g(X,Y)':b='b(X,Y)':a='if(lt(X,32),255,0)'",
			"-filter_complex",
			"[0:v]split[c][a];[c]format=yuv444p10le,setparams=colorspace=bt709:range=pc[co];[a]alphaextract,format=gray10le[ao]",
			"-map",
			"[co]",
			"-map",
			"[ao]",
		]);
	} else {
		args.extend_from_slice(&[
			"testsrc2=s=64x48",
			"-vf",
			"format=yuv444p10le,setparams=colorspace=bt709:range=pc",
		]);
	}
	args.extend_from_slice(&[
		"-c:v",
		"libaom-av1",
		"-still-picture",
		"1",
		"-crf",
		"30",
		"-cpu-used",
		"8",
		"-frames:v",
		"1",
		path.to_str().unwrap(),
	]);
	support::ffmpeg(&args);
	std::fs::read(path).unwrap()
}

fn written(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
	let path = dir.join(name);
	std::fs::write(&path, bytes).unwrap();
	path
}

fn ffmpeg_decodes(path: &Path, alpha: bool) -> bool {
	let mut cmd = Command::new("ffmpeg");
	cmd.args(["-v", "error", "-i"]).arg(path);
	if alpha {
		cmd.args(["-filter_complex", "[0:v:0][0:v:1]alphamerge"]);
	}
	let out = cmd.args(["-f", "null", "-"]).output().unwrap();
	out.status.success() && out.stderr.is_empty()
}

#[test]
fn metadata_and_colour_boxes_follow_the_libavif_conventions() {
	let dir = tempfile::tempdir().unwrap();
	let encoded = libaom_still(dir.path(), false);
	let icc = ColorProfile::new_adobe_rgb().encode().unwrap();
	let tiff = support::exif_block(1, 64, 48);
	let metadata = AvifMetadata {
		icc: Some(&icc),
		exif: Some(&tiff),
		xmp: Some(b"<x:xmpmeta/>"),
	};

	let avif = rewrite(&encoded, Cicp::UNSPECIFIED, &metadata).unwrap();

	let expected = AvifExpectations {
		width: 64,
		height: 48,
		alpha: false,
		cicp: Cicp::UNSPECIFIED,
		metadata,
	};
	check(&avif, &expected).unwrap();
	let meta = parse_meta(&avif).unwrap();
	let colours: Vec<&Property> = meta
		.properties_of(meta.primary.unwrap())
		.filter(|property| matches!(property, Property::Icc(_) | Property::Nclx(_)))
		.collect();
	let matrix = read_sequence_header(&meta.item_data(&avif, 1).unwrap())
		.unwrap()
		.matrix
		.unwrap();
	assert_eq!(
		colours,
		[
			&Property::Icc(icc.clone()),
			&Property::Nclx(Nclx {
				primaries: 2,
				transfer: 2,
				matrix: u16::from(matrix),
				full_range: true
			})
		]
	);
	let color = meta.item_data(&avif, meta.primary.unwrap()).unwrap();
	let header = read_sequence_header(&color).unwrap();
	assert_eq!((header.primaries, header.transfer), (Some(2), Some(2)));
	assert!(ffmpeg_decodes(&written(dir.path(), "out.avif", &avif), false));
}

#[test]
fn a_two_stream_alpha_still_keeps_its_alpha_item() {
	let dir = tempfile::tempdir().unwrap();
	let encoded = libaom_still(dir.path(), true);

	let avif = rewrite(&encoded, Cicp::SRGB, &AvifMetadata::default()).unwrap();

	check(
		&avif,
		&AvifExpectations {
			width: 64,
			height: 48,
			alpha: true,
			cicp: Cicp::SRGB,
			metadata: AvifMetadata::default(),
		},
	)
	.unwrap();
	assert!(ffmpeg_decodes(&written(dir.path(), "alpha-out.avif", &avif), true));
}

#[test]
fn check_names_what_does_not_match() {
	let dir = tempfile::tempdir().unwrap();
	let avif = rewrite(&libaom_still(dir.path(), false), Cicp::SRGB, &AvifMetadata::default()).unwrap();
	let expected = AvifExpectations {
		width: 64,
		height: 48,
		alpha: false,
		cicp: Cicp::SRGB,
		metadata: AvifMetadata::default(),
	};
	let tiff = support::exif_block(1, 64, 48);

	assert!(
		check(&avif, &AvifExpectations { width: 48, ..expected })
			.unwrap_err()
			.contains("expected")
	);
	assert!(
		check(
			&avif,
			&AvifExpectations {
				alpha: true,
				..expected
			}
		)
		.unwrap_err()
		.contains("alpha")
	);
	assert!(
		check(
			&avif,
			&AvifExpectations {
				cicp: Cicp::UNSPECIFIED,
				..expected
			}
		)
		.unwrap_err()
		.contains("nclx")
	);
	let wants_exif = AvifExpectations {
		metadata: AvifMetadata {
			exif: Some(&tiff),
			..AvifMetadata::default()
		},
		..expected
	};
	assert!(check(&avif, &wants_exif).unwrap_err().contains("EXIF"));
}

#[test]
#[ignore = "needs the local sample corpus (set CRY_CORPUS, default C:\\media-corpus)"]
fn matches_the_libavif_reference_file() {
	let corpus = std::env::var_os("CRY_CORPUS").map_or_else(|| PathBuf::from(r"C:\media-corpus"), PathBuf::from);
	let reference = std::fs::read(corpus.join("screenshot_with_ICC.avif")).unwrap();
	let reference_meta = parse_meta(&reference).unwrap();
	let icc = reference_meta.primary_icc().unwrap().to_vec();

	let rewrapped = rewrite(
		&reference,
		Cicp::UNSPECIFIED,
		&AvifMetadata {
			icc: Some(&icc),
			..AvifMetadata::default()
		},
	)
	.unwrap();

	let colour_boxes = |bytes: &[u8]| {
		let meta = parse_meta(bytes).unwrap();
		meta.properties_of(meta.primary.unwrap())
			.filter(|property| matches!(property, Property::Icc(_) | Property::Nclx(_)))
			.cloned()
			.collect::<Vec<_>>()
	};
	assert_eq!(colour_boxes(&rewrapped), colour_boxes(&reference));
}
