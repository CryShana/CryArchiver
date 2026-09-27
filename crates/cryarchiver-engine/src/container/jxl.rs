use std::io::Read;

use crate::container::isobmff;

const CODESTREAM_START: [u8; 2] = [0xFF, 0x0A];
const SIGNATURE: [u8; 4] = [0x0D, 0x0A, 0x87, 0x0A];
const FILE_TYPE: &[u8] = b"jxl \0\0\0\0jxl ";
const LAST_PART: u32 = 0x8000_0000;
const MAX_METADATA_BYTES: u64 = 64 << 20;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct JxlError(String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Codestream {
	pub bytes: Vec<u8>,
	pub level: Option<u8>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct JxlMetadata<'a> {
	pub exif: Option<&'a [u8]>,
	pub xmp: Option<&'a [u8]>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JxlBoxes {
	pub container: bool,
	pub level: Option<u8>,
	pub exif: Option<Vec<u8>>,
	pub xmp: Option<Vec<u8>>,
}

pub fn extract_codestream(data: &[u8]) -> Result<Codestream, JxlError> {
	if data.starts_with(&CODESTREAM_START) {
		return Ok(Codestream {
			bytes: data.to_vec(),
			level: None,
		});
	}

	let mut bytes = Vec::new();
	let mut level = None;
	let mut next_part = 0u32;
	for found in isobmff::boxes(data).map_err(|err| JxlError(err.to_string()))? {
		match &found.kind {
			b"jxlc" => bytes.extend_from_slice(found.body),
			b"jxlp" => {
				let index = found
					.body
					.get(..4)
					.map(|prefix| u32::from_be_bytes(prefix.try_into().expect("4 bytes")))
					.ok_or_else(|| JxlError("short 'jxlp' box".into()))?;
				if index & !LAST_PART != next_part {
					return Err(JxlError("the codestream parts are out of order".into()));
				}
				next_part += 1;
				bytes.extend_from_slice(&found.body[4..]);
			}
			b"jxll" => level = found.body.first().copied(),
			_ => {}
		}
	}

	if !bytes.starts_with(&CODESTREAM_START) {
		return Err(JxlError("no JPEG XL codestream".into()));
	}
	Ok(Codestream { bytes, level })
}

pub fn write_container(codestream: &Codestream, metadata: &JxlMetadata<'_>) -> Vec<u8> {
	let mut out = Vec::with_capacity(codestream.bytes.len() + 4096);
	put_box(&mut out, *b"JXL ", &SIGNATURE);
	put_box(&mut out, *b"ftyp", FILE_TYPE);
	if let Some(level) = codestream.level {
		put_box(&mut out, *b"jxll", &[level]);
	}
	if let Some(tiff) = metadata.exif {
		put_box(&mut out, *b"Exif", &[&[0u8; 4][..], tiff].concat());
	}
	if let Some(xmp) = metadata.xmp {
		put_box(&mut out, *b"xml ", xmp);
	}
	put_box(&mut out, *b"jxlc", &codestream.bytes);
	out
}

fn put_box(out: &mut Vec<u8>, kind: [u8; 4], payload: &[u8]) {
	let size = payload.len() as u64 + 8;
	if let Ok(small) = u32::try_from(size) {
		out.extend_from_slice(&small.to_be_bytes());
		out.extend_from_slice(&kind);
	} else {
		out.extend_from_slice(&1u32.to_be_bytes());
		out.extend_from_slice(&kind);
		out.extend_from_slice(&(size + 8).to_be_bytes());
	}
	out.extend_from_slice(payload);
}

pub fn read_boxes(data: &[u8]) -> Result<JxlBoxes, JxlError> {
	if data.starts_with(&CODESTREAM_START) {
		return Ok(JxlBoxes::default());
	}

	let mut found = JxlBoxes {
		container: true,
		..JxlBoxes::default()
	};
	for entry in isobmff::boxes(data).map_err(|err| JxlError(err.to_string()))? {
		let (kind, payload) = match &entry.kind {
			b"brob" => {
				let kind: [u8; 4] = entry
					.body
					.get(..4)
					.and_then(|kind| kind.try_into().ok())
					.ok_or_else(|| JxlError("short 'brob' box".into()))?;
				(kind, inflate_brotli(&entry.body[4..])?)
			}
			_ => (entry.kind, entry.body.to_vec()),
		};

		match &kind {
			b"Exif" if found.exif.is_none() => found.exif = Some(exif_tiff(&payload)?),
			b"xml " if found.xmp.is_none() => found.xmp = Some(payload),
			b"jxll" => found.level = payload.first().copied(),
			_ => {}
		}
	}
	Ok(found)
}

fn exif_tiff(payload: &[u8]) -> Result<Vec<u8>, JxlError> {
	let offset = payload
		.get(..4)
		.map(|prefix| u32::from_be_bytes(prefix.try_into().expect("4 bytes")))
		.ok_or_else(|| JxlError("short 'Exif' box".into()))?;
	usize::try_from(offset)
		.ok()
		.and_then(|offset| payload.get(4 + offset..))
		.map(<[u8]>::to_vec)
		.ok_or_else(|| JxlError("the 'Exif' box's TIFF offset is outside the box".into()))
}

fn inflate_brotli(compressed: &[u8]) -> Result<Vec<u8>, JxlError> {
	let mut out = Vec::new();
	brotli_decompressor::Decompressor::new(compressed, 4096)
		.take(MAX_METADATA_BYTES + 1)
		.read_to_end(&mut out)
		.map_err(|err| JxlError(format!("unreadable 'brob' box: {err}")))?;

	if out.len() as u64 > MAX_METADATA_BYTES {
		return Err(JxlError("a 'brob' box inflates past 64 MiB".into()));
	}
	Ok(out)
}
