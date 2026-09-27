use crate::container::isobmff;
use crate::metadata::{ImageFormat, jpeg, sniff};

const PNG_SIGNATURE_LEN: usize = 8;
const GIF_HEADER_LEN: usize = 13;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Structure {
	pub damage: Option<String>,
	pub warnings: Vec<String>,
}

impl Structure {
	fn damaged(reason: impl Into<String>) -> Self {
		Self {
			damage: Some(reason.into()),
			warnings: Vec::new(),
		}
	}
}

pub fn check(bytes: &[u8]) -> Structure {
	match sniff(bytes) {
		ImageFormat::Jpeg => jpeg(bytes),
		ImageFormat::Png => png(bytes),
		ImageFormat::Gif => gif(bytes),
		ImageFormat::Heif | ImageFormat::Avif => heif(bytes),
		_ => Structure::default(),
	}
}

fn jpeg(bytes: &[u8]) -> Structure {
	match jpeg::image_end(bytes) {
		Ok(_) => Structure::default(),
		Err(damage) => Structure::damaged(damage),
	}
}

fn png(bytes: &[u8]) -> Structure {
	let mut at = PNG_SIGNATURE_LEN;
	let mut warnings = Vec::new();

	while at + 12 <= bytes.len() {
		let len = u32::from_be_bytes(bytes[at..at + 4].try_into().expect("4 bytes"));
		let name = String::from_utf8_lossy(&bytes[at + 4..at + 8]).into_owned();
		let Some(end) = usize::try_from(len)
			.ok()
			.and_then(|len| (at + 12).checked_add(len))
			.filter(|end| *end <= bytes.len())
		else {
			return Structure {
				damage: Some(format!("truncated: the {name} chunk runs past the end of the file")),
				warnings,
			};
		};

		let stored = u32::from_be_bytes(bytes[end - 4..end].try_into().expect("4 bytes"));
		if crc32fast::hash(&bytes[at + 4..end - 4]) != stored {
			warnings.push(format!("CRC mismatch in the {name} chunk"));
		}
		if name == "IEND" {
			return Structure { damage: None, warnings };
		}
		at = end;
	}
	Structure {
		damage: Some("truncated: no IEND chunk".into()),
		warnings,
	}
}

fn gif(bytes: &[u8]) -> Structure {
	let Some(&flags) = bytes.get(10) else {
		return Structure::damaged("truncated GIF header");
	};
	let mut at = GIF_HEADER_LEN + colour_table_len(flags);

	loop {
		match bytes.get(at) {
			Some(0x3B) => return Structure::default(),
			Some(0x21) => match skip_sub_blocks(bytes, at + 2) {
				Some(next) => at = next,
				None => return Structure::damaged("truncated inside an extension block"),
			},
			Some(0x2C) => {
				let Some(&local_flags) = bytes.get(at + 9) else {
					return Structure::damaged("truncated image descriptor");
				};
				match skip_sub_blocks(bytes, at + 10 + colour_table_len(local_flags) + 1) {
					Some(next) => at = next,
					None => return Structure::damaged("truncated: image data cut off before the trailer"),
				}
			}
			Some(other) => return Structure::damaged(format!("unexpected GIF block 0x{other:02X}")),
			None => return Structure::damaged("truncated: no GIF trailer"),
		}
	}
}

fn colour_table_len(flags: u8) -> usize {
	if flags & 0x80 == 0 {
		0
	} else {
		3 << ((flags & 0x07) + 1)
	}
}

fn skip_sub_blocks(bytes: &[u8], mut at: usize) -> Option<usize> {
	loop {
		let len = usize::from(*bytes.get(at)?);
		at += 1 + len;
		if len == 0 {
			return Some(at);
		}
		if at > bytes.len() {
			return None;
		}
	}
}

fn heif(bytes: &[u8]) -> Structure {
	let boxes = match isobmff::boxes(bytes) {
		Ok(boxes) => boxes,
		Err(err) => return Structure::damaged(err.to_string()),
	};
	let meta = match isobmff::parse_meta(bytes) {
		Ok(meta) => meta,
		Err(err) => return Structure::damaged(err.to_string()),
	};

	if let Some(problem) = meta.extent_problem(bytes.len() as u64) {
		return Structure::damaged(problem);
	}

	let sequence = isobmff::brands(bytes)
		.is_some_and(|brands| brands.iter().any(|brand| isobmff::SEQUENCE_BRANDS.contains(brand)));
	if sequence && !boxes.iter().any(|found| &found.kind == b"moov") {
		return Structure::damaged("the image sequence has no 'moov' box");
	}
	Structure::default()
}
