pub mod auxiliary;
pub mod exif;
mod gif;
pub mod icc;
pub mod iptc;
mod jpeg;
mod png;
mod riff;
pub mod xmp;

use std::fmt;

use crate::container::{isobmff, jxl};
use crate::metadata::icc::Cicp;

const TIFF_XMP: u16 = 700;
const TIFF_IPTC: u16 = 33723;
const TIFF_PHOTOSHOP: u16 = 34377;
const TIFF_ICC: u16 = 34675;
const AVIF_BRANDS: &[[u8; 4]] = &[*b"avif", *b"avis"];
const HEIF_BRANDS: &[[u8; 4]] = &[*b"heic", *b"heix", *b"heim", *b"heis", *b"mif1", *b"msf1"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
	Jpeg,
	Png,
	Gif,
	WebP,
	Tiff,
	Heif,
	Avif,
	Jxl,
	Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuxKind {
	GainMap(String),
	Depth(String),
	Other(String),
}

impl fmt::Display for AuxKind {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::GainMap(detail) => write!(f, "gain map ({detail})"),
			Self::Depth(detail) => write!(f, "depth map ({detail})"),
			Self::Other(detail) => write!(f, "auxiliary image ({detail})"),
		}
	}
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceMetadata {
	pub exif: Option<Vec<u8>>,
	pub xmp: Option<Vec<u8>>,
	pub xmp_extended: Vec<String>,
	pub icc: Option<Vec<u8>>,
	pub icc_problem: Option<String>,
	pub cicp: Option<Cicp>,
	pub irb: Option<Vec<u8>>,
	pub iim: Option<Vec<u8>>,
	pub comments: Vec<String>,
	pub text: Vec<(String, String)>,
	pub jpeg_components: Option<u8>,
	pub adobe_transform: Option<u8>,
	pub aux: Vec<AuxKind>,
	pub warnings: Vec<String>,
}

pub fn sniff(bytes: &[u8]) -> ImageFormat {
	match bytes {
		[0xFF, 0xD8, 0xFF, ..] => ImageFormat::Jpeg,
		[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n', ..] => ImageFormat::Png,
		[b'G', b'I', b'F', b'8', b'7' | b'9', b'a', ..] => ImageFormat::Gif,
		[b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => ImageFormat::WebP,
		[b'I', b'I', 42, 0, ..] | [b'M', b'M', 0, 42, ..] => ImageFormat::Tiff,
		[0xFF, 0x0A, ..] | [0, 0, 0, 0x0C, b'J', b'X', b'L', b' ', ..] => ImageFormat::Jxl,
		_ => match isobmff::brands(bytes) {
			Some(brands) if brands.iter().any(|brand| AVIF_BRANDS.contains(brand)) => ImageFormat::Avif,
			Some(brands) if brands.iter().any(|brand| HEIF_BRANDS.contains(brand)) => ImageFormat::Heif,
			_ => ImageFormat::Other,
		},
	}
}

pub fn extract(bytes: &[u8]) -> SourceMetadata {
	let mut metadata = match sniff(bytes) {
		ImageFormat::Jpeg => jpeg::extract(bytes),
		ImageFormat::Png => png::extract(bytes),
		ImageFormat::Gif => gif::extract(bytes),
		ImageFormat::WebP => riff::extract(bytes),
		ImageFormat::Tiff => tiff_source(bytes),
		ImageFormat::Heif | ImageFormat::Avif => heif_source(bytes),
		ImageFormat::Jxl => jxl_source(bytes),
		ImageFormat::Other => SourceMetadata::default(),
	};

	if metadata.icc_problem.is_none() {
		metadata.icc_problem = metadata.icc.as_deref().and_then(icc::problem);
	}
	metadata
}

fn tiff_source(bytes: &[u8]) -> SourceMetadata {
	let mut metadata = SourceMetadata {
		xmp: exif::ifd0_value(bytes, TIFF_XMP),
		icc: exif::ifd0_value(bytes, TIFF_ICC),
		iim: exif::ifd0_value(bytes, TIFF_IPTC),
		irb: exif::ifd0_value(bytes, TIFF_PHOTOSHOP),
		..SourceMetadata::default()
	};
	match exif::from_tiff_file(bytes) {
		Ok(block) => metadata.exif = Some(block),
		Err(err) => metadata.warnings.push(format!("EXIF not copied: {err}")),
	}
	metadata
}

fn heif_source(bytes: &[u8]) -> SourceMetadata {
	let mut metadata = SourceMetadata::default();
	let meta = match isobmff::parse_meta(bytes) {
		Ok(meta) => meta,
		Err(err) => {
			metadata.warnings.push(format!("metadata not read: {err}"));
			return metadata;
		}
	};

	match meta.exif_tiff(bytes).transpose() {
		Ok(exif) => metadata.exif = exif,
		Err(err) => metadata.warnings.push(format!("EXIF not copied: {err}")),
	}
	match meta.xmp(bytes).transpose() {
		Ok(xmp) => metadata.xmp = xmp,
		Err(err) => metadata.warnings.push(format!("XMP not copied: {err}")),
	}
	metadata.icc = meta.primary_icc().map(<[u8]>::to_vec);
	metadata.cicp = meta.primary_nclx().and_then(|nclx| {
		Some(Cicp {
			primaries: u8::try_from(nclx.primaries).ok()?,
			transfer: u8::try_from(nclx.transfer).ok()?,
		})
	});
	metadata.aux = auxiliary::from_heif(&meta);
	metadata
}

fn jxl_source(bytes: &[u8]) -> SourceMetadata {
	let mut metadata = SourceMetadata::default();
	match jxl::read_boxes(bytes) {
		Ok(boxes) => {
			metadata.exif = boxes.exif;
			metadata.xmp = boxes.xmp;
		}
		Err(err) => metadata.warnings.push(format!("metadata not read: {err}")),
	}
	metadata
}
