mod support;

use std::io::Write;

use cryarchiver_engine::container::jxl::{Codestream, JxlMetadata, write_container};
use cryarchiver_engine::metadata::gain_map::{Channel, GainMapMetadata, Signed, Unsigned};
use cryarchiver_engine::metadata::icc::Cicp;
use cryarchiver_engine::metadata::jpeg::image_end;
use cryarchiver_engine::metadata::{AuxKind, ImageFormat, extract, sniff};
use moxcms::ColorProfile;
use support::heif::{HeifFile, HeifItem, aux_c, colr_icc, colr_nclx, exif_item, ispe};
use support::tiff::{Entry, tiff};
use support::ultra_hdr::{UltraHdrOptions, ultra_hdr_jpeg};

const EXTENDED_GUID: &str = "83DE3BB8A00448E560FFAD25263768DF";

fn segment(marker: u8, payload: &[u8]) -> Vec<u8> {
	let mut bytes = vec![0xFF, marker];
	bytes.extend_from_slice(&u16::try_from(payload.len() + 2).unwrap().to_be_bytes());
	bytes.extend_from_slice(payload);
	bytes
}

fn jpeg(segments: &[Vec<u8>], components: u8) -> Vec<u8> {
	let mut frame = vec![8, 0, 16, 0, 16, components];
	for id in 1..=components {
		frame.extend_from_slice(&[id, 0x11, 0]);
	}
	let scan = [1, 1, 0, 0, 63, 0];

	[
		vec![0xFF, 0xD8],
		segments.concat(),
		segment(0xC0, &frame),
		segment(0xDA, &scan),
		vec![0x12, 0x34, 0xFF, 0xD9],
	]
	.concat()
}

fn srgb_icc() -> Vec<u8> {
	ColorProfile::new_srgb().encode().unwrap()
}

fn icc_segments(icc: &[u8], declared_count: u8) -> Vec<Vec<u8>> {
	let half = icc.len() / 2;
	[&icc[..half], &icc[half..]]
		.iter()
		.zip(1u8..)
		.map(|(chunk, sequence)| {
			segment(
				0xE2,
				&[b"ICC_PROFILE\0".as_slice(), &[sequence, declared_count], chunk].concat(),
			)
		})
		.collect()
}

fn xmp_segment(packet: &str) -> Vec<u8> {
	segment(
		0xE1,
		&[b"http://ns.adobe.com/xap/1.0/\0".as_slice(), packet.as_bytes()].concat(),
	)
}

fn extended_segment(full: &str, offset: usize, part: &str) -> Vec<u8> {
	let mut payload = b"http://ns.adobe.com/xmp/extension/\0".to_vec();
	payload.extend_from_slice(EXTENDED_GUID.as_bytes());
	payload.extend_from_slice(&u32::try_from(full.len()).unwrap().to_be_bytes());
	payload.extend_from_slice(&u32::try_from(offset).unwrap().to_be_bytes());
	payload.extend_from_slice(part.as_bytes());
	segment(0xE1, &payload)
}

fn mpf_segment(kinds: &[u32]) -> Vec<u8> {
	let mut tiff = b"II*\0".to_vec();
	tiff.extend_from_slice(&8u32.to_le_bytes());
	tiff.extend_from_slice(&1u16.to_le_bytes());
	tiff.extend_from_slice(&0xB002u16.to_le_bytes());
	tiff.extend_from_slice(&7u16.to_le_bytes());
	tiff.extend_from_slice(&u32::try_from(kinds.len() * 16).unwrap().to_le_bytes());
	tiff.extend_from_slice(&26u32.to_le_bytes());
	tiff.extend_from_slice(&0u32.to_le_bytes());
	for kind in kinds {
		tiff.extend_from_slice(&kind.to_le_bytes());
		tiff.extend_from_slice(&[0; 12]);
	}
	segment(0xE2, &[b"MPF\0".as_slice(), &tiff].concat())
}

fn irb_with_keyword(keyword: &[u8]) -> Vec<u8> {
	let mut iim = vec![0x1C, 2, 25];
	iim.extend_from_slice(&u16::try_from(keyword.len()).unwrap().to_be_bytes());
	iim.extend_from_slice(keyword);
	let mut irb = b"8BIM\x04\x04\0\0".to_vec();
	irb.extend_from_slice(&u32::try_from(iim.len()).unwrap().to_be_bytes());
	irb.extend_from_slice(&iim);
	irb
}

#[test]
fn jpeg_segments_are_collected() {
	let icc = srgb_icc();
	let exif = support::exif_block(6, 16, 16);
	let irb = irb_with_keyword(b"cat");
	let mut segments = vec![
		segment(0xE1, &[b"Exif\0\0".as_slice(), &exif].concat()),
		xmp_segment("<x:xmpmeta/>"),
		segment(0xED, &[b"Photoshop 3.0\0".as_slice(), &irb].concat()),
		segment(0xEE, b"Adobe\0\x64\0\0\0\0\x01"),
		segment(0xFE, b"Lavc63.1.101\0"),
	];
	segments.extend(icc_segments(&icc, 2));

	let metadata = extract(&jpeg(&segments, 3));

	assert_eq!(metadata.exif.as_deref(), Some(exif.as_slice()));
	assert_eq!(metadata.xmp.as_deref(), Some(b"<x:xmpmeta/>".as_slice()));
	assert_eq!(metadata.icc.as_deref(), Some(icc.as_slice()));
	assert_eq!(metadata.icc_problem, None);
	assert_eq!(metadata.irb.as_deref(), Some(irb.as_slice()));
	assert_eq!(metadata.comments, ["Lavc63.1.101"]);
	assert_eq!((metadata.jpeg_components, metadata.adobe_transform), (Some(3), Some(1)));
	assert!(metadata.aux.is_empty());
}

#[test]
fn cmyk_jpegs_and_broken_icc_profiles_are_visible() {
	let icc = srgb_icc();
	let mut oversized = icc.clone();
	oversized[..4].copy_from_slice(&u32::MAX.to_be_bytes());

	let ycck = extract(&jpeg(&[segment(0xEE, b"Adobe\0\x64\0\0\0\0\x02")], 4));
	let miscounted = extract(&jpeg(&icc_segments(&icc, 3), 3));
	let bad_size = extract(&jpeg(&icc_segments(&oversized, 2), 3));

	assert_eq!((ycck.jpeg_components, ycck.adobe_transform), (Some(4), Some(2)));
	assert!(miscounted.icc_problem.unwrap().contains("2 chunks but says 3"));
	assert!(bad_size.icc_problem.unwrap().contains("declares 4294967295 bytes"));
}

#[test]
fn an_extended_xmp_size_bomb_is_refused() {
	let main = format!(
		"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><rdf:Description rdf:about='' xmlns:xmpNote='http://ns.adobe.com/xmp/note/' xmpNote:HasExtendedXMP='{EXTENDED_GUID}'/></rdf:RDF></x:xmpmeta>"
	);
	let mut payload = b"http://ns.adobe.com/xmp/extension/\0".to_vec();
	payload.extend_from_slice(EXTENDED_GUID.as_bytes());
	payload.extend_from_slice(&u32::MAX.to_be_bytes());
	payload.extend_from_slice(&0u32.to_be_bytes());
	payload.extend_from_slice(b"<x:xmpmeta/>");

	let metadata = extract(&jpeg(&[xmp_segment(&main), segment(0xE1, &payload)], 3));

	assert!(metadata.xmp_extended.is_empty());
	assert!(!metadata.warnings.is_empty(), "{:?}", metadata.warnings);
}

#[test]
fn referenced_extended_xmp_is_reassembled() {
	let main = format!(
		"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><rdf:Description rdf:about='' xmlns:xmpNote='http://ns.adobe.com/xmp/note/' xmpNote:HasExtendedXMP='{EXTENDED_GUID}'/></rdf:RDF></x:xmpmeta>"
	);
	let extended = "<x:xmpmeta><rdf:RDF><rdf:Description>long</rdf:Description></rdf:RDF></x:xmpmeta>";
	let (first, second) = extended.split_at(30);

	let metadata = extract(&jpeg(
		&[
			xmp_segment(&main),
			extended_segment(extended, 30, second),
			extended_segment(extended, 0, first),
		],
		3,
	));
	let unreferenced = extract(&jpeg(&[extended_segment(extended, 0, extended)], 3));

	assert_eq!(metadata.xmp_extended, [extended]);
	assert!(unreferenced.xmp_extended.is_empty());
	assert!(
		unreferenced.warnings[0].contains("not referenced"),
		"{:?}",
		unreferenced.warnings
	);
}

#[test]
fn mpf_thumbnails_are_not_auxiliary_but_gain_maps_are() {
	let nikon = extract(&jpeg(&[mpf_segment(&[0x03_0000, 0x01_0002])], 3));
	let undefined = extract(&jpeg(&[mpf_segment(&[0x03_0000, 0x00_0000])], 3));
	let ultra_hdr = extract(&jpeg(
		&[xmp_segment(
			"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><rdf:Description rdf:about='' xmlns:hdrgm='http://ns.adobe.com/hdr-gain-map/1.0/' hdrgm:Version='1.0'/></rdf:RDF></x:xmpmeta>",
		)],
		3,
	));

	assert!(nikon.aux.is_empty(), "{:?}", nikon.aux);
	assert_eq!(undefined.aux, [AuxKind::Other("MPF image type 0x000000".into())]);
	assert_eq!(ultra_hdr.aux, [AuxKind::GainMap("Ultra HDR".into())]);
}

fn mp4_bytes() -> Vec<u8> {
	let mut mp4 = b"\x00\x00\x00\x18ftypisom\x00\x00\x00\x00isommp41".to_vec();
	mp4.extend_from_slice(&4008u32.to_be_bytes());
	mp4.extend_from_slice(b"mdat");
	mp4.extend_from_slice(&[0x42; 4000]);
	mp4
}

fn mpf_with_image(size: u32, offset: u32) -> Vec<u8> {
	let mut tiff = b"II*\x00".to_vec();
	tiff.extend_from_slice(&8u32.to_le_bytes());
	tiff.extend_from_slice(&1u16.to_le_bytes());
	tiff.extend_from_slice(&0xB002u16.to_le_bytes());
	tiff.extend_from_slice(&7u16.to_le_bytes());
	tiff.extend_from_slice(&32u32.to_le_bytes());
	tiff.extend_from_slice(&26u32.to_le_bytes());
	tiff.extend_from_slice(&0u32.to_le_bytes());
	for (kind, size, offset) in [(0x03_0000u32, 0u32, 0u32), (0x01_0002, size, offset)] {
		tiff.extend_from_slice(&kind.to_le_bytes());
		tiff.extend_from_slice(&size.to_le_bytes());
		tiff.extend_from_slice(&offset.to_le_bytes());
		tiff.extend_from_slice(&[0; 4]);
	}
	segment(0xE2, &[b"MPF\x00".as_slice(), &tiff].concat())
}

#[test]
fn videos_after_the_image_and_motion_photo_xmp_are_auxiliary() {
	let plain = jpeg(&[], 3);
	let samsung = [
		plain.clone(),
		mp4_bytes(),
		b"MotionPhoto_Data".to_vec(),
		b"\x00\x00\x00\x00SEFT".to_vec(),
	]
	.concat();
	let appended = [plain.clone(), mp4_bytes()].concat();
	let micro_video = jpeg(
		&[xmp_segment(
			"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><rdf:Description rdf:about='' xmlns:GCamera='http://ns.google.com/photos/1.0/camera/' GCamera:MicroVideo='1' GCamera:MicroVideoOffset='4000'/></rdf:RDF></x:xmpmeta>",
		)],
		3,
	);
	let small_trailer = [plain.clone(), vec![0x42; 100]].concat();
	let padded = [plain.clone(), vec![0; 8192]].concat();
	let without_image = jpeg(&[mpf_with_image(5000, 0)], 3);
	let offset = u32::try_from(without_image.len() - 10).unwrap();
	let thumbnail = [jpeg(&[mpf_with_image(5000, offset)], 3), vec![0x42; 5000]].concat();

	assert_eq!(
		extract(&samsung).aux,
		[AuxKind::MotionVideo("Samsung motion photo".into())]
	);
	assert_eq!(
		extract(&appended).aux,
		[AuxKind::MotionVideo("an MP4 video after the image".into())]
	);
	assert_eq!(
		extract(&micro_video).aux,
		[AuxKind::MotionVideo("Google motion photo".into())]
	);
	assert!(extract(&small_trailer).aux.is_empty());
	assert!(extract(&padded).aux.is_empty());
	assert!(extract(&thumbnail).aux.is_empty(), "{:?}", extract(&thumbnail).aux);
	assert_eq!(
		extract(&[plain, vec![0x42; 5000]].concat()).aux,
		[AuxKind::Other("5000 bytes after the image".into())]
	);
}

/// A Samsung SEF trailer: each record is two padding bytes, its type, its name's length and name, then its data;
/// a `SEFH` directory of (padding, type, offset back from the directory, length) entries ends the file with `SEFT`.
fn sef_trailer(records: &[(&[u8], usize)]) -> Vec<u8> {
	let mut data = Vec::new();
	let mut entries = Vec::new();
	for (name, size) in records {
		let start = data.len();
		data.extend_from_slice(&[0, 0, 0x01, 0x0B]);
		data.extend_from_slice(&u32::try_from(name.len()).unwrap().to_le_bytes());
		data.extend_from_slice(name);
		data.extend(std::iter::repeat_n(0x42, *size));
		entries.push((start, data.len() - start));
	}

	let mut directory = b"SEFH".to_vec();
	directory.extend_from_slice(&107u32.to_le_bytes());
	directory.extend_from_slice(&u32::try_from(entries.len()).unwrap().to_le_bytes());
	for (start, length) in entries {
		directory.extend_from_slice(&[0, 0, 0x01, 0x0B]);
		directory.extend_from_slice(&u32::try_from(data.len() - start).unwrap().to_le_bytes());
		directory.extend_from_slice(&u32::try_from(length).unwrap().to_le_bytes());
	}
	directory.extend_from_slice(&u32::try_from(directory.len()).unwrap().to_le_bytes());
	directory.extend_from_slice(b"SEFT");
	[data, directory].concat()
}

#[test]
fn a_samsung_live_focus_trailer_is_named() {
	let plain = jpeg(&[], 3);
	let live_focus = [
		plain.clone(),
		sef_trailer(&[
			(b"DualShot_1", 3000),
			(b"DualShot_DepthMap_1", 1500),
			(b"DualShot_Extra_Info", 64),
		]),
	]
	.concat();
	let other_sef = [plain.clone(), sef_trailer(&[(b"Image_UTC_Data", 3000)])].concat();
	let mut lying_length = sef_trailer(&[(b"DualShot_1", 3000)]);
	lying_length[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
	let lying = [plain.clone(), lying_length].concat();
	let unframed = [plain, b"\x0a\0\0\0DualShot_1".to_vec(), vec![0x42; 3000]].concat();

	assert_eq!(
		extract(&live_focus).aux,
		[AuxKind::Other(
			"Samsung Live Focus: the unblurred original and its depth map".into()
		)]
	);
	for (bytes, name) in [
		(&other_sef, "other SEF"),
		(&lying, "lying length"),
		(&unframed, "no SEF framing"),
	] {
		assert!(
			matches!(&extract(bytes).aux[..], [AuxKind::Other(detail)] if detail.ends_with("bytes after the image")),
			"{name}: {:?}",
			extract(bytes).aux
		);
	}
}

fn ultra_hdr(opts: &UltraHdrOptions) -> Vec<u8> {
	let base = image::RgbImage::from_fn(64, 48, |x, y| {
		image::Rgb([u8::try_from(x * 4).unwrap(), u8::try_from(y * 5).unwrap(), 90])
	});
	let gain = image::GrayImage::from_fn(16, 12, |x, y| image::Luma([u8::try_from(x * 15 + y).unwrap()]));
	ultra_hdr_jpeg(&base, &gain, opts)
}

fn three_channel_metadata() -> GainMapMetadata {
	let mut metadata = UltraHdrOptions::default().metadata;
	let channel = metadata.channels[0];
	metadata.channels = [2, 3, 4]
		.map(|max| Channel {
			max: Signed {
				numerator: max,
				denominator: 1,
			},
			..channel
		})
		.to_vec();
	metadata
}

#[test]
fn an_ultra_hdr_gain_map_is_found_with_its_metadata() {
	let opts = UltraHdrOptions::default();
	let bytes = ultra_hdr(&opts);

	let metadata = extract(&bytes);
	let gain_map = metadata.gain_map.expect("a gain map");
	let decoded = image::load_from_memory_with_format(&bytes[gain_map.jpeg.clone()], image::ImageFormat::Jpeg).unwrap();

	assert_eq!((decoded.width(), decoded.height()), (16, 12));
	assert_eq!(decoded.color(), image::ColorType::L8);
	assert_eq!(gain_map.jpeg.end, bytes.len());
	assert_eq!(gain_map.metadata, opts.metadata);
	assert!(metadata.warnings.is_empty(), "{:?}", metadata.warnings);
}

#[test]
fn gain_map_metadata_comes_from_iso_or_xmp() {
	let three = three_channel_metadata();
	let xmp_only = UltraHdrOptions {
		iso: false,
		metadata: three.clone(),
		..UltraHdrOptions::default()
	};
	let iso_only = UltraHdrOptions {
		xmp: false,
		metadata: three.clone(),
		..UltraHdrOptions::default()
	};
	let common = |numerator| Signed {
		numerator,
		denominator: 640,
	};
	let legacy = UltraHdrOptions {
		legacy_common_denominator: true,
		metadata: GainMapMetadata {
			use_base_colour_space: true,
			base_headroom: Unsigned {
				numerator: 0,
				denominator: 640,
			},
			alternate_headroom: Unsigned {
				numerator: 1472,
				denominator: 640,
			},
			channels: vec![Channel {
				min: common(0),
				max: common(1472),
				gamma: Unsigned {
					numerator: 640,
					denominator: 640,
				},
				base_offset: common(10),
				alternate_offset: common(10),
			}],
		},
		..UltraHdrOptions::default()
	};
	let disagreeing = UltraHdrOptions {
		xmp_metadata: Some(three.clone()),
		..UltraHdrOptions::default()
	};

	let found = |opts: &UltraHdrOptions| extract(&ultra_hdr(opts)).gain_map.expect("a gain map").metadata;

	assert_eq!(found(&xmp_only), three);
	assert_eq!(found(&iso_only), three);
	assert_eq!(found(&legacy), legacy.metadata);
	assert_eq!(found(&disagreeing), UltraHdrOptions::default().metadata);
}

#[test]
fn a_gain_map_is_found_without_mpf_and_before_a_trailer() {
	let without_mpf = ultra_hdr(&UltraHdrOptions {
		mpf: false,
		..UltraHdrOptions::default()
	});
	let trailer: Vec<u8> = [b"SEFH".as_slice(), &[0x5A; 2040], b"SEFT"].concat();
	let with_trailer = ultra_hdr(&UltraHdrOptions {
		trailer: Some(trailer.clone()),
		..UltraHdrOptions::default()
	});

	let without_mpf = extract(&without_mpf).gain_map.expect("found by the second image");
	let with_trailer_found = extract(&with_trailer).gain_map.expect("found before the trailer");

	assert_eq!(without_mpf.metadata, UltraHdrOptions::default().metadata);
	assert_eq!(with_trailer_found.jpeg.end, with_trailer.len() - trailer.len());
	assert_eq!(&with_trailer[with_trailer_found.jpeg.start..][..3], [0xFF, 0xD8, 0xFF]);
}

#[test]
fn a_plain_jpeg_has_no_gain_map() {
	let mut plain = Vec::new();
	image::codecs::jpeg::JpegEncoder::new_with_quality(&mut plain, 90)
		.encode_image(&image::RgbImage::new(16, 16))
		.unwrap();

	let metadata = extract(&plain);

	assert_eq!(metadata.gain_map, None);
	assert!(metadata.warnings.is_empty(), "{:?}", metadata.warnings);
	assert!(metadata.aux_without_gain_map().is_empty());
}

#[test]
fn the_carried_gain_map_leaves_only_other_auxiliary_images() {
	let ultra_hdr_file = extract(&ultra_hdr(&UltraHdrOptions::default()));
	let with_depth = extract(&ultra_hdr(&UltraHdrOptions {
		depth_item: true,
		..UltraHdrOptions::default()
	}));

	assert_eq!(
		ultra_hdr_file.aux,
		[
			AuxKind::Other("MPF image type 0x000000".into()),
			AuxKind::GainMap("Ultra HDR".into()),
			AuxKind::GainMap("ISO 21496-1 metadata".into()),
		]
	);
	assert!(
		ultra_hdr_file.aux_without_gain_map().is_empty(),
		"{:?}",
		ultra_hdr_file.aux_without_gain_map()
	);
	assert_eq!(
		with_depth.aux_without_gain_map(),
		[&AuxKind::Depth("container item".into())]
	);
}

#[test]
fn an_unrelated_mpf_image_of_the_same_type_is_still_reported() {
	let mut metadata = extract(&ultra_hdr(&UltraHdrOptions::default()));
	metadata.aux.push(AuxKind::Other("MPF image type 0x000000".into()));

	assert_eq!(
		metadata.aux_without_gain_map(),
		[&AuxKind::Other("MPF image type 0x000000".into())]
	);
}

#[test]
fn a_cut_off_gain_map_is_a_warning() {
	let mut bytes = ultra_hdr(&UltraHdrOptions::default());
	bytes.truncate(bytes.len() - 20);

	let metadata = extract(&bytes);

	assert_eq!(metadata.gain_map, None);
	assert!(
		metadata
			.warnings
			.iter()
			.any(|warning| warning.starts_with("the HDR gain map can't be read: ")),
		"{:?}",
		metadata.warnings
	);
	assert_eq!(metadata.aux_without_gain_map().len(), metadata.aux.len());
}

#[test]
fn without_mpf_the_gain_map_is_not_counted_as_trailing_data() {
	let base = image::RgbImage::new(64, 48);
	let noisy_gain = image::GrayImage::from_fn(48, 36, |x, y| {
		image::Luma([u8::try_from((x * 37 + y * 91 + x * y * 13) % 256).unwrap()])
	});
	let trailer: Vec<u8> = [b"SEFH".as_slice(), &[0x5A; 2040], b"SEFT"].concat();
	let without_mpf = extract(&ultra_hdr_jpeg(
		&base,
		&noisy_gain,
		&UltraHdrOptions {
			mpf: false,
			..UltraHdrOptions::default()
		},
	));
	let with_trailer = extract(&ultra_hdr_jpeg(
		&base,
		&noisy_gain,
		&UltraHdrOptions {
			mpf: false,
			trailer: Some(trailer),
			..UltraHdrOptions::default()
		},
	));
	let trailing = |aux: &AuxKind| matches!(aux, AuxKind::Other(detail) if detail.ends_with("bytes after the image"));

	assert!(without_mpf.aux.iter().any(trailing), "{:?}", without_mpf.aux);
	assert!(
		without_mpf.aux_without_gain_map().is_empty(),
		"{:?}",
		without_mpf.aux_without_gain_map()
	);
	assert_eq!(with_trailer.aux.iter().filter(|aux| trailing(aux)).count(), 1);
	assert_eq!(
		with_trailer.aux_without_gain_map(),
		[&AuxKind::Other("2048 bytes after the image".into())]
	);
}

#[test]
fn the_fallback_skips_embedded_images_that_are_not_the_gain_map() {
	let bytes = ultra_hdr(&UltraHdrOptions {
		mpf: false,
		..UltraHdrOptions::default()
	});
	let primary_end = image_end(&bytes).unwrap();
	let decoy = jpeg(&[], 3);
	let with_decoy = [&bytes[..primary_end], &decoy, &bytes[primary_end..]].concat();

	let found = extract(&with_decoy).gain_map.expect("the gain map after the decoy");

	assert_eq!(found.jpeg.start, primary_end + decoy.len());
	assert_eq!(found.metadata, UltraHdrOptions::default().metadata);
}

#[test]
fn broken_gain_map_signals_are_warnings_not_panics() {
	let hdrgm = xmp_segment(
		"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><rdf:Description rdf:about='' xmlns:hdrgm='http://ns.adobe.com/hdr-gain-map/1.0/' hdrgm:Version='1.0'/></rdf:RDF></x:xmpmeta>",
	);
	let iso = |payload: &[u8]| segment(0xE2, &[b"urn:iso:std:iso:ts:21496:-1\0".as_slice(), payload].concat());
	let version_only = jpeg(&[iso(&[0; 4])], 3);
	let cases = [
		(
			jpeg(&[hdrgm.clone(), mpf_with_image(5000, u32::MAX)], 3),
			"no gain map image follows",
		),
		(
			jpeg(&[hdrgm.clone(), mpf_with_image(5000, 1 << 20)], 3),
			"no gain map image follows",
		),
		(jpeg(&[hdrgm], 3), "no gain map image follows"),
		(
			[version_only.clone(), version_only.clone()].concat(),
			"no gain map metadata",
		),
		([version_only, jpeg(&[iso(&[0, 0, 0, 0, 0x40])], 3)].concat(), "cut off"),
	];

	for (bytes, reason) in cases {
		let metadata = extract(&bytes);
		let warnings: Vec<&String> = metadata
			.warnings
			.iter()
			.filter(|warning| warning.starts_with("the HDR gain map can't be read: "))
			.collect();

		assert_eq!(metadata.gain_map, None);
		assert_eq!(warnings.len(), 1, "{:?}", metadata.warnings);
		assert!(warnings[0].contains(reason), "{warnings:?}");
	}
}

#[test]
fn a_run_of_image_starts_after_the_primary_is_probed_in_bounded_time() {
	let hdrgm = xmp_segment(
		"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><rdf:Description rdf:about='' xmlns:hdrgm='http://ns.adobe.com/hdr-gain-map/1.0/' hdrgm:Version='1.0'/></rdf:RDF></x:xmpmeta>",
	);
	let starts = [0xFF, 0xD8].repeat(1 << 20);
	let bytes = [jpeg(&[hdrgm], 3), starts].concat();

	let started = std::time::Instant::now();
	let metadata = extract(&bytes);

	assert!(
		started.elapsed() < std::time::Duration::from_secs(10),
		"{:?}",
		started.elapsed()
	);
	assert_eq!(metadata.gain_map, None);
	assert!(
		metadata
			.warnings
			.iter()
			.any(|warning| warning.starts_with("the HDR gain map can't be read: ")),
		"{:?}",
		metadata.warnings
	);
}

#[test]
fn the_fallback_looks_at_no_more_than_16_embedded_images() {
	let bytes = ultra_hdr(&UltraHdrOptions {
		mpf: false,
		..UltraHdrOptions::default()
	});
	let primary_end = image_end(&bytes).unwrap();
	let decoys = |count| jpeg(&[], 3).repeat(count);
	let behind = |count| [&bytes[..primary_end], &decoys(count), &bytes[primary_end..]].concat();

	assert!(extract(&behind(15)).gain_map.is_some());
	assert_eq!(extract(&behind(16)).gain_map, None);
}

fn chunk(kind: [u8; 4], body: &[u8]) -> Vec<u8> {
	let mut bytes = u32::try_from(body.len()).unwrap().to_be_bytes().to_vec();
	bytes.extend_from_slice(&kind);
	bytes.extend_from_slice(body);
	let mut hasher = crc32fast::Hasher::new();
	hasher.update(&kind);
	hasher.update(body);
	bytes.extend_from_slice(&hasher.finalize().to_be_bytes());
	bytes
}

fn zlib(data: &[u8]) -> Vec<u8> {
	let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
	encoder.write_all(data).unwrap();
	encoder.finish().unwrap()
}

fn png(chunks: &[Vec<u8>]) -> Vec<u8> {
	[b"\x89PNG\r\n\x1a\n".to_vec(), chunks.concat(), chunk(*b"IEND", b"")].concat()
}

#[test]
fn png_chunks_are_collected_even_past_a_bad_crc() {
	let icc = srgb_icc();
	let exif = support::exif_block(1, 16, 16);
	let hex = exif
		.iter()
		.fold(String::new(), |hex, byte| hex + &format!("{byte:02x}"));
	let raw_profile = format!("\nexif\n{}\n{hex}\n", exif.len());
	let mut broken = chunk(*b"tEXt", b"Comment\0x");
	let crc_at = broken.len() - 1;
	broken[crc_at] ^= 0xFF;

	let metadata = extract(&png(&[
		chunk(*b"iCCP", &[b"sRGB\0\0".as_slice(), &zlib(&icc)].concat()),
		chunk(*b"cICP", &[9, 16, 0, 1]),
		chunk(*b"iTXt", b"XML:com.adobe.xmp\0\0\0\0\0<x:xmpmeta/>"),
		chunk(*b"tEXt", b"parameters\0a cat, masterpiece"),
		chunk(
			*b"zTXt",
			&[b"workflow\0\0".as_slice(), &zlib("{\"nodes\":[]}".as_bytes())].concat(),
		),
		chunk(
			*b"zTXt",
			&[b"Raw profile type exif\0\0".as_slice(), &zlib(raw_profile.as_bytes())].concat(),
		),
		broken,
	]));

	assert_eq!(metadata.icc.as_deref(), Some(icc.as_slice()));
	assert_eq!(
		metadata.cicp,
		Some(Cicp {
			primaries: 9,
			transfer: 16
		})
	);
	assert_eq!(metadata.xmp.as_deref(), Some(b"<x:xmpmeta/>".as_slice()));
	assert_eq!(metadata.exif.as_deref(), Some(exif.as_slice()));
	assert_eq!(
		metadata.text,
		[
			("parameters".to_owned(), "a cat, masterpiece".to_owned()),
			("workflow".to_owned(), "{\"nodes\":[]}".to_owned()),
			("Comment".to_owned(), "x".to_owned()),
		]
	);
}

#[test]
fn compressed_text_shares_one_inflate_budget() {
	let four_mib = zlib(&vec![b'a'; 4 << 20]);
	let chunks: Vec<Vec<u8>> = (0..40)
		.map(|index| {
			chunk(
				*b"zTXt",
				&[format!("part{index}\x00\x00").as_bytes(), &four_mib].concat(),
			)
		})
		.collect();

	let metadata = extract(&png(&chunks));

	assert_eq!(metadata.text.len(), 16);
	let budget_warnings: Vec<&String> = metadata
		.warnings
		.iter()
		.filter(|warning| warning.contains("64 MiB"))
		.collect();
	assert_eq!(budget_warnings.len(), 1, "{:?}", metadata.warnings);
}

#[test]
fn an_iccp_bomb_is_refused() {
	let bomb = zlib(&vec![0u8; (64 << 20) + 1]);

	let metadata = extract(&png(&[chunk(*b"iCCP", &[b"bomb\0\0".as_slice(), &bomb].concat())]));

	assert_eq!(metadata.icc, None);
	assert!(metadata.icc_problem.unwrap().contains("64 MiB"));
}

#[test]
fn webp_gif_heif_jxl_and_tiff_blocks_are_collected() {
	let icc = srgb_icc();
	let exif = support::exif_block(3, 16, 16);

	let mut riff_body = b"WEBP".to_vec();
	for (kind, body) in [
		(b"VP8X", vec![0; 10]),
		(b"ICCP", icc.clone()),
		(b"EXIF", [b"Exif\0\0".as_slice(), &exif].concat()),
		(b"XMP ", b"<x/>".to_vec()),
	] {
		riff_body.extend_from_slice(kind);
		riff_body.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
		riff_body.extend_from_slice(&body);
		if body.len() % 2 == 1 {
			riff_body.push(0);
		}
	}
	let webp = [
		b"RIFF".as_slice(),
		&u32::try_from(riff_body.len()).unwrap().to_le_bytes(),
		&riff_body,
	]
	.concat();
	let gif = [
		b"GIF89a\x01\0\x01\0\0\0\0".as_slice(),
		b"\x21\xFE\x05hello\x00",
		b"\x2C\0\0\0\0\x01\0\x01\0\0\x02\x02\x4C\x01\x00\x3B",
	]
	.concat();
	let heif = HeifFile {
		brand: *b"heic",
		primary: 1,
		items: vec![
			HeifItem::new(1, *b"hvc1", vec![0; 8])
				.with(false, ispe(16, 16))
				.with(false, colr_icc(&icc))
				.with(false, colr_nclx(9, 16, 9, false)),
			HeifItem::new(2, *b"hvc1", vec![0; 8]).with(true, aux_c("urn:mpeg:hevc:2015:auxid:2")),
			HeifItem::new(3, *b"Exif", exif_item(&exif)),
		],
		references: vec![(*b"auxl", 2, vec![1]), (*b"cdsc", 3, vec![1])],
	}
	.build();
	let jxl = write_container(
		&Codestream {
			bytes: vec![0xFF, 0x0A, 0],
			level: None,
		},
		&JxlMetadata {
			exif: Some(&exif),
			xmp: Some(b"<x/>"),
		},
	);
	let tiff_file = tiff(
		true,
		&[
			Entry {
				tag: 700,
				kind: 1,
				count: 4,
				value: b"<x/>".to_vec(),
			},
			support::tiff::short(true, 0x0112, 6),
		],
		&[],
		&[],
	);

	let webp = extract(&webp);
	let gif = extract(&gif);
	let heif = extract(&heif);
	let jxl = extract(&jxl);
	let tiff_metadata = extract(&tiff_file);

	assert_eq!(
		(webp.exif.as_deref(), webp.icc.as_deref()),
		(Some(exif.as_slice()), Some(icc.as_slice()))
	);
	assert_eq!(webp.xmp.as_deref(), Some(b"<x/>".as_slice()));
	assert_eq!(gif.comments, ["hello"]);
	assert_eq!(heif.exif.as_deref(), Some(exif.as_slice()));
	assert_eq!(heif.icc.as_deref(), Some(icc.as_slice()));
	assert_eq!(
		heif.cicp,
		Some(Cicp {
			primaries: 9,
			transfer: 16
		})
	);
	assert_eq!(heif.aux, [AuxKind::Depth("urn:mpeg:hevc:2015:auxid:2".into())]);
	assert_eq!(
		(jxl.exif.as_deref(), jxl.xmp.as_deref()),
		(Some(exif.as_slice()), Some(b"<x/>".as_slice()))
	);
	assert_eq!(tiff_metadata.xmp.as_deref(), Some(b"<x/>".as_slice()));
	assert!(tiff_metadata.exif.is_some());
}

#[test]
fn formats_are_sniffed_from_content() {
	let heic = HeifFile {
		brand: *b"heic",
		primary: 1,
		items: vec![HeifItem::new(1, *b"hvc1", vec![0])],
		references: Vec::new(),
	}
	.build();

	assert_eq!(sniff(&jpeg(&[], 3)), ImageFormat::Jpeg);
	assert_eq!(sniff(&png(&[])), ImageFormat::Png);
	assert_eq!(sniff(b"GIF87a\x01\0"), ImageFormat::Gif);
	assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), ImageFormat::WebP);
	assert_eq!(sniff(b"MM\0*\0\0\0\x08"), ImageFormat::Tiff);
	assert_eq!(sniff(&[0xFF, 0x0A, 0]), ImageFormat::Jxl);
	assert_eq!(sniff(&heic), ImageFormat::Heif);
	assert_eq!(sniff(b"plain text"), ImageFormat::Other);
}
