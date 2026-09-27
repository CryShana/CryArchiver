pub mod exif;
pub mod icc;
pub mod iptc;
pub mod xmp;

use crate::container::isobmff;

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
