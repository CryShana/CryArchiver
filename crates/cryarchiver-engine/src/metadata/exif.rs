use std::collections::HashSet;
use std::ops::Range;

const TAG_NEW_SUBFILE_TYPE: u16 = 0x00FE;
const TAG_IMAGE_WIDTH: u16 = 0x0100;
const TAG_IMAGE_LENGTH: u16 = 0x0101;
const TAG_ORIENTATION: u16 = 0x0112;
const TAG_EXIF_IFD: u16 = 0x8769;
const TAG_GPS_IFD: u16 = 0x8825;
const TAG_INTEROP_IFD: u16 = 0xA005;
const TAG_COLOR_SPACE: u16 = 0xA001;
const TAG_PIXEL_X: u16 = 0xA002;
const TAG_PIXEL_Y: u16 = 0xA003;
const TAG_INTEROP_INDEX: u16 = 0x0001;
const TYPE_BYTE: u16 = 1;
const TYPE_SHORT: u16 = 3;
const TYPE_LONG: u16 = 4;
const TYPE_SSHORT: u16 = 8;
const TYPE_SLONG: u16 = 9;
const TYPE_SIZES: [usize; 14] = [0, 1, 1, 2, 4, 8, 1, 1, 2, 4, 8, 4, 8, 4];
const UNCALIBRATED: u16 = 0xFFFF;
const MAX_IFDS: usize = 16;
const MAX_PAGES: usize = 1024;
const TIFF_METADATA_TAGS: &[u16] = &[
	0x010D, 0x010E, 0x010F, 0x0110, 0x0112, 0x011A, 0x011B, 0x011D, 0x0128, 0x0131, 0x0132, 0x013B, 0x013C, 0x4746,
	0x4749, 0x8298, 0x9C9B, 0x9C9C, 0x9C9D, 0x9C9E, 0x9C9F,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endian {
	Little,
	Big,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExifError {
	#[error("not a TIFF header")]
	BadHeader,
	#[error("{ifd} offset {offset} is outside the {len}-byte block")]
	IfdOutOfBounds { ifd: &'static str, offset: u32, len: usize },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExifPatch {
	pub orientation: Option<u16>,
	pub patched: Vec<(u16, u32, u32)>,
	pub skipped: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
enum Value {
	Byte(u8),
	Short(u16),
	Long(u32),
}

#[derive(Clone, Copy)]
struct Tiff<'a> {
	data: &'a [u8],
	endian: Endian,
}

impl<'a> Tiff<'a> {
	fn new(data: &'a [u8]) -> Option<Self> {
		endian(data).map(|endian| Self { data, endian })
	}

	fn u16_at(self, at: usize) -> Option<u16> {
		let bytes: [u8; 2] = self.data.get(at..at + 2)?.try_into().ok()?;
		Some(match self.endian {
			Endian::Little => u16::from_le_bytes(bytes),
			Endian::Big => u16::from_be_bytes(bytes),
		})
	}

	fn u32_at(self, at: usize) -> Option<u32> {
		let bytes: [u8; 4] = self.data.get(at..at + 4)?.try_into().ok()?;
		Some(match self.endian {
			Endian::Little => u32::from_le_bytes(bytes),
			Endian::Big => u32::from_be_bytes(bytes),
		})
	}

	fn first_ifd(self) -> u32 {
		self.u32_at(4).unwrap_or(0)
	}

	fn entries(self, ifd: &'static str, offset: u32) -> Result<Range<usize>, ExifError> {
		let out_of_bounds = || ExifError::IfdOutOfBounds {
			ifd,
			offset,
			len: self.data.len(),
		};
		let start = usize::try_from(offset).map_err(|_| out_of_bounds())?;
		if start < 8 {
			return Err(out_of_bounds());
		}

		let count = usize::from(self.u16_at(start).ok_or_else(out_of_bounds)?);
		let end = start + 2 + count * 12;
		if end + 4 > self.data.len() {
			return Err(out_of_bounds());
		}
		Ok(start + 2..end)
	}

	fn tag(self, entry: usize) -> u16 {
		self.u16_at(entry).unwrap_or(0)
	}

	fn kind(self, entry: usize) -> u16 {
		self.u16_at(entry + 2).unwrap_or(0)
	}

	fn count(self, entry: usize) -> u32 {
		self.u32_at(entry + 4).unwrap_or(0)
	}

	fn find(self, range: Range<usize>, tag: u16) -> Option<usize> {
		range.step_by(12).find(|&entry| self.tag(entry) == tag)
	}

	fn short(self, entry: usize) -> Option<u16> {
		(self.kind(entry) == TYPE_SHORT && self.count(entry) == 1)
			.then(|| self.u16_at(entry + 8))
			.flatten()
	}

	fn integer(self, entry: usize) -> Option<i64> {
		if self.count(entry) != 1 {
			return None;
		}

		let at = entry + 8;
		match self.kind(entry) {
			TYPE_BYTE => self.data.get(at).copied().map(i64::from),
			TYPE_SHORT => self.u16_at(at).map(i64::from),
			TYPE_LONG => self.u32_at(at).map(i64::from),
			TYPE_SSHORT => self.u16_at(at).map(|value| i64::from(value.cast_signed())),
			TYPE_SLONG => self.u32_at(at).map(|value| i64::from(value.cast_signed())),
			_ => None,
		}
	}

	fn pointer(self, range: Range<usize>, tag: u16) -> Option<u32> {
		self.find(range, tag).and_then(|entry| self.u32_at(entry + 8))
	}
}

pub fn endian(tiff: &[u8]) -> Option<Endian> {
	match tiff.get(..4)? {
		[b'I', b'I', 42, 0] => Some(Endian::Little),
		[b'M', b'M', 0, 42] => Some(Endian::Big),
		_ => None,
	}
}

pub fn orientation(tiff: &[u8]) -> Option<u16> {
	let reader = Tiff::new(tiff)?;
	let ifd0 = reader.entries("IFD0", reader.first_ifd()).ok()?;
	reader
		.find(ifd0, TAG_ORIENTATION)
		.and_then(|entry| reader.integer(entry))
		.map(known_orientation)
}

fn known_orientation(value: i64) -> u16 {
	u16::try_from(value)
		.ok()
		.filter(|orientation| (1..=8).contains(orientation))
		.unwrap_or(1)
}

fn upright(kind: u16) -> Value {
	match kind {
		TYPE_BYTE => Value::Byte(1),
		TYPE_LONG | TYPE_SLONG => Value::Long(1),
		_ => Value::Short(1),
	}
}

pub fn problem(tiff: &[u8]) -> Option<String> {
	let Some(reader) = Tiff::new(tiff) else {
		return Some("the EXIF block has no TIFF header".to_owned());
	};

	let mut pending = vec![("IFD0", reader.first_ifd())];
	let mut visited = 0;
	while let Some((name, offset)) = pending.pop() {
		visited += 1;
		if visited > MAX_IFDS {
			return Some("the EXIF block links more IFDs than it can hold".to_owned());
		}

		let entries = match reader.entries(name, offset) {
			Ok(entries) => entries,
			Err(err) => return Some(err.to_string()),
		};
		for (tag, child) in [
			(TAG_EXIF_IFD, "ExifIFD"),
			(TAG_GPS_IFD, "GPS"),
			(TAG_INTEROP_IFD, "Interop"),
		] {
			if let Some(pointer) = reader.pointer(entries.clone(), tag) {
				pending.push((child, pointer));
			}
		}
		if name == "IFD0" {
			match reader.u32_at(entries.end) {
				Some(0) | None => {}
				Some(next) => pending.push(("IFD1", next)),
			}
		}
	}
	None
}

pub fn patch(tiff: &mut [u8], width: u32, height: u32) -> Result<ExifPatch, ExifError> {
	let reader = Tiff::new(tiff).ok_or(ExifError::BadHeader)?;
	let endian = reader.endian;
	let ifd0 = reader.entries("IFD0", reader.first_ifd())?;

	let mut dimensions = Vec::new();
	for entry in ifd0.clone().step_by(12) {
		match reader.tag(entry) {
			TAG_IMAGE_WIDTH => dimensions.push((entry, width)),
			TAG_IMAGE_LENGTH => dimensions.push((entry, height)),
			_ => {}
		}
	}
	if let Some(offset) = reader.pointer(ifd0.clone(), TAG_EXIF_IFD) {
		for entry in reader.entries("ExifIFD", offset)?.step_by(12) {
			match reader.tag(entry) {
				TAG_PIXEL_X => dimensions.push((entry, width)),
				TAG_PIXEL_Y => dimensions.push((entry, height)),
				_ => {}
			}
		}
	}

	let mut result = ExifPatch::default();
	let mut writes = Vec::new();
	if let Some(entry) = reader.find(ifd0, TAG_ORIENTATION) {
		match reader.integer(entry) {
			Some(value) => {
				result.orientation = Some(known_orientation(value));
				writes.push((entry + 8, upright(reader.kind(entry))));
			}
			None => result.skipped.push("Orientation is not a single number".to_owned()),
		}
	}
	for (entry, value) in dimensions {
		if let Some(write) = dimension_write(reader, entry, value, &mut result) {
			writes.push(write);
		}
	}

	for (at, value) in writes {
		write(tiff, endian, at, value);
	}
	Ok(result)
}

fn dimension_write(reader: Tiff<'_>, entry: usize, value: u32, result: &mut ExifPatch) -> Option<(usize, Value)> {
	let tag = reader.tag(entry);
	let count = reader.count(entry);
	if count != 1 {
		result.skipped.push(format!("tag 0x{tag:04X} has {count} values"));
		return None;
	}

	match reader.kind(entry) {
		TYPE_SHORT => {
			let Ok(short) = u16::try_from(value) else {
				result
					.skipped
					.push(format!("tag 0x{tag:04X}: {value} does not fit a SHORT"));
				return None;
			};
			let old = u32::from(reader.u16_at(entry + 8)?);
			result.patched.push((tag, old, value));
			Some((entry + 8, Value::Short(short)))
		}
		TYPE_LONG => {
			result.patched.push((tag, reader.u32_at(entry + 8)?, value));
			Some((entry + 8, Value::Long(value)))
		}
		kind => {
			result.skipped.push(format!("tag 0x{tag:04X} has type {kind}"));
			None
		}
	}
}

fn write(tiff: &mut [u8], endian: Endian, at: usize, value: Value) {
	match (value, endian) {
		(Value::Byte(byte), _) => tiff[at] = byte,
		(Value::Short(short), Endian::Little) => tiff[at..at + 2].copy_from_slice(&short.to_le_bytes()),
		(Value::Short(short), Endian::Big) => tiff[at..at + 2].copy_from_slice(&short.to_be_bytes()),
		(Value::Long(long), Endian::Little) => tiff[at..at + 4].copy_from_slice(&long.to_le_bytes()),
		(Value::Long(long), Endian::Big) => tiff[at..at + 4].copy_from_slice(&long.to_be_bytes()),
	}
}

pub fn adobe_rgb_only(tiff: &[u8]) -> bool {
	let Some(reader) = Tiff::new(tiff) else {
		return false;
	};
	let Some(exif) = reader
		.entries("IFD0", reader.first_ifd())
		.ok()
		.and_then(|ifd0| reader.pointer(ifd0, TAG_EXIF_IFD))
		.and_then(|offset| reader.entries("ExifIFD", offset).ok())
	else {
		return false;
	};

	let uncalibrated = reader
		.find(exif.clone(), TAG_COLOR_SPACE)
		.and_then(|entry| reader.short(entry))
		== Some(UNCALIBRATED);
	let r03 = reader
		.pointer(exif, TAG_INTEROP_IFD)
		.and_then(|offset| reader.entries("Interop", offset).ok())
		.and_then(|interop| reader.find(interop, TAG_INTEROP_INDEX))
		.and_then(|entry| tiff.get(entry + 8..entry + 11))
		== Some(b"R03".as_slice());
	uncalibrated && r03
}

pub fn tiff_pages(file: &[u8]) -> Option<u32> {
	let reader = Tiff::new(file)?;
	let mut offset = reader.first_ifd();
	let mut seen = HashSet::new();
	let mut pages = 0;

	while offset != 0 && seen.insert(offset) && seen.len() <= MAX_PAGES {
		let entries = reader.entries("IFD", offset).ok()?;
		let reduced = reader
			.find(entries.clone(), TAG_NEW_SUBFILE_TYPE)
			.and_then(|entry| reader.u32_at(entry + 8))
			.is_some_and(|kind| kind & 1 == 1);
		if !reduced {
			pages += 1;
		}
		offset = reader.u32_at(entries.end).unwrap_or(0);
	}
	Some(pages)
}

pub fn ifd0_value(file: &[u8], tag: u16) -> Option<Vec<u8>> {
	let reader = Tiff::new(file)?;
	read_ifd(reader, "IFD0", reader.first_ifd())
		.ok()?
		.into_iter()
		.find(|entry| entry.tag == tag)
		.map(|entry| entry.value)
}

#[derive(Clone)]
struct RawEntry {
	tag: u16,
	kind: u16,
	count: u32,
	value: Vec<u8>,
}

pub fn from_tiff_file(file: &[u8]) -> Result<Vec<u8>, ExifError> {
	let reader = Tiff::new(file).ok_or(ExifError::BadHeader)?;
	let ifd0 = read_ifd(reader, "IFD0", reader.first_ifd())?;
	let pointer = |entries: &[RawEntry], tag: u16| {
		let entry = entries
			.iter()
			.find(|entry| entry.tag == tag && entry.value.len() == 4)?;
		Tiff {
			data: &entry.value,
			endian: reader.endian,
		}
		.u32_at(0)
	};

	let exif = pointer(&ifd0, TAG_EXIF_IFD)
		.map(|offset| read_ifd(reader, "ExifIFD", offset))
		.transpose()?;
	let gps = pointer(&ifd0, TAG_GPS_IFD)
		.map(|offset| read_ifd(reader, "GPS", offset))
		.transpose()?;
	let interop = exif
		.as_deref()
		.and_then(|exif| pointer(exif, TAG_INTEROP_IFD))
		.map(|offset| read_ifd(reader, "Interop", offset))
		.transpose()?;
	let metadata: Vec<RawEntry> = ifd0
		.into_iter()
		.filter(|entry| TIFF_METADATA_TAGS.contains(&entry.tag))
		.collect();

	let mut block = Block {
		bytes: file[..4].to_vec(),
		endian: reader.endian,
	};
	block.push_u32(8);

	let mut children = Vec::new();
	if exif.is_some() {
		children.push(TAG_EXIF_IFD);
	}
	if gps.is_some() {
		children.push(TAG_GPS_IFD);
	}
	let slots = block.write_ifd(&metadata, &children);

	for (tag, slot) in slots {
		block.point(slot);
		if tag == TAG_EXIF_IFD {
			let entries: Vec<RawEntry> = exif
				.iter()
				.flatten()
				.filter(|entry| entry.tag != TAG_INTEROP_IFD)
				.cloned()
				.collect();
			let interop_child: &[u16] = if interop.is_some() { &[TAG_INTEROP_IFD] } else { &[] };
			for (_, interop_slot) in block.write_ifd(&entries, interop_child) {
				block.point(interop_slot);
				block.write_ifd(interop.as_deref().unwrap_or_default(), &[]);
			}
		} else {
			block.write_ifd(gps.as_deref().unwrap_or_default(), &[]);
		}
	}
	Ok(block.bytes)
}

fn read_ifd(reader: Tiff<'_>, name: &'static str, offset: u32) -> Result<Vec<RawEntry>, ExifError> {
	let mut entries = Vec::new();

	for entry in reader.entries(name, offset)?.step_by(12) {
		let (tag, kind, count) = (reader.tag(entry), reader.kind(entry), reader.count(entry));
		let Some(&size) = TYPE_SIZES.get(usize::from(kind)).filter(|size| **size > 0) else {
			continue;
		};
		let Some(len) = usize::try_from(count).ok().and_then(|count| count.checked_mul(size)) else {
			continue;
		};

		let start = if len <= 4 {
			Some(entry + 8)
		} else {
			reader
				.u32_at(entry + 8)
				.and_then(|pointer| usize::try_from(pointer).ok())
		};
		let Some(value) = start.and_then(|start| reader.data.get(start..start.checked_add(len)?)) else {
			continue;
		};
		entries.push(RawEntry {
			tag,
			kind,
			count,
			value: value.to_vec(),
		});
	}
	Ok(entries)
}

struct Block {
	bytes: Vec<u8>,
	endian: Endian,
}

impl Block {
	fn push_u16(&mut self, value: u16) {
		match self.endian {
			Endian::Little => self.bytes.extend_from_slice(&value.to_le_bytes()),
			Endian::Big => self.bytes.extend_from_slice(&value.to_be_bytes()),
		}
	}

	fn push_u32(&mut self, value: u32) {
		match self.endian {
			Endian::Little => self.bytes.extend_from_slice(&value.to_le_bytes()),
			Endian::Big => self.bytes.extend_from_slice(&value.to_be_bytes()),
		}
	}

	fn position(&self) -> u32 {
		u32::try_from(self.bytes.len()).expect("EXIF blocks stay far below 4 GiB")
	}

	fn point(&mut self, slot: usize) {
		let here = self.position();
		write(&mut self.bytes, self.endian, slot, Value::Long(here));
	}

	fn write_ifd(&mut self, entries: &[RawEntry], children: &[u16]) -> Vec<(u16, usize)> {
		let mut tags: Vec<(u16, Option<&RawEntry>)> = entries.iter().map(|entry| (entry.tag, Some(entry))).collect();
		tags.extend(children.iter().map(|tag| (*tag, None)));
		tags.sort_by_key(|(tag, _)| *tag);

		let count = u16::try_from(tags.len()).expect("an IFD holds at most 65535 entries");
		let data_start = self.position() + 2 + u32::from(count) * 12 + 4;
		let mut data = Vec::new();
		let mut slots = Vec::new();

		self.push_u16(count);
		for (tag, entry) in tags {
			self.push_u16(tag);
			if let Some(entry) = entry {
				self.push_u16(entry.kind);
				self.push_u32(entry.count);
				if entry.value.len() <= 4 {
					let mut inline = entry.value.clone();
					inline.resize(4, 0);
					self.bytes.extend_from_slice(&inline);
				} else {
					self.push_u32(data_start + u32::try_from(data.len()).expect("EXIF blocks stay far below 4 GiB"));
					data.extend_from_slice(&entry.value);
					if entry.value.len() % 2 == 1 {
						data.push(0);
					}
				}
			} else {
				self.push_u16(TYPE_LONG);
				self.push_u32(1);
				slots.push((tag, self.bytes.len()));
				self.push_u32(0);
			}
		}
		self.push_u32(0);
		self.bytes.extend_from_slice(&data);
		slots
	}
}
