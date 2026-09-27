mod support;

use std::process::Command;

use cryarchiver_engine::metadata::exif::orientation;
use cryarchiver_engine::metadata::{MetadataMode, SourceMetadata, build_output};

fn irb_with_keywords(keywords: &[&str]) -> Vec<u8> {
	let mut iim = Vec::new();
	for keyword in keywords {
		iim.extend_from_slice(&[0x1C, 2, 25]);
		iim.extend_from_slice(&u16::try_from(keyword.len()).unwrap().to_be_bytes());
		iim.extend_from_slice(keyword.as_bytes());
	}
	let mut irb = b"8BIM\x04\x04\0\0".to_vec();
	irb.extend_from_slice(&u32::try_from(iim.len()).unwrap().to_be_bytes());
	irb.extend_from_slice(&iim);
	irb
}

fn camera_source() -> SourceMetadata {
	SourceMetadata {
		exif: Some(support::exif_block(6, 1200, 800)),
		irb: Some(irb_with_keywords(&["cat", "テスト"])),
		comments: vec!["Lavc63.1.101".into()],
		text: vec![("parameters".into(), "a cat, <masterpiece>".into())],
		..SourceMetadata::default()
	}
}

#[test]
fn full_mode_patches_exif_and_builds_xmp_from_iptc_comments_and_text() {
	let output = build_output(&camera_source(), MetadataMode::Full, 800, 1200);

	let exif = output.exif.unwrap();
	assert_eq!(orientation(&exif), Some(1));
	assert!(exif.windows(4).any(|w| w == 800u32.to_le_bytes()));
	let xmp = String::from_utf8(output.xmp.unwrap()).unwrap();
	assert!(xmp.contains("<rdf:li>テスト</rdf:li>"), "{xmp}");
	assert!(xmp.contains("<cryarchiver:IPTCIIM>OEJJTQ"), "{xmp}");
	assert!(
		xmp.contains("<cryarchiver:Comment>Lavc63.1.101</cryarchiver:Comment>"),
		"{xmp}"
	);
	assert!(
		xmp.contains("<rdf:li>parameters=a cat, &lt;masterpiece&gt;</rdf:li>"),
		"{xmp}"
	);
	assert!(output.warnings.is_empty(), "{:?}", output.warnings);
}

#[test]
fn clean_mode_writes_no_personal_metadata() {
	let output = build_output(&camera_source(), MetadataMode::IccOnly, 800, 1200);

	assert_eq!(output.exif, None);
	assert_eq!(output.xmp, None);
}

#[test]
fn an_existing_packet_is_patched_and_nothing_is_invented() {
	let source = SourceMetadata {
		xmp: Some(
			b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><rdf:Description rdf:about='' xmlns:tiff='http://ns.adobe.com/tiff/1.0/' tiff:Orientation='6'/></rdf:RDF></x:xmpmeta>".to_vec(),
		),
		..SourceMetadata::default()
	};

	let patched = String::from_utf8(build_output(&source, MetadataMode::Full, 10, 20).xmp.unwrap()).unwrap();
	let nothing = build_output(&SourceMetadata::default(), MetadataMode::Full, 10, 20);

	assert!(patched.contains("tiff:Orientation=\"1\""), "{patched}");
	assert!(!patched.contains("cryarchiver"), "{patched}");
	assert_eq!((nothing.exif, nothing.xmp), (None, None));
}

#[test]
fn unreadable_blocks_are_copied_unchanged_with_a_warning() {
	let mut broken_exif = support::exif_block(6, 10, 10);
	broken_exif[4..8].copy_from_slice(&0xFFFF_FF00u32.to_le_bytes());
	let source = SourceMetadata {
		exif: Some(broken_exif.clone()),
		xmp: Some(vec![0xFF, 0xFE, b'<', 0]),
		..SourceMetadata::default()
	};

	let output = build_output(&source, MetadataMode::Full, 10, 10);

	assert_eq!(output.exif, Some(broken_exif));
	assert_eq!(output.xmp, Some(vec![0xFF, 0xFE, b'<', 0]));
	assert!(
		output
			.warnings
			.iter()
			.any(|warning| warning.starts_with("EXIF copied without changes"))
	);
	assert!(output.warnings.iter().any(|warning| warning.contains("not UTF-8")));
}

#[test]
fn exiftool_reads_the_created_packet() {
	if Command::new("exiftool").arg("-ver").output().is_err() {
		eprintln!("exiftool is not installed; skipping");
		return;
	}
	let dir = tempfile::tempdir().unwrap();
	let path = dir.path().join("out.xmp");
	std::fs::write(
		&path,
		build_output(&camera_source(), MetadataMode::Full, 800, 1200)
			.xmp
			.unwrap(),
	)
	.unwrap();

	let out = Command::new("exiftool")
		.args(["-charset", "utf8", "-G1", "-s"])
		.arg(&path)
		.output()
		.unwrap();

	let tags = String::from_utf8_lossy(&out.stdout);
	let has = |group: &str, value: &str| {
		tags.lines()
			.any(|line| line.starts_with(group) && line.ends_with(value))
	};
	assert!(has("[XMP-dc]", ": cat, テスト"), "{tags}");
	assert!(has("[XMP-cryarchiver]", ": Lavc63.1.101"), "{tags}");
	assert!(has("[XMP-cryarchiver]", ": parameters=a cat, <masterpiece>"), "{tags}");
}
