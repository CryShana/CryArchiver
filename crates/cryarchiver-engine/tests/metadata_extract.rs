mod support;

use std::io::Write;

use cryarchiver_engine::container::jxl::{Codestream, JxlMetadata, write_container};
use cryarchiver_engine::metadata::icc::Cicp;
use cryarchiver_engine::metadata::{AuxKind, ImageFormat, extract, sniff};
use moxcms::ColorProfile;
use support::heif::{HeifFile, HeifItem, aux_c, colr_icc, colr_nclx, exif_item, ispe};
use support::tiff::{Entry, tiff};

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
