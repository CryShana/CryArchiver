mod support;

use std::path::{Path, PathBuf};
use std::process::Command;

use cryarchiver_engine::container::av1::read_sequence_header;
use cryarchiver_engine::container::avif::{
	AvifExpectations, AvifMetadata, GainMapInput, check, rewrite, writable_cicp,
};
use cryarchiver_engine::container::isobmff::{EntityGroup, Meta, Nclx, Property, brands, parse_meta};
use cryarchiver_engine::images::avif::encode;
use cryarchiver_engine::images::gain_map;
use cryarchiver_engine::images::plan::RavifSettings;
use cryarchiver_engine::metadata::gain_map::GainMapMetadata;
use cryarchiver_engine::metadata::icc::Cicp;
use image::{DynamicImage, GrayImage, ImageBuffer, Luma, Rgb, Rgba};
use moxcms::ColorProfile;
use support::replaced;
use support::ultra_hdr::{UltraHdrOptions, gain_map_av1};

const QUICK: RavifSettings = RavifSettings {
	quality: 80.0,
	speed: 10,
};

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

	let avif = rewrite(&encoded, Cicp::UNSPECIFIED, &metadata, None).unwrap();

	let expected = AvifExpectations {
		width: 64,
		height: 48,
		alpha: false,
		cicp: Cicp::UNSPECIFIED,
		metadata,
		gain_map: None,
		gain_map_coded_cicp: false,
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

	let avif = rewrite(&encoded, Cicp::SRGB, &AvifMetadata::default(), None).unwrap();

	check(
		&avif,
		&AvifExpectations {
			width: 64,
			height: 48,
			alpha: true,
			cicp: Cicp::SRGB,
			metadata: AvifMetadata::default(),
			gain_map: None,
			gain_map_coded_cicp: false,
		},
	)
	.unwrap();
	assert!(ffmpeg_decodes(&written(dir.path(), "alpha-out.avif", &avif), true));
}

#[test]
fn colour_codes_avif_cannot_carry_become_unspecified() {
	let dir = tempfile::tempdir().unwrap();
	let legacy = Cicp {
		primaries: 5,
		transfer: 12,
	};

	let avif = rewrite(&libaom_still(dir.path(), false), legacy, &AvifMetadata::default(), None).unwrap();

	check(
		&avif,
		&AvifExpectations {
			width: 64,
			height: 48,
			alpha: false,
			cicp: Cicp::UNSPECIFIED,
			metadata: AvifMetadata::default(),
			gain_map: None,
			gain_map_coded_cicp: false,
		},
	)
	.unwrap();
	for primaries in [4, 5, 7, 8, 10, 22] {
		assert_eq!(
			writable_cicp(Cicp {
				primaries,
				transfer: 13
			})
			.primaries,
			2
		);
	}
	for transfer in [4, 5, 12] {
		assert_eq!(writable_cicp(Cicp { primaries: 1, transfer }).transfer, 2);
	}
	assert_eq!(writable_cicp(Cicp::SRGB), Cicp::SRGB);
}

#[test]
fn check_names_what_does_not_match() {
	let dir = tempfile::tempdir().unwrap();
	let avif = rewrite(
		&libaom_still(dir.path(), false),
		Cicp::SRGB,
		&AvifMetadata::default(),
		None,
	)
	.unwrap();
	let expected = AvifExpectations {
		width: 64,
		height: 48,
		alpha: false,
		cicp: Cicp::SRGB,
		metadata: AvifMetadata::default(),
		gain_map: None,
		gain_map_coded_cicp: false,
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

fn ravif_still(alpha: bool) -> Vec<u8> {
	let image = if alpha {
		DynamicImage::ImageRgba8(ImageBuffer::from_fn(24, 16, |x, y| {
			Rgba([
				u8::try_from(x * 10).unwrap(),
				u8::try_from(y * 15).unwrap(),
				90,
				if x < 12 { 255 } else { 0 },
			])
		}))
	} else {
		DynamicImage::ImageRgb8(ImageBuffer::from_fn(24, 16, |x, y| {
			Rgb([u8::try_from(x * 10).unwrap(), u8::try_from(y * 15).unwrap(), 90])
		}))
	};
	encode(&image, alpha, QUICK, 1).unwrap()
}

#[test]
fn without_a_gain_map_the_output_bytes_stay_as_before() {
	let mut icc = ColorProfile::new_srgb().encode().unwrap();
	for (at, field) in [2026u16, 10, 5, 7, 3, 28].into_iter().enumerate() {
		icc[24 + 2 * at..26 + 2 * at].copy_from_slice(&field.to_be_bytes());
	}
	let tiff = support::exif_block(1, 24, 16);
	let metadata = AvifMetadata {
		icc: Some(&icc),
		exif: Some(&tiff),
		xmp: Some(b"<x:xmpmeta/>"),
	};
	let opaque = ravif_still(false);
	let alpha = ravif_still(true);
	let fingerprint = |bytes: &[u8]| (bytes.len(), crc32fast::hash(bytes));

	let with_metadata = rewrite(&opaque, Cicp::SRGB, &metadata, None).unwrap();
	let with_alpha = rewrite(&alpha, Cicp::UNSPECIFIED, &AvifMetadata::default(), None).unwrap();

	// The output fingerprints were taken from the writer before it knew gain maps, with moxcms's ICC creation time
	// (header bytes 24..36) fixed as above. If the inputs no longer match, moxcms, ravif or rav1e changed and the pins need
	// retaking from that writer.
	assert_eq!(fingerprint(&icc), (612, 124_158_978));
	assert_eq!(fingerprint(&opaque), (339, 1_661_020_295));
	assert_eq!(fingerprint(&alpha), (560, 1_567_315_361));
	assert_eq!(fingerprint(&with_metadata), (1190, 2_001_786_637));
	assert_eq!(fingerprint(&with_alpha), (560, 213_443_570));
}

fn gain_map_levels(width: u32, height: u32) -> GrayImage {
	GrayImage::from_fn(width, height, |x, y| {
		Luma([u8::try_from((x * 7 + y * 11) % 256).unwrap()])
	})
}

fn grey_gain_map(width: u32, height: u32) -> Vec<u8> {
	gain_map::encode(&gain_map_levels(width, height).into(), 60.0, 10, 1).unwrap()
}

fn ravif_gain_map(width: u32, height: u32) -> Vec<u8> {
	let image = DynamicImage::ImageLuma8(gain_map_levels(width, height));
	encode(&image, false, QUICK, 1).unwrap()
}

fn gain_map_metadata() -> GainMapMetadata {
	UltraHdrOptions::default().metadata
}

fn expectations<'a>(metadata: AvifMetadata<'a>, gain_map: Option<&'a GainMapMetadata>) -> AvifExpectations<'a> {
	AvifExpectations {
		width: 24,
		height: 16,
		alpha: false,
		cicp: Cicp::SRGB,
		metadata,
		gain_map,
		gain_map_coded_cicp: true,
	}
}

fn with_gain_map(metadata: &AvifMetadata<'_>, gain_map: &GainMapMetadata) -> (Vec<u8>, Vec<u8>) {
	with_encoded_gain_map(metadata, gain_map, grey_gain_map(6, 4))
}

fn with_encoded_gain_map(
	metadata: &AvifMetadata<'_>,
	gain_map: &GainMapMetadata,
	gain_map_avif: Vec<u8>,
) -> (Vec<u8>, Vec<u8>) {
	let input = GainMapInput {
		encoded: &gain_map_avif,
		metadata: gain_map,
	};
	let avif = rewrite(&ravif_still(false), Cicp::SRGB, metadata, Some(&input)).unwrap();
	(avif, gain_map_avif)
}

fn nclx_of(meta: &Meta, id: u32) -> Vec<Nclx> {
	meta.properties_of(id)
		.filter_map(|property| match property {
			Property::Nclx(nclx) => Some(*nclx),
			_ => None,
		})
		.collect()
}

#[test]
fn a_gain_map_is_stored_in_libavifs_layout() {
	let dir = tempfile::tempdir().unwrap();
	let metadata = gain_map_metadata();

	let (avif, gain_map_avif) = with_gain_map(&AvifMetadata::default(), &metadata);

	check(&avif, &expectations(AvifMetadata::default(), Some(&metadata))).unwrap();
	assert_eq!(brands(&avif).unwrap(), [*b"avif", *b"mif1", *b"miaf", *b"tmap"]);
	let meta = parse_meta(&avif).unwrap();
	let items: Vec<(u32, [u8; 4], bool)> = meta
		.items
		.iter()
		.map(|item| (item.id, item.kind, item.hidden))
		.collect();
	assert_eq!(items, [(1, *b"av01", false), (2, *b"tmap", false), (3, *b"av01", true)]);
	assert_eq!(meta.primary, Some(1));
	let dimg: Vec<(u32, Vec<u32>)> = meta
		.references
		.iter()
		.filter(|reference| &reference.kind == b"dimg")
		.map(|reference| (reference.from, reference.to.clone()))
		.collect();
	assert_eq!(dimg, [(2, vec![1, 3])]);
	assert_eq!(
		meta.groups,
		[EntityGroup {
			kind: *b"altr",
			id: 4,
			entities: vec![2, 1]
		}]
	);

	let tmap: Vec<&Property> = meta.properties_of(2).collect();
	let base_matrix = nclx_of(&meta, 1)[0].matrix;
	assert_eq!(
		tmap,
		[
			&Property::Size { width: 24, height: 16 },
			&Property::PixelDepths(vec![10, 10, 10]),
			&Property::Nclx(Nclx {
				primaries: 1,
				transfer: 16,
				matrix: base_matrix,
				full_range: true
			})
		]
	);
	let gain_map_properties: Vec<&Property> = meta.properties_of(3).collect();
	assert!(gain_map_properties.contains(&&Property::Size { width: 6, height: 4 }));
	assert_eq!(nclx_of(&meta, 3)[0].primaries, 2);
	assert_eq!(nclx_of(&meta, 3)[0].transfer, 2);
	assert!(meta.item(3).unwrap().properties.iter().any(|link| link.essential));

	let payload = meta.item_data(&avif, 2).unwrap();
	assert_eq!(payload.len(), 62);
	assert_eq!(payload[0], 0);
	assert_eq!(GainMapMetadata::parse_iso(&payload[1..]).unwrap(), metadata);
	let encoded_gain_map = parse_meta(&gain_map_avif)
		.unwrap()
		.item_data(&gain_map_avif, 1)
		.unwrap();
	assert_eq!(gain_map_av1(&avif).unwrap(), encoded_gain_map);
	assert!(ffmpeg_decodes(&written(dir.path(), "gain-map.avif", &avif), false));
}

#[test]
fn metadata_items_follow_the_gain_map_items() {
	let icc = ColorProfile::new_display_p3().encode().unwrap();
	let tiff = support::exif_block(1, 24, 16);
	let metadata = AvifMetadata {
		icc: Some(&icc),
		exif: Some(&tiff),
		xmp: Some(b"<x:xmpmeta/>"),
	};
	let gain_map = gain_map_metadata();

	let (avif, _) = with_gain_map(&metadata, &gain_map);

	check(&avif, &expectations(metadata, Some(&gain_map))).unwrap();
	let meta = parse_meta(&avif).unwrap();
	let items: Vec<(u32, [u8; 4])> = meta.items.iter().map(|item| (item.id, item.kind)).collect();
	assert_eq!(
		items,
		[
			(1, *b"av01"),
			(2, *b"tmap"),
			(3, *b"av01"),
			(4, *b"Exif"),
			(5, *b"mime")
		]
	);
	assert_eq!(meta.groups[0].id, 6);
	assert_eq!(meta.groups[0].entities, [2, 1]);
	let icc_link = |id: u32| {
		meta.item(id)
			.unwrap()
			.properties
			.iter()
			.map(|link| link.index)
			.find(|&index| matches!(meta.properties[index - 1], Property::Icc(_)))
	};
	assert!(icc_link(1).is_some());
	assert_eq!(icc_link(2), icc_link(1));
	assert_eq!(icc_link(3), None);
}

#[test]
fn check_refuses_gain_maps_readers_would_ignore_or_misread() {
	let metadata = gain_map_metadata();
	let (avif, _) = with_gain_map(&AvifMetadata::default(), &metadata);
	let expected = expectations(AvifMetadata::default(), Some(&metadata));
	let refusal = |bytes: &[u8], expected: &AvifExpectations<'_>| check(bytes, expected).unwrap_err();

	let unbranded = replaced(&avif, b"miaftmap", b"miafmiaf");
	assert!(refusal(&unbranded, &expected).contains("'tmap'"));

	let swapped_inputs = replaced(&avif, b"dimg\0\x02\0\x02\0\x01\0\x03", b"dimg\0\x02\0\x02\0\x03\0\x01");
	assert!(refusal(&swapped_inputs, &expected).contains("libavif"));

	let base_preferred = replaced(
		&avif,
		b"altr\0\0\0\0\0\0\0\x04\0\0\0\x02\0\0\0\x02\0\0\0\x01",
		b"altr\0\0\0\0\0\0\0\x04\0\0\0\x02\0\0\0\x01\0\0\0\x02",
	);
	assert!(refusal(&base_preferred, &expected).contains("libavif"));

	let mut other = metadata.clone();
	other.alternate_headroom.numerator += 1;
	assert!(
		refusal(&avif, &expectations(AvifMetadata::default(), Some(&other)))
			.contains("gain map metadata was not carried over")
	);

	assert!(refusal(&avif, &expectations(AvifMetadata::default(), None)).contains("unexpected gain map"));

	let plain = rewrite(&ravif_still(false), Cicp::SRGB, &AvifMetadata::default(), None).unwrap();
	assert!(refusal(&plain, &expected).contains("'tmap'"));
}

fn ipma_entry(meta: &Meta, id: u32) -> Vec<u8> {
	let links = &meta.item(id).unwrap().properties;
	let mut entry = u16::try_from(id).unwrap().to_be_bytes().to_vec();
	entry.push(u8::try_from(links.len()).unwrap());
	entry.extend(
		links
			.iter()
			.map(|link| u8::try_from(link.index).unwrap() | if link.essential { 0x80 } else { 0 }),
	);
	entry
}

#[test]
fn check_refuses_tone_map_and_gain_map_items_that_break_libavifs_rules() {
	let metadata = gain_map_metadata();
	let (avif, _) = with_gain_map(&AvifMetadata::default(), &metadata);
	let expected = expectations(AvifMetadata::default(), Some(&metadata));
	let refusal = |bytes: &[u8]| check(bytes, &expected).unwrap_err();
	let meta = parse_meta(&avif).unwrap();

	let only_major = replaced(&avif, b"avif\0\0\0\0mif1miaftmap", b"tmap\0\0\0\0mif1miafavif");
	assert!(refusal(&only_major).contains("compatible brands"));

	let gain_map_size = meta
		.item(3)
		.unwrap()
		.properties
		.iter()
		.find(|link| matches!(meta.properties[link.index - 1], Property::Size { width: 6, height: 4 }));
	let tmap_entry = ipma_entry(&meta, 2);
	let mut resized_entry = tmap_entry.clone();
	resized_entry[3] = u8::try_from(gain_map_size.unwrap().index).unwrap();
	let resized = replaced(&avif, &tmap_entry, &resized_entry);
	assert!(refusal(&resized).contains("tone map item is Some((6, 4))"));

	let transformed = replaced(&avif, b"colrnclx\0\x01\0\x10", b"paspnclx\0\x01\0\x10");
	assert!(refusal(&transformed).contains("transform box"));

	let not_av1 = replaced(&avif, b"\0\x03\0\0av01GMap", b"\0\x03\0\0hvc1GMap");
	assert!(refusal(&not_av1).contains("not AV1"));

	let shown = replaced(&avif, b"infe\x02\0\0\x01", b"infe\x02\0\0\0");
	assert!(refusal(&shown).contains("not hidden"));

	let without_nclx = replaced(&avif, b"colrnclx\0\x02\0\x02", b"colrnclX\0\x02\0\x02");
	assert!(refusal(&without_nclx).contains("found 0"));

	let coloured = replaced(&avif, b"colrnclx\0\x02\0\x02", b"colrnclx\0\x01\0\x02");
	assert!(refusal(&coloured).contains("nclx box says"));
}

#[test]
fn check_refuses_a_gain_map_item_whose_boxes_disagree_with_its_av1() {
	let metadata = gain_map_metadata();
	let (avif, _) = with_gain_map(&AvifMetadata::default(), &metadata);
	let expected = expectations(AvifMetadata::default(), Some(&metadata));
	let refusal = |bytes: &[u8]| check(bytes, &expected).unwrap_err();

	let ten_bit_pixi = replaced(&avif, b"pixi\0\0\0\0\x01\x08", b"pixi\0\0\0\0\x01\x0a");
	assert!(refusal(&ten_bit_pixi).contains("pixi box"));

	let other_matrix = replaced(&avif, b"colrnclx\0\x02\0\x02\0\x06", b"colrnclx\0\x02\0\x02\0\x01");
	assert!(refusal(&other_matrix).contains("sequence header says matrix"));

	let coloured_av1c = replaced(&avif, b"av1C\x81\x1f\x1c", b"av1C\x81\x1f\x0c");
	assert!(refusal(&coloured_av1c).contains("gain map item's av1C box does not match"));
}

#[test]
fn the_coded_gain_map_cicp_check_refuses_ravifs_described_header() {
	let metadata = gain_map_metadata();
	let (avif, _) = with_encoded_gain_map(&AvifMetadata::default(), &metadata, ravif_gain_map(6, 4));
	let (ours, _) = with_gain_map(&AvifMetadata::default(), &metadata);
	let header = read_sequence_header(&gain_map_av1(&avif).unwrap()).unwrap();
	let our_header = read_sequence_header(&gain_map_av1(&ours).unwrap()).unwrap();

	let strict = expectations(AvifMetadata::default(), Some(&metadata));
	let lenient = AvifExpectations {
		gain_map_coded_cicp: false,
		..strict
	};

	assert_eq!((header.primaries, header.transfer), (Some(1), Some(13)));
	assert!(check(&avif, &strict).unwrap_err().contains("sequence header"));
	check(&avif, &lenient).unwrap();
	assert_eq!((our_header.primaries, our_header.transfer), (Some(2), Some(2)));
	check(&ours, &strict).unwrap();
}

#[test]
#[ignore = "writes CRY_GAINMAP_OUT for libavif's avifgainmaputil"]
fn writes_a_gain_map_avif_for_external_checks() {
	let Some(out) = std::env::var_os("CRY_GAINMAP_OUT") else {
		return;
	};
	let icc = ColorProfile::new_srgb().encode().unwrap();
	let tiff = support::exif_block(1, 24, 16);
	let metadata = AvifMetadata {
		icc: Some(&icc),
		exif: Some(&tiff),
		xmp: Some(b"<x:xmpmeta/>"),
	};
	let gain_map = gain_map_metadata();

	let (avif, _) = with_gain_map(&metadata, &gain_map);

	check(&avif, &expectations(metadata, Some(&gain_map))).unwrap();
	std::fs::write(out, avif).unwrap();
}

#[test]
fn rewrapping_a_profiled_avif_with_its_own_profile_keeps_its_colour_boxes() {
	let dir = tempfile::tempdir().unwrap();
	let icc = ColorProfile::new_display_p3().encode().unwrap();
	let profiled = rewrite(
		&libaom_still(dir.path(), false),
		Cicp::UNSPECIFIED,
		&AvifMetadata {
			icc: Some(&icc),
			..AvifMetadata::default()
		},
		None,
	)
	.unwrap();
	let embedded = parse_meta(&profiled).unwrap().primary_icc().unwrap().to_vec();

	let rewrapped = rewrite(
		&profiled,
		Cicp::UNSPECIFIED,
		&AvifMetadata {
			icc: Some(&embedded),
			..AvifMetadata::default()
		},
		None,
	)
	.unwrap();

	let colour_boxes = |bytes: &[u8]| {
		let meta = parse_meta(bytes).unwrap();
		meta.properties_of(meta.primary.unwrap())
			.filter(|property| matches!(property, Property::Icc(_) | Property::Nclx(_)))
			.cloned()
			.collect::<Vec<_>>()
	};
	assert_eq!(embedded, icc);
	assert_eq!(colour_boxes(&rewrapped), colour_boxes(&profiled));
	assert!(ffmpeg_decodes(
		&written(dir.path(), "rewrapped.avif", &rewrapped),
		false
	));
}
