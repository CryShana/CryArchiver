use crate::container::isobmff::SEQUENCE_BRANDS;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

const EBML_MAGIC: [u8; 4] = [0x1A, 0x45, 0xDF, 0xA3];
const SEGMENT_ID: [u8; 4] = [0x18, 0x53, 0x80, 0x67];
const DOC_TYPE_ID: [u8; 2] = [0x42, 0x82];
const DOC_TYPES: &[&[u8]] = &[b"matroska", b"webm"];
const MAX_EBML_HEADER: u64 = 4096;
const ISOBMFF_LEADS: &[&[u8; 4]] = &[b"ftyp", b"moov", b"mdat", b"free", b"skip", b"wide", b"pnot"];
const STILL_IMAGE_BRANDS: [[u8; 4]; 6] = [*b"avif", *b"heic", *b"heix", *b"heim", *b"heis", *b"mif1"];
const MAX_FTYP: u64 = 256;

pub fn find_damage(path: &Path) -> io::Result<Option<String>> {
	let mut file = File::open(path)?;
	let len = file.metadata()?.len();

	let mut lead = [0u8; 8];
	if read_up_to(&mut file, &mut lead)? < lead.len() {
		return Ok(None);
	}

	if lead[..4] == EBML_MAGIC {
		return matroska_damage(&mut file, len);
	}
	if ISOBMFF_LEADS.iter().any(|kind| lead[4..] == kind[..]) {
		return isobmff_damage(&mut file, len);
	}
	Ok(None)
}

fn isobmff_damage(file: &mut File, len: u64) -> io::Result<Option<String>> {
	let mut offset = 0;
	let mut has_moov = false;
	let mut has_meta = false;
	let mut brands = Vec::new();

	while len - offset >= 8 {
		file.seek(SeekFrom::Start(offset))?;
		let mut header = [0u8; 16];
		let read = read_up_to(file, &mut header)?;
		let kind = &header[4..8];
		if !kind.iter().all(|byte| byte.is_ascii_graphic() || *byte == b' ') {
			break;
		}

		let name = String::from_utf8_lossy(kind);
		let size = match u32::from_be_bytes([header[0], header[1], header[2], header[3]]) {
			0 => len - offset,
			1 if read == header.len() => u64::from_be_bytes(header[8..16].try_into().expect("8 bytes")),
			1 => return Ok(Some(format!("truncated: the '{name}' box header is cut off"))),
			size => u64::from(size),
		};
		if size < 8 {
			return Ok(Some(format!("invalid size {size} for the '{name}' box")));
		}

		let end = offset.saturating_add(size);
		if end > len {
			return Ok(Some(format!(
				"truncated: the '{name}' box ends {} bytes past the end of the file",
				end - len
			)));
		}

		if kind == b"ftyp" && offset == 0 {
			brands = read_brands(file, size.min(MAX_FTYP))?;
		}
		has_moov |= kind == b"moov";
		has_meta |= kind == b"meta";
		offset = end;
	}

	let brand = |set: &[[u8; 4]]| brands.iter().any(|brand| set.contains(brand));
	if brand(&STILL_IMAGE_BRANDS) && !brand(&SEQUENCE_BRANDS) {
		return Ok((!has_meta).then(|| "no 'meta' box".to_owned()));
	}
	Ok((!has_moov).then(|| "no 'moov' box".to_owned()))
}

fn read_brands(file: &mut File, ftyp_size: u64) -> io::Result<Vec<[u8; 4]>> {
	file.seek(SeekFrom::Start(8))?;
	let mut payload = vec![0u8; usize::try_from(ftyp_size.saturating_sub(8)).expect("at most MAX_FTYP")];
	let read = read_up_to(file, &mut payload)?;
	payload.truncate(read);

	let mut brands: Vec<[u8; 4]> = payload
		.get(..4)
		.and_then(|major| major.try_into().ok())
		.into_iter()
		.collect();
	brands.extend(payload.get(8..).unwrap_or_default().as_chunks::<4>().0.iter().copied());
	Ok(brands)
}

fn matroska_damage(file: &mut File, len: u64) -> io::Result<Option<String>> {
	file.seek(SeekFrom::Start(4))?;
	let header_len = match read_vint(file)? {
		Some((header_len, false)) if header_len <= MAX_EBML_HEADER => header_len,
		_ => return Ok(Some("invalid EBML header".into())),
	};

	let mut header = vec![0u8; usize::try_from(header_len).expect("at most MAX_EBML_HEADER")];
	if read_up_to(file, &mut header)? < header.len() || !has_known_doc_type(&header) {
		return Ok(Some("invalid EBML header".into()));
	}

	let mut id = [0u8; 4];
	if read_up_to(file, &mut id)? < id.len() || id != SEGMENT_ID {
		return Ok(Some("no Segment after the EBML header".into()));
	}

	let Some((segment_len, unknown)) = read_vint(file)? else {
		return Ok(Some("invalid Segment size".into()));
	};
	let end = file.stream_position()?.saturating_add(segment_len);
	if !unknown && end > len {
		return Ok(Some(format!(
			"truncated: the Segment ends {} bytes past the end of the file",
			end - len
		)));
	}
	Ok(None)
}

fn has_known_doc_type(header: &[u8]) -> bool {
	header.windows(DOC_TYPE_ID.len()).enumerate().any(|(at, window)| {
		let Some(&size) = header.get(at + 2).filter(|_| window == DOC_TYPE_ID) else {
			return false;
		};
		let len = usize::from(size & 0x7F);
		let value = header.get(at + 3..at + 3 + len).unwrap_or_default();
		size & 0x80 != 0 && DOC_TYPES.contains(&value)
	})
}

fn read_vint(file: &mut File) -> io::Result<Option<(u64, bool)>> {
	let mut first = [0u8; 1];
	if read_up_to(file, &mut first)? == 0 || first[0] == 0 {
		return Ok(None);
	}

	let extra = first[0].leading_zeros() as usize;
	let mut rest = [0u8; 7];
	if read_up_to(file, &mut rest[..extra])? < extra {
		return Ok(None);
	}

	let marker_mask = 0xFFu8
		.checked_shr(u32::try_from(extra + 1).expect("at most 8"))
		.unwrap_or(0);
	let mut value = u64::from(first[0] & marker_mask);
	let mut all_ones = first[0] & marker_mask == marker_mask;
	for &byte in &rest[..extra] {
		value = (value << 8) | u64::from(byte);
		all_ones &= byte == 0xFF;
	}
	Ok(Some((value, all_ones)))
}

fn read_up_to(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
	let mut filled = 0;
	while filled < buf.len() {
		match file.read(&mut buf[filled..])? {
			0 => break,
			n => filled += n,
		}
	}
	Ok(filled)
}
