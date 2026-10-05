mod support;

use std::path::Path;
use std::process::Command;

use cryarchiver_engine::container::avif::{AvifExpectations, AvifMetadata, check};
use cryarchiver_engine::images::avif::{AvifColour, encode, finish, output_cicp};
use cryarchiver_engine::images::plan::RavifSettings;
use cryarchiver_engine::metadata::OutputMetadata;
use cryarchiver_engine::metadata::icc::Cicp;
use image::{DynamicImage, ImageBuffer, Rgb, Rgba};
use moxcms::ColorProfile;

const QUICK: RavifSettings = RavifSettings {
	quality: 80.0,
	speed: 10,
};

fn gradient() -> DynamicImage {
	DynamicImage::ImageRgb8(ImageBuffer::from_fn(64, 48, |x, y| {
		Rgb([u8::try_from(x * 4).unwrap(), u8::try_from(y * 5).unwrap(), 128])
	}))
}

fn ffmpeg_decodes(dir: &Path, bytes: &[u8], alpha: bool) -> bool {
	let path = dir.join("out.avif");
	std::fs::write(&path, bytes).unwrap();
	let mut cmd = Command::new("ffmpeg");
	cmd.args(["-v", "error", "-i"]).arg(&path);
	if alpha {
		cmd.args(["-filter_complex", "[0:v:0][0:v:1]alphamerge"]);
	}
	let out = cmd.args(["-f", "null", "-"]).output().unwrap();
	out.status.success() && out.stderr.is_empty()
}

#[test]
fn an_opaque_still_becomes_a_checked_avif_with_its_metadata() {
	let dir = tempfile::tempdir().unwrap();
	let icc = ColorProfile::new_srgb().encode().unwrap();
	let metadata = OutputMetadata {
		exif: Some(support::exif_block(1, 64, 48)),
		xmp: Some(b"<x:xmpmeta/>".to_vec()),
		warnings: Vec::new(),
	};
	let colour = AvifColour {
		icc: Some(&icc),
		cicp: None,
	};

	let encoded = encode(&gradient(), false, QUICK, 2).unwrap();
	let avif = finish(&encoded, colour, &metadata).unwrap();

	check(
		&avif,
		&AvifExpectations {
			width: 64,
			height: 48,
			alpha: false,
			cicp: Cicp::SRGB,
			metadata: AvifMetadata {
				icc: Some(&icc),
				exif: metadata.exif.as_deref(),
				xmp: metadata.xmp.as_deref(),
			},
			gain_map: None,
		},
	)
	.unwrap();
	assert!(ffmpeg_decodes(dir.path(), &avif, false));
}

#[test]
fn transparent_and_sixteen_bit_stills_encode() {
	let dir = tempfile::tempdir().unwrap();
	let transparent = DynamicImage::ImageRgba8(ImageBuffer::from_fn(64, 48, |x, _| {
		Rgba([200, 100, 50, if x < 32 { 255 } else { 0 }])
	}));
	let widened = DynamicImage::ImageRgb16(ImageBuffer::from_fn(64, 48, |x, y| {
		let value = u16::try_from(x + y).unwrap() * 257;
		Rgb([value, value, value])
	}));

	let with_alpha = finish(
		&encode(&transparent, true, QUICK, 1).unwrap(),
		AvifColour { icc: None, cicp: None },
		&OutputMetadata::default(),
	)
	.unwrap();
	let from_sixteen_bits = encode(&widened, false, QUICK, 1).unwrap();

	check(
		&with_alpha,
		&AvifExpectations {
			width: 64,
			height: 48,
			alpha: true,
			cicp: Cicp::SRGB,
			metadata: AvifMetadata::default(),
			gain_map: None,
		},
	)
	.unwrap();
	assert!(ffmpeg_decodes(dir.path(), &with_alpha, true));
	assert!(!from_sixteen_bits.is_empty());
}

#[test]
fn output_colour_follows_the_icc_then_the_source_cicp_then_srgb() {
	let adobe = ColorProfile::new_adobe_rgb().encode().unwrap();
	let p3 = ColorProfile::new_display_p3().encode().unwrap();
	let pq = Cicp {
		primaries: 9,
		transfer: 16,
	};

	assert_eq!(
		output_cicp(AvifColour {
			icc: Some(&adobe),
			cicp: None
		}),
		Cicp::UNSPECIFIED
	);
	assert_eq!(
		output_cicp(AvifColour {
			icc: Some(&p3),
			cicp: Some(pq)
		}),
		Cicp {
			primaries: 12,
			transfer: 13
		}
	);
	assert_eq!(
		output_cicp(AvifColour {
			icc: None,
			cicp: Some(pq)
		}),
		pq
	);
	assert_eq!(output_cicp(AvifColour { icc: None, cicp: None }), Cicp::SRGB);
	assert_eq!(
		output_cicp(AvifColour {
			icc: None,
			cicp: Some(Cicp {
				primaries: 22,
				transfer: 4
			})
		}),
		Cicp::UNSPECIFIED
	);
}
