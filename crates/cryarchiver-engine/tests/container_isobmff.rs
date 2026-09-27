mod support;

use cryarchiver_engine::container::isobmff::{Nclx, brands, parse_meta};
use support::heif::{HeifFile, HeifItem, aux_c, boxed, colr_icc, colr_nclx, exif_item, full_box, imir, irot, ispe};

const TIFF: &[u8] = b"MM\0*\0\0\0\x08\0\0\0\0\0\0";
const ALPHA: &str = "urn:mpeg:mpegB:cicp:systems:auxiliary:alpha";

fn still(properties: Vec<Vec<u8>>) -> Vec<u8> {
	let mut color = HeifItem::new(1, *b"av01", vec![0xAA; 16]).with(false, ispe(60, 40));
	for property in properties {
		color = color.with(true, property);
	}
	HeifFile {
		brand: *b"avif",
		primary: 1,
		items: vec![color],
		references: Vec::new(),
	}
	.build()
}

#[test]
fn reads_items_properties_and_item_data() {
	let file = HeifFile {
		brand: *b"heic",
		primary: 1,
		items: vec![
			HeifItem::new(1, *b"hvc1", vec![1, 2, 3, 4])
				.with(false, ispe(60, 40))
				.with(false, colr_icc(b"icc bytes"))
				.with(false, colr_nclx(2, 2, 6, true)),
			HeifItem::new(2, *b"Exif", exif_item(TIFF)),
			HeifItem {
				content_type: Some("application/rdf+xml"),
				..HeifItem::new(3, *b"mime", b"<x:xmpmeta/>".to_vec())
			},
		],
		references: vec![(*b"cdsc", 2, vec![1]), (*b"cdsc", 3, vec![1])],
	}
	.build();

	let meta = parse_meta(&file).unwrap();

	assert_eq!(brands(&file).unwrap()[0], *b"heic");
	assert_eq!(meta.handler, Some(*b"pict"));
	assert_eq!(meta.primary_item().unwrap().kind, *b"hvc1");
	assert_eq!(meta.item_data(&file, 1).unwrap(), [1, 2, 3, 4]);
	assert_eq!(meta.primary_size(), Some((60, 40)));
	assert_eq!(meta.primary_icc(), Some(b"icc bytes".as_slice()));
	assert_eq!(
		meta.primary_nclx(),
		Some(Nclx {
			primaries: 2,
			transfer: 2,
			matrix: 6,
			full_range: true
		})
	);
	assert_eq!(meta.exif_tiff(&file).unwrap().unwrap(), TIFF);
	assert_eq!(meta.xmp(&file).unwrap().unwrap(), b"<x:xmpmeta/>");
	assert_eq!(meta.extent_problem(file.len() as u64), None);
}

#[test]
fn irot_and_imir_become_exif_orientations() {
	let orientation = |properties| parse_meta(&still(properties)).unwrap().primary_orientation();

	assert_eq!(orientation(vec![]), 1);
	assert_eq!(orientation(vec![irot(1)]), 8);
	assert_eq!(orientation(vec![irot(2)]), 3);
	assert_eq!(orientation(vec![irot(3)]), 6);
	assert_eq!(orientation(vec![imir(0)]), 4);
	assert_eq!(orientation(vec![imir(1)]), 2);
	assert_eq!(orientation(vec![irot(1), imir(0)]), 5);
	assert_eq!(orientation(vec![irot(1), imir(1)]), 7);
	assert_eq!(orientation(vec![irot(2), imir(0)]), 2);
	assert_eq!(orientation(vec![irot(3), imir(0)]), 7);
	assert_eq!(orientation(vec![irot(3), imir(1)]), 5);
}

#[test]
fn alpha_and_other_auxiliary_items_are_told_apart() {
	let file = HeifFile {
		brand: *b"avif",
		primary: 1,
		items: vec![
			HeifItem::new(1, *b"av01", vec![1]).with(false, ispe(60, 40)),
			HeifItem::new(2, *b"av01", vec![2]).with(true, aux_c(ALPHA)),
			HeifItem::new(3, *b"hvc1", vec![3]).with(true, aux_c("urn:mpeg:hevc:2015:auxid:2")),
			HeifItem::new(4, *b"tmap", vec![0; 4]),
		],
		references: vec![(*b"auxl", 2, vec![1]), (*b"auxl", 3, vec![1]), (*b"prem", 1, vec![2])],
	}
	.build();

	let meta = parse_meta(&file).unwrap();

	assert_eq!(meta.alpha_item(), Some(2));
	assert!(meta.premultiplied());
	assert_eq!(meta.auxiliary_urns(), ["urn:mpeg:hevc:2015:auxid:2"]);
	assert!(meta.has_item_kind(*b"tmap"));
}

#[test]
fn truncated_items_and_missing_meta_are_reported() {
	let file = still(vec![]);
	let meta = parse_meta(&file).unwrap();
	let cut = &file[..file.len() - 4];

	assert!(meta.extent_problem(cut.len() as u64).unwrap().contains("item 1"));
	assert!(meta.item_data(cut, 1).is_err());
	assert!(
		parse_meta(b"\0\0\0\x10ftypavif\0\0\0\0")
			.unwrap_err()
			.to_string()
			.contains("meta")
	);
	assert!(parse_meta(&file[..40]).is_err());
}

fn with_iloc(iloc: &[u8]) -> Vec<u8> {
	let ftyp = boxed(*b"ftyp", b"avif\0\0\0\0mif1avif");
	let hdlr = full_box(*b"hdlr", 0, 0, &[&[0u8; 4][..], b"pict", &[0; 13]].concat());
	let pitm = full_box(*b"pitm", 0, 0, &1u16.to_be_bytes());
	let meta = full_box(*b"meta", 0, 0, &[hdlr, pitm, iloc.to_vec()].concat());
	[ftyp, meta, boxed(*b"mdat", &[0xAA; 64])].concat()
}

#[test]
fn empty_extents_repeated_in_iloc_are_rejected() {
	let mut body = vec![0x00, 0x00];
	body.extend_from_slice(&1u16.to_be_bytes());
	body.extend_from_slice(&1u16.to_be_bytes());
	body.extend_from_slice(&0u16.to_be_bytes());
	body.extend_from_slice(&u16::MAX.to_be_bytes());
	let file = with_iloc(&full_box(*b"iloc", 0, 0, &body));

	let err = parse_meta(&file).unwrap_err();

	assert!(err.to_string().contains("empty extent"), "{err}");
}

#[test]
fn repeated_rest_of_file_extents_cannot_outgrow_the_file() {
	let extents = 4000u16;
	let mut body = vec![0x40, 0x00];
	body.extend_from_slice(&1u16.to_be_bytes());
	body.extend_from_slice(&1u16.to_be_bytes());
	body.extend_from_slice(&0u16.to_be_bytes());
	body.extend_from_slice(&extents.to_be_bytes());
	for _ in 0..extents {
		body.extend_from_slice(&0u32.to_be_bytes());
	}
	let file = with_iloc(&full_box(*b"iloc", 0, 0, &body));
	let meta = parse_meta(&file).unwrap();

	let err = meta.item_data(&file, 1).unwrap_err();

	assert_eq!(meta.locations[0].extents.len(), usize::from(extents));
	assert!(err.to_string().contains("larger than the file"), "{err}");
}

#[test]
fn iloc_extent_counts_beyond_the_box_are_truncation() {
	let mut body = vec![0x44, 0x00];
	body.extend_from_slice(&1u16.to_be_bytes());
	body.extend_from_slice(&1u16.to_be_bytes());
	body.extend_from_slice(&0u16.to_be_bytes());
	body.extend_from_slice(&u16::MAX.to_be_bytes());
	body.extend_from_slice(&[0; 8]);
	let file = with_iloc(&full_box(*b"iloc", 0, 0, &body));

	assert!(parse_meta(&file).unwrap_err().to_string().contains("truncated"));
}
