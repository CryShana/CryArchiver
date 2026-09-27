use std::collections::BTreeMap;

use crate::metadata::{SourceMetadata, auxiliary, xmp};

const EXIF: &[u8] = b"Exif\0\0";
const XMP: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
const EXTENDED_XMP: &[u8] = b"http://ns.adobe.com/xmp/extension/\0";
const MAX_EXTENDED_XMP: usize = 64 << 20;
const ICC: &[u8] = b"ICC_PROFILE\0";
const MPF: &[u8] = b"MPF\0";
const ISO_GAIN_MAP: &[u8] = b"urn:iso:std:iso:ts:21496:-1\0";
const PHOTOSHOP: &[u8] = b"Photoshop 3.0\0";
const ADOBE: &[u8] = b"Adobe";
const APP1: u8 = 0xE1;
const APP2: u8 = 0xE2;
const APP13: u8 = 0xED;
const APP14: u8 = 0xEE;
const COMMENT: u8 = 0xFE;

struct ExtendedPart<'a> {
	full: u32,
	offset: u32,
	data: &'a [u8],
}

#[derive(Default)]
struct Segments<'a> {
	icc_chunks: Vec<(u8, u8, &'a [u8])>,
	extended: BTreeMap<String, Vec<ExtendedPart<'a>>>,
	irb: Vec<u8>,
	mpf: Option<&'a [u8]>,
	iso_gain_map: bool,
}

pub(super) fn extract(bytes: &[u8]) -> SourceMetadata {
	let mut metadata = SourceMetadata::default();
	let mut segments = Segments::default();
	let mut at = 2;

	while let Some(&[0xFF, marker]) = bytes.get(at..at + 2) {
		match marker {
			0xFF => {
				at += 1;
				continue;
			}
			0x01 | 0xD0..=0xD8 => {
				at += 2;
				continue;
			}
			0xD9 | 0xDA => break,
			_ => {}
		}

		let Some(len) = bytes
			.get(at + 2..at + 4)
			.map(|len| usize::from(u16::from_be_bytes([len[0], len[1]])))
		else {
			break;
		};
		let Some(payload) = bytes.get(at + 4..(at + 2 + len).max(at + 4)) else {
			metadata.warnings.push(format!("the FF{marker:02X} segment is cut off"));
			break;
		};
		read_segment(marker, payload, &mut metadata, &mut segments);
		at += 2 + len.max(2);
	}

	finish(&mut metadata, segments);
	metadata
}

fn read_segment<'a>(marker: u8, payload: &'a [u8], metadata: &mut SourceMetadata, segments: &mut Segments<'a>) {
	match marker {
		APP1 if payload.starts_with(EXIF) && metadata.exif.is_none() => {
			metadata.exif = Some(payload[EXIF.len()..].to_vec());
		}
		APP1 if payload.starts_with(XMP) && metadata.xmp.is_none() => {
			metadata.xmp = Some(payload[XMP.len()..].to_vec());
		}
		APP1 if payload.starts_with(EXTENDED_XMP) => {
			let part = &payload[EXTENDED_XMP.len()..];
			if part.len() < 40 {
				metadata
					.warnings
					.push("a short extended XMP segment was ignored".into());
				return;
			}
			let guid = String::from_utf8_lossy(&part[..32]).into_owned();
			let full = u32::from_be_bytes(part[32..36].try_into().expect("4 bytes"));
			let offset = u32::from_be_bytes(part[36..40].try_into().expect("4 bytes"));
			segments.extended.entry(guid).or_default().push(ExtendedPart {
				full,
				offset,
				data: &part[40..],
			});
		}
		APP2 if payload.starts_with(ICC) => match payload.get(ICC.len()..ICC.len() + 2) {
			Some(&[sequence, count]) => segments.icc_chunks.push((sequence, count, &payload[ICC.len() + 2..])),
			_ => metadata.icc_problem = Some("an ICC segment has no sequence number".into()),
		},
		APP2 if payload.starts_with(MPF) => segments.mpf = Some(&payload[MPF.len()..]),
		APP2 if payload.starts_with(ISO_GAIN_MAP) => segments.iso_gain_map = true,
		APP13 if payload.starts_with(PHOTOSHOP) => segments.irb.extend_from_slice(&payload[PHOTOSHOP.len()..]),
		APP14 if payload.starts_with(ADOBE) && payload.len() >= 12 => metadata.adobe_transform = Some(payload[11]),
		COMMENT => metadata
			.comments
			.push(String::from_utf8_lossy(payload).trim_end_matches('\0').to_owned()),
		0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF if payload.len() >= 6 => {
			metadata.jpeg_components = Some(payload[5]);
		}
		_ => {}
	}
}

fn finish(metadata: &mut SourceMetadata, segments: Segments<'_>) {
	if !segments.icc_chunks.is_empty() {
		match reassemble_icc(segments.icc_chunks) {
			Ok(icc) => metadata.icc = Some(icc),
			Err(problem) => metadata.icc_problem = Some(problem),
		}
	}
	if !segments.irb.is_empty() {
		metadata.irb = Some(segments.irb);
	}

	let main_xmp = metadata.xmp.as_deref().and_then(|xmp| std::str::from_utf8(xmp).ok());
	let wanted = main_xmp
		.map(|xmp| xmp::values(xmp, xmp::XMP_NOTE, "HasExtendedXMP"))
		.unwrap_or_default();
	for (guid, parts) in segments.extended {
		if !wanted.contains(&guid) {
			metadata
				.warnings
				.push(format!("extended XMP {guid} is not referenced and was dropped"));
			continue;
		}
		match reassemble_extended(&parts) {
			Some(packet) => metadata.xmp_extended.push(packet),
			None => metadata
				.warnings
				.push(format!("extended XMP {guid} is incomplete and was dropped")),
		}
	}

	if let Some(mpf) = segments.mpf {
		match auxiliary::from_mpf(mpf) {
			Ok(found) => metadata.aux.extend(found),
			Err(err) => metadata.warnings.push(format!("MPF index not read: {err}")),
		}
	}
	if let Some(xmp) = main_xmp {
		metadata.aux.extend(auxiliary::from_xmp(xmp));
	}
	if segments.iso_gain_map {
		metadata
			.aux
			.push(crate::metadata::AuxKind::GainMap("ISO 21496-1 metadata".into()));
	}
}

fn reassemble_icc(mut chunks: Vec<(u8, u8, &[u8])>) -> Result<Vec<u8>, String> {
	let count = chunks[0].1;
	chunks.sort_by_key(|(sequence, _, _)| *sequence);

	if chunks.iter().any(|(_, total, _)| *total != count) || chunks.len() != usize::from(count) {
		return Err(format!("the ICC profile has {} chunks but says {count}", chunks.len()));
	}
	if chunks
		.iter()
		.enumerate()
		.any(|(index, (sequence, _, _))| usize::from(*sequence) != index + 1)
	{
		return Err("the ICC chunks are not numbered 1 to N".into());
	}
	Ok(chunks.iter().flat_map(|(_, _, data)| data.iter().copied()).collect())
}

fn reassemble_extended(parts: &[ExtendedPart<'_>]) -> Option<String> {
	let full = usize::try_from(parts.first()?.full).ok()?;
	let supplied: usize = parts.iter().map(|part| part.data.len()).sum();
	if full > MAX_EXTENDED_XMP || supplied != full {
		return None;
	}

	let mut packet = vec![0u8; full];
	for part in parts {
		let offset = usize::try_from(part.offset).ok()?;
		let end = offset.checked_add(part.data.len())?;
		if usize::try_from(part.full).ok()? != full || end > full {
			return None;
		}
		packet[offset..end].copy_from_slice(part.data);
	}

	String::from_utf8(packet).ok()
}
