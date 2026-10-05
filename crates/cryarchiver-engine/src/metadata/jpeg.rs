use std::collections::BTreeMap;

use crate::metadata::{AuxKind, SourceMetadata, auxiliary, gain_map, xmp};

const EXIF: &[u8] = b"Exif\0\0";
pub(super) const XMP: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
const EXTENDED_XMP: &[u8] = b"http://ns.adobe.com/xmp/extension/\0";
const MAX_EXTENDED_XMP: usize = 64 << 20;
const ICC: &[u8] = b"ICC_PROFILE\0";
const MPF: &[u8] = b"MPF\0";
pub(super) const ISO_GAIN_MAP: &[u8] = b"urn:iso:std:iso:ts:21496:-1\0";
const PHOTOSHOP: &[u8] = b"Photoshop 3.0\0";
const ADOBE: &[u8] = b"Adobe";
const APP1: u8 = 0xE1;
const APP2: u8 = 0xE2;
const APP13: u8 = 0xED;
const APP14: u8 = 0xEE;
const COMMENT: u8 = 0xFE;
const SAMSUNG_MOTION_PHOTO: &[u8] = b"MotionPhoto_Data";
const MIN_TRAILING_CONTENT: usize = 1024;

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
	mpf_at: usize,
	iso_gain_map: bool,
}

pub(super) struct Segment<'a> {
	pub marker: u8,
	pub at: usize,
	pub payload: &'a [u8],
}

/// The marker segments before the first scan. `cut_off` names the marker whose segment ran past the end.
pub(super) struct SegmentWalk<'a> {
	bytes: &'a [u8],
	at: usize,
	pub cut_off: Option<u8>,
}

impl<'a> SegmentWalk<'a> {
	pub(super) fn new(bytes: &'a [u8]) -> Self {
		Self {
			bytes,
			at: 2,
			cut_off: None,
		}
	}
}

impl<'a> Iterator for SegmentWalk<'a> {
	type Item = Segment<'a>;

	fn next(&mut self) -> Option<Segment<'a>> {
		while let Some(&[0xFF, marker]) = self.bytes.get(self.at..self.at + 2) {
			match marker {
				0xFF => {
					self.at += 1;
					continue;
				}
				0x01 | 0xD0..=0xD8 => {
					self.at += 2;
					continue;
				}
				0xD9 | 0xDA => return None,
				_ => {}
			}

			let at = self.at;
			let len = self
				.bytes
				.get(at + 2..at + 4)
				.map(|len| usize::from(u16::from_be_bytes([len[0], len[1]])))?;
			let Some(payload) = self.bytes.get(at + 4..(at + 2 + len).max(at + 4)) else {
				self.cut_off = Some(marker);
				return None;
			};

			self.at += 2 + len.max(2);
			return Some(Segment { marker, at, payload });
		}
		None
	}
}

pub(super) fn extract(bytes: &[u8]) -> SourceMetadata {
	let mut metadata = SourceMetadata::default();
	let mut segments = Segments::default();

	let mut walk = SegmentWalk::new(bytes);
	for Segment { marker, at, payload } in walk.by_ref() {
		if marker == APP2 && payload.starts_with(MPF) {
			segments.mpf_at = at + 4 + MPF.len();
		}
		read_segment(marker, payload, &mut metadata, &mut segments);
	}
	if let Some(marker) = walk.cut_off {
		metadata.warnings.push(format!("the FF{marker:02X} segment is cut off"));
	}

	let mpf = segments.mpf.map(|mpf| (mpf, segments.mpf_at));
	let trailer = image_end(bytes).ok().and_then(|end| trailing_content(bytes, end, mpf));
	if let Some(trailer) = &trailer {
		metadata.aux.push(trailer.clone());
	}

	finish(&mut metadata, segments);
	read_gain_map(bytes, mpf, trailer, &mut metadata);
	metadata
}

fn read_gain_map(bytes: &[u8], mpf: Option<(&[u8], usize)>, trailer: Option<AuxKind>, metadata: &mut SourceMetadata) {
	match gain_map::find(bytes, mpf) {
		Some(Ok(found)) => {
			metadata.gain_map_mpf_entry =
				mpf.and_then(|(mpf, mpf_at)| auxiliary::mpf_entry_at(mpf, found.jpeg.start.checked_sub(mpf_at)?));

			if listed_images_end(mpf).is_none_or(|end| end < found.jpeg.end) {
				metadata.gain_map_trailer =
					trailer.map(|trailer| (trailer, trailing_content(bytes, found.jpeg.end, mpf)));
			}
			metadata.gain_map = Some(found);
		}
		Some(Err(reason)) => metadata
			.warnings
			.push(format!("the HDR gain map can't be read: {reason}")),
		None => {}
	}
}

pub fn image_end(bytes: &[u8]) -> Result<usize, String> {
	let mut at = 2;
	let mut scanned = false;

	loop {
		let Some(&[0xFF, marker]) = bytes.get(at..at + 2) else {
			return Err(if scanned {
				"truncated: no end-of-image marker after the last scan".into()
			} else {
				"truncated before the first scan".into()
			});
		};
		match marker {
			0xFF => at += 1,
			0xD9 => return Ok(at + 2),
			0x01 | 0xD0..=0xD7 => at += 2,
			_ => {
				let Some(len) = bytes
					.get(at + 2..at + 4)
					.map(|len| usize::from(u16::from_be_bytes([len[0], len[1]])))
				else {
					return Err("truncated inside a marker segment".into());
				};
				if len < 2 || at + 2 + len > bytes.len() {
					return Err(format!("the FF{marker:02X} segment runs past the end of the file"));
				}
				at += 2 + len;
				if marker == 0xDA {
					scanned = true;
					at = skip_entropy_data(bytes, at);
				}
			}
		}
	}
}

fn skip_entropy_data(bytes: &[u8], mut at: usize) -> usize {
	while at + 1 < bytes.len() {
		let next = bytes[at + 1];
		if bytes[at] == 0xFF && next != 0x00 && next != 0xFF && !(0xD0..=0xD7).contains(&next) {
			return at;
		}
		at += 1;
	}
	bytes.len()
}

fn listed_images_end(mpf: Option<(&[u8], usize)>) -> Option<usize> {
	let (mpf, mpf_at) = mpf?;
	auxiliary::mpf_images_end(mpf).map(|end| mpf_at.saturating_add(end))
}

fn trailing_content(bytes: &[u8], image_end: usize, mpf: Option<(&[u8], usize)>) -> Option<AuxKind> {
	let after_image = bytes.get(image_end..)?;
	if after_image
		.windows(SAMSUNG_MOTION_PHOTO.len())
		.any(|window| window == SAMSUNG_MOTION_PHOTO)
	{
		return Some(AuxKind::MotionVideo("Samsung motion photo".into()));
	}

	let mpf_end = listed_images_end(mpf);
	let unaccounted = bytes.get(mpf_end.map_or(image_end, |end| end.max(image_end))..)?;
	let padding = unaccounted.iter().all(|&byte| byte == 0x00 || byte == 0xFF);
	if unaccounted.len() < MIN_TRAILING_CONTENT || padding {
		return None;
	}

	let mp4 = unaccounted.get(4..8) == Some(b"ftyp");
	Some(if mp4 {
		AuxKind::MotionVideo("an MP4 video after the image".into())
	} else {
		AuxKind::Other(format!("{} bytes after the image", unaccounted.len()))
	})
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
