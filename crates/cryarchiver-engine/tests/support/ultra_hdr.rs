use std::fmt::Write as _;

use cryarchiver_engine::container::isobmff;
use cryarchiver_engine::metadata::gain_map::{Channel, GainMapMetadata, Signed, Unsigned};
use image::codecs::jpeg::JpegEncoder;
use image::{GenericImageView, PixelWithColorType};

const XMP: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
const ISO: &[u8] = b"urn:iso:std:iso:ts:21496:-1\0";
const MPF: &[u8] = b"MPF\0";

#[allow(clippy::struct_excessive_bools)]
pub struct UltraHdrOptions {
	pub iso: bool,
	pub xmp: bool,
	pub mpf: bool,
	pub legacy_common_denominator: bool,
	pub metadata: GainMapMetadata,
	/// What the gain map's XMP says when it should differ from `metadata`.
	pub xmp_metadata: Option<GainMapMetadata>,
	pub depth_item: bool,
	pub motion_photo: bool,
	pub exif_orientation: Option<u16>,
	pub trailer: Option<Vec<u8>>,
}

impl Default for UltraHdrOptions {
	fn default() -> Self {
		let headroom = Signed {
			numerator: 23,
			denominator: 10,
		};
		let offset = Signed {
			numerator: 1,
			denominator: 64,
		};

		Self {
			iso: true,
			xmp: true,
			mpf: true,
			legacy_common_denominator: false,
			metadata: GainMapMetadata {
				use_base_colour_space: true,
				base_headroom: Unsigned {
					numerator: 0,
					denominator: 1,
				},
				alternate_headroom: Unsigned {
					numerator: 23,
					denominator: 10,
				},
				channels: vec![Channel {
					min: Signed {
						numerator: 0,
						denominator: 1,
					},
					max: headroom,
					gamma: Unsigned {
						numerator: 1,
						denominator: 1,
					},
					base_offset: offset,
					alternate_offset: offset,
				}],
			},
			xmp_metadata: None,
			depth_item: false,
			motion_photo: false,
			exif_orientation: None,
			trailer: None,
		}
	}
}

pub fn ultra_hdr_jpeg<G: GenericImageView>(base: &image::RgbImage, gain: &G, opts: &UltraHdrOptions) -> Vec<u8>
where
	G::Pixel: PixelWithColorType,
{
	let gain_map = gain_map_jpeg(gain, opts);
	let base_jpeg = encode(base);

	let mut head = vec![0xFF, 0xD8];
	if let Some(orientation) = opts.exif_orientation {
		let exif = super::exif_block(orientation, base.width(), base.height());
		head.extend(segment(0xE1, &[b"Exif\0\0".as_slice(), &exif].concat()));
	}

	if opts.xmp {
		head.extend(segment(
			0xE1,
			&[
				XMP,
				primary_xmp(gain_map.len(), opts.depth_item, opts.motion_photo).as_bytes(),
			]
			.concat(),
		));
	}

	if opts.iso {
		head.extend(segment(0xE2, &[ISO, &[0, 0, 0, 0]].concat()));
	}

	let mpf = opts.mpf.then(|| {
		let mpf_at = head.len() + 4 + MPF.len();
		let primary_len = head.len() + mpf_segment(0, 0, 0).len() + base_jpeg.len() - 2;
		mpf_segment(primary_len, gain_map.len(), primary_len - mpf_at)
	});

	[
		head,
		mpf.unwrap_or_default(),
		base_jpeg[2..].to_vec(),
		gain_map,
		opts.trailer.clone().unwrap_or_default(),
	]
	.concat()
}

pub fn gain_map_av1(avif: &[u8]) -> Result<Vec<u8>, String> {
	let meta = isobmff::parse_meta(avif).map_err(|err| err.to_string())?;
	let items = meta.gain_map().ok_or("the AVIF has no gain map")?;
	meta.item_data(avif, items.gain_map).map_err(|err| err.to_string())
}

fn gain_map_jpeg<G: GenericImageView>(gain: &G, opts: &UltraHdrOptions) -> Vec<u8>
where
	G::Pixel: PixelWithColorType,
{
	let encoded = encode(gain);

	let mut out = vec![0xFF, 0xD8];
	if opts.xmp {
		let metadata = opts.xmp_metadata.as_ref().unwrap_or(&opts.metadata);
		out.extend(segment(0xE1, &[XMP, gain_map_xmp(metadata).as_bytes()].concat()));
	}

	if opts.iso {
		let payload = if opts.legacy_common_denominator {
			legacy_iso(&opts.metadata)
		} else {
			opts.metadata.to_iso()
		};
		out.extend(segment(0xE2, &[ISO, &payload].concat()));
	}

	out.extend_from_slice(&encoded[2..]);
	out
}

fn encode<I: GenericImageView>(image: &I) -> Vec<u8>
where
	I::Pixel: PixelWithColorType,
{
	let mut out = Vec::new();
	JpegEncoder::new_with_quality(&mut out, 90).encode_image(image).unwrap();
	out
}

fn segment(marker: u8, payload: &[u8]) -> Vec<u8> {
	let mut bytes = vec![0xFF, marker];
	bytes.extend_from_slice(&u16::try_from(payload.len() + 2).unwrap().to_be_bytes());
	bytes.extend_from_slice(payload);
	bytes
}

fn mpf_segment(primary_len: usize, gain_map_len: usize, gain_map_offset: usize) -> Vec<u8> {
	let u32_of = |value: usize| u32::try_from(value).unwrap().to_be_bytes();
	let entries_at = 8 + 2 + 3 * 12 + 4;

	let mut tiff = b"MM\0*".to_vec();
	tiff.extend_from_slice(&8u32.to_be_bytes());
	tiff.extend_from_slice(&3u16.to_be_bytes());
	for (tag, kind, count, value) in [
		(0xB000u16, 7u16, 4u32, u32::from_be_bytes(*b"0100")),
		(0xB001, 4, 1, 2),
		(0xB002, 7, 32, entries_at),
	] {
		tiff.extend_from_slice(&tag.to_be_bytes());
		tiff.extend_from_slice(&kind.to_be_bytes());
		tiff.extend_from_slice(&count.to_be_bytes());
		tiff.extend_from_slice(&value.to_be_bytes());
	}
	tiff.extend_from_slice(&0u32.to_be_bytes());

	for (attribute, size, offset) in [(0x03_0000, primary_len, 0), (0x00_0000, gain_map_len, gain_map_offset)] {
		tiff.extend_from_slice(&u32::to_be_bytes(attribute));
		tiff.extend_from_slice(&u32_of(size));
		tiff.extend_from_slice(&u32_of(offset));
		tiff.extend_from_slice(&[0; 4]);
	}
	segment(0xE2, &[MPF, &tiff].concat())
}

fn primary_xmp(gain_map_len: usize, depth_item: bool, motion_photo: bool) -> String {
	let depth = if depth_item {
		"<rdf:li rdf:parseType='Resource'><Container:Item Item:Semantic='Depth' Item:Mime='image/jpeg'/></rdf:li>"
	} else {
		""
	};
	let camera = if motion_photo {
		" xmlns:GCamera='http://ns.google.com/photos/1.0/camera/' GCamera:MotionPhoto='1'"
	} else {
		""
	};
	format!(
		"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'>\
		 <rdf:Description rdf:about='' xmlns:Container='http://ns.google.com/photos/1.0/container/' \
		 xmlns:Item='http://ns.google.com/photos/1.0/container/item/' xmlns:hdrgm='http://ns.adobe.com/hdr-gain-map/1.0/' \
		 hdrgm:Version='1.0'{camera}><Container:Directory><rdf:Seq>\
		 <rdf:li rdf:parseType='Resource'><Container:Item Item:Semantic='Primary' Item:Mime='image/jpeg'/></rdf:li>\
		 <rdf:li rdf:parseType='Resource'><Container:Item Item:Semantic='GainMap' Item:Mime='image/jpeg' Item:Length='{gain_map_len}'/></rdf:li>\
		 {depth}</rdf:Seq></Container:Directory></rdf:Description></rdf:RDF></x:xmpmeta>"
	)
}

fn gain_map_xmp(metadata: &GainMapMetadata) -> String {
	let signed = |value: Signed| f64::from(value.numerator) / f64::from(value.denominator);
	let unsigned = |value: Unsigned| f64::from(value.numerator) / f64::from(value.denominator);
	let per_channel: [(&str, Vec<f64>); 5] = [
		("GainMapMin", metadata.channels.iter().map(|c| signed(c.min)).collect()),
		("GainMapMax", metadata.channels.iter().map(|c| signed(c.max)).collect()),
		("Gamma", metadata.channels.iter().map(|c| unsigned(c.gamma)).collect()),
		(
			"OffsetSDR",
			metadata.channels.iter().map(|c| signed(c.base_offset)).collect(),
		),
		(
			"OffsetHDR",
			metadata.channels.iter().map(|c| signed(c.alternate_offset)).collect(),
		),
	];

	let mut attributes = format!(
		"hdrgm:Version='1.0' hdrgm:HDRCapacityMin='{}' hdrgm:HDRCapacityMax='{}' hdrgm:BaseRenditionIsHDR='False'",
		unsigned(metadata.base_headroom),
		unsigned(metadata.alternate_headroom)
	);
	let mut children = String::new();
	for (name, values) in per_channel {
		if let [value] = values[..] {
			let _ = write!(attributes, " hdrgm:{name}='{value}'");
		} else {
			let _ = write!(children, "<hdrgm:{name}><rdf:Seq>");
			for value in values {
				let _ = write!(children, "<rdf:li>{value}</rdf:li>");
			}
			let _ = write!(children, "</rdf:Seq></hdrgm:{name}>");
		}
	}

	format!(
		"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'>\
		 <rdf:Description rdf:about='' xmlns:hdrgm='http://ns.adobe.com/hdr-gain-map/1.0/' {attributes}>{children}\
		 </rdf:Description></rdf:RDF></x:xmpmeta>"
	)
}

fn legacy_iso(metadata: &GainMapMetadata) -> Vec<u8> {
	let denominator = metadata.base_headroom.denominator;
	let channels = &metadata.channels;
	let all_equal = channels.iter().all(|c| {
		[
			metadata.alternate_headroom.denominator,
			c.min.denominator,
			c.max.denominator,
			c.gamma.denominator,
			c.base_offset.denominator,
			c.alternate_offset.denominator,
		] == [denominator; 6]
	});
	assert!(all_equal, "the legacy layout needs one common denominator");

	let mut flags = 0x08;
	if channels.len() == 3 {
		flags |= 0x80;
	}

	if metadata.use_base_colour_space {
		flags |= 0x40;
	}

	let mut out = vec![0, 0, 0, 0, flags];
	for value in [
		denominator,
		metadata.base_headroom.numerator,
		metadata.alternate_headroom.numerator,
	] {
		out.extend_from_slice(&value.to_be_bytes());
	}
	for c in channels {
		out.extend_from_slice(&c.min.numerator.to_be_bytes());
		out.extend_from_slice(&c.max.numerator.to_be_bytes());
		out.extend_from_slice(&c.gamma.numerator.to_be_bytes());
		out.extend_from_slice(&c.base_offset.numerator.to_be_bytes());
		out.extend_from_slice(&c.alternate_offset.numerator.to_be_bytes());
	}
	out
}
