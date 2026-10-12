use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use super::{SECTOR, array};

const VRS_START: u64 = 16;
const VRS_SCAN: u64 = 64;
const ANCHOR: u64 = 256;
const MAX_VDS_SECTORS: u64 = 256;
const MAX_MAPS: u32 = 64;
const MAX_DEPTH: usize = 32;
const MAX_ENTRIES: usize = 100_000;
const MAX_PIECES: usize = 65_536;
const MAX_CONTINUATIONS: usize = 64;
const MAX_DIRECTORY_BYTES: u64 = 16 << 20;

const PRIMARY_ANCHOR: u16 = 2;
const POINTER: u16 = 3;
const PARTITION: u16 = 5;
const LOGICAL_VOLUME: u16 = 6;
const TERMINATOR: u16 = 8;
const FILE_SET: u16 = 256;
const FILE_IDENTIFIER: u16 = 257;
const ALLOCATION_EXTENT: u16 = 258;
const FILE_ENTRY: u16 = 261;
const EXTENDED_FILE_ENTRY: u16 = 266;

const DIRECTORY: u8 = 0x02;
const DELETED: u8 = 0x04;
const PARENT: u8 = 0x08;

const METADATA_PARTITION: &[u8] = b"*UDF Metadata Partition";
const SPARABLE_PARTITION: &[u8] = b"*UDF Sparable Partition";

/// A byte range of the file a `DiscFile` is read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
	pub offset: u64,
	pub length: u64,
}

/// A file on a disc: its bytes are its extents of `source`, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscFile {
	pub path: String,
	pub size: u64,
	pub extents: Vec<Extent>,
	pub source: PathBuf,
}

impl DiscFile {
	/// The file's bytes, its extents read in order.
	pub fn read(&self) -> io::Result<Vec<u8>> {
		let mut source = File::open(&self.source)?;
		let mut bytes = Vec::new();
		for extent in &self.extents {
			let length = usize::try_from(extent.length)
				.map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "the file is too large"))?;
			let start = bytes.len();
			bytes.resize(start + length, 0);
			source.seek(SeekFrom::Start(extent.offset))?;
			source.read_exact(&mut bytes[start..])?;
		}
		Ok(bytes)
	}
}

/// Every file and directory of a disc, with `/`-separated paths relative to its root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscFiles {
	pub files: Vec<DiscFile>,
	pub dirs: Vec<String>,
}

impl DiscFiles {
	pub fn find(&self, path: &str) -> Option<&DiscFile> {
		let path = path.trim_matches('/');
		self.files.iter().find(|file| same_path(&file.path, path))
	}

	pub fn dir_exists(&self, path: &str) -> bool {
		let path = path.trim_matches('/');
		path.is_empty() || self.dirs.iter().any(|dir| same_path(dir, path))
	}

	fn sort(&mut self) {
		self.files.sort_by(|a, b| a.path.cmp(&b.path));
		self.dirs.sort();
	}
}

fn same_path(a: &str, b: &str) -> bool {
	a.chars()
		.flat_map(char::to_lowercase)
		.eq(b.chars().flat_map(char::to_lowercase))
}

/// Whether the volume recognition sequence at byte 32768 holds `BEA01` followed by `NSR02` or `NSR03`.
pub fn is_udf(file: &mut File) -> io::Result<bool> {
	let len = file.metadata()?.len();
	let mut extended = false;

	for sector in VRS_START..VRS_START + VRS_SCAN {
		let at = sector * SECTOR;
		if at + 7 > len {
			return Ok(false);
		}

		let mut descriptor = [0; 7];
		file.seek(SeekFrom::Start(at))?;
		file.read_exact(&mut descriptor)?;
		match &descriptor[1..6] {
			b"BEA01" => extended = true,
			b"TEA01" => extended = false,
			b"NSR02" | b"NSR03" if extended => return Ok(true),
			b"CD001" | b"CDW02" | b"BOOT2" | b"NSR02" | b"NSR03" => {}
			_ => return Ok(false),
		}
	}
	Ok(false)
}

/// The files of a UDF 1.02–2.50 image, or `None` when it isn't UDF or its structure doesn't hold together.
pub fn read_image(path: &Path) -> io::Result<Option<DiscFiles>> {
	let mut file = File::open(path)?;
	if !is_udf(&mut file)? {
		return Ok(None);
	}

	let len = file.metadata()?.len();
	let mut reader = Reader { file, len };
	match list(&mut reader, path) {
		Ok(files) => Ok(Some(files)),
		Err(Fail::Bad) => Ok(None),
		Err(Fail::Io(error)) => Err(error),
	}
}

/// The files under `root`, each one whole extent of itself.
pub fn read_folder(root: &Path) -> io::Result<DiscFiles> {
	let mut listing = DiscFiles::default();
	walk_folder(root, "", 0, &mut listing)?;
	listing.sort();
	Ok(listing)
}

fn walk_folder(dir: &Path, prefix: &str, depth: usize, listing: &mut DiscFiles) -> io::Result<()> {
	if depth > MAX_DEPTH {
		return Err(io::Error::new(
			io::ErrorKind::InvalidData,
			"the folder is nested too deeply",
		));
	}

	for entry in fs::read_dir(dir)? {
		if listing.files.len() + listing.dirs.len() >= MAX_ENTRIES {
			return Err(io::Error::new(
				io::ErrorKind::InvalidData,
				"the folder holds too many files",
			));
		}

		let entry = entry?;
		let source = entry.path();
		let name = entry.file_name().to_string_lossy().into_owned();
		let path = if prefix.is_empty() {
			name
		} else {
			format!("{prefix}/{name}")
		};
		let metadata = fs::metadata(&source)?;
		if metadata.is_dir() {
			listing.dirs.push(path.clone());
			walk_folder(&source, &path, depth + 1, listing)?;
		} else if metadata.is_file() {
			let size = metadata.len();
			let extents = if size == 0 {
				Vec::new()
			} else {
				vec![Extent {
					offset: 0,
					length: size,
				}]
			};
			listing.files.push(DiscFile {
				path,
				size,
				extents,
				source,
			});
		}
	}
	Ok(())
}

enum Fail {
	Io(io::Error),
	Bad,
}

impl From<io::Error> for Fail {
	fn from(error: io::Error) -> Self {
		Self::Io(error)
	}
}

type Parsed<T> = Result<T, Fail>;

trait OrBad<T> {
	fn or_bad(self) -> Parsed<T>;
}

impl<T> OrBad<T> for Option<T> {
	fn or_bad(self) -> Parsed<T> {
		self.ok_or(Fail::Bad)
	}
}

fn bytes<const N: usize>(buf: &[u8], at: usize) -> Parsed<[u8; N]> {
	array(buf, at).or_bad()
}

fn u8_at(buf: &[u8], at: usize) -> Parsed<u8> {
	buf.get(at).copied().or_bad()
}

fn u16_at(buf: &[u8], at: usize) -> Parsed<u16> {
	Ok(u16::from_le_bytes(bytes(buf, at)?))
}

fn u32_at(buf: &[u8], at: usize) -> Parsed<u32> {
	Ok(u32::from_le_bytes(bytes(buf, at)?))
}

fn u64_at(buf: &[u8], at: usize) -> Parsed<u64> {
	Ok(u64::from_le_bytes(bytes(buf, at)?))
}

fn size(value: u32) -> Parsed<usize> {
	usize::try_from(value).ok().or_bad()
}

/// The descriptor tag's identifier, when its checksum holds.
fn tag(descriptor: &[u8]) -> Option<u16> {
	let header = descriptor.get(..16)?;
	let sum = header
		.iter()
		.enumerate()
		.filter(|(index, _)| *index != 4)
		.fold(0u8, |sum, (_, byte)| sum.wrapping_add(*byte));
	(sum == header[4]).then(|| u16::from_le_bytes([header[0], header[1]]))
}

fn expect(descriptor: &[u8], id: u16) -> Parsed<()> {
	if tag(descriptor) == Some(id) {
		Ok(())
	} else {
		Err(Fail::Bad)
	}
}

/// A logical block address (`lb_addr`): a block within the partition with this reference number.
#[derive(Clone, Copy)]
struct Address {
	block: u32,
	part: u16,
}

fn address(buf: &[u8], at: usize) -> Parsed<Address> {
	Ok(Address {
		block: u32_at(buf, at)?,
		part: u16_at(buf, at + 4)?,
	})
}

enum Part {
	Physical(u64),
	Metadata(Vec<Piece>),
	Unsupported,
}

/// `length` bytes of a file's content, from its byte `logical`, found at `offset` in the image.
struct Piece {
	logical: u64,
	offset: u64,
	length: u64,
}

enum Map {
	Physical(u16),
	Metadata { number: u16, main: u32, mirror: u32 },
	Unsupported,
}

struct PartitionDescriptor {
	number: u16,
	sequence: u32,
	start: u32,
}

struct Entry {
	info_len: u64,
	pieces: Vec<Piece>,
}

impl Entry {
	fn extents(&self) -> Parsed<Vec<Extent>> {
		let mut extents: Vec<Extent> = Vec::new();
		let mut covered = 0;
		for piece in &self.pieces {
			if piece.logical != covered {
				return Err(Fail::Bad);
			}

			covered += piece.length;
			match extents.last_mut() {
				Some(last) if last.offset + last.length == piece.offset => last.length += piece.length,
				_ => extents.push(Extent {
					offset: piece.offset,
					length: piece.length,
				}),
			}
		}

		if covered == self.info_len {
			Ok(extents)
		} else {
			Err(Fail::Bad)
		}
	}
}

struct Reader {
	file: File,
	len: u64,
}

impl Reader {
	fn read(&mut self, offset: u64, length: u64) -> Parsed<Vec<u8>> {
		if offset.checked_add(length).is_none_or(|end| end > self.len) {
			return Err(Fail::Bad);
		}

		let mut buf = vec![0; usize::try_from(length).ok().or_bad()?];
		self.file.seek(SeekFrom::Start(offset))?;
		self.file.read_exact(&mut buf)?;
		Ok(buf)
	}

	fn sector(&mut self, sector: u64) -> Parsed<Vec<u8>> {
		self.read(sector.checked_mul(SECTOR).or_bad()?, SECTOR)
	}

	fn anchor(&mut self) -> Parsed<[(u64, u64); 2]> {
		let last = self.len / SECTOR;
		for sector in [ANCHOR, last.saturating_sub(1), last.saturating_sub(1 + ANCHOR)] {
			let descriptor = match self.sector(sector) {
				Ok(descriptor) => descriptor,
				Err(Fail::Bad) => continue,
				Err(error) => return Err(error),
			};
			if tag(&descriptor) != Some(PRIMARY_ANCHOR) {
				continue;
			}

			let main = (u64::from(u32_at(&descriptor, 20)?), u64::from(u32_at(&descriptor, 16)?));
			let reserve = (u64::from(u32_at(&descriptor, 28)?), u64::from(u32_at(&descriptor, 24)?));
			return Ok([main, reserve]);
		}
		Err(Fail::Bad)
	}

	/// The partitions by reference number and the file set descriptor's address, from the volume descriptor
	/// sequence of `length` bytes at sector `start`.
	fn volume(&mut self, start: u64, length: u64) -> Parsed<(Vec<Part>, Address)> {
		let mut partitions: Vec<PartitionDescriptor> = Vec::new();
		let mut logical_volume: Option<(u32, Vec<u8>)> = None;
		let mut sector = start;
		let mut end = start + (length / SECTOR).min(MAX_VDS_SECTORS);

		for _ in 0..MAX_VDS_SECTORS {
			if sector >= end {
				break;
			}

			let descriptor = self.sector(sector)?;
			sector += 1;
			let sequence = u32_at(&descriptor, 16)?;
			match tag(&descriptor) {
				Some(PARTITION) => {
					let found = PartitionDescriptor {
						number: u16_at(&descriptor, 22)?,
						sequence,
						start: u32_at(&descriptor, 188)?,
					};
					match partitions.iter_mut().find(|known| known.number == found.number) {
						Some(known) if known.sequence < sequence => *known = found,
						Some(_) => {}
						None => partitions.push(found),
					}
				}
				Some(LOGICAL_VOLUME) => {
					if logical_volume.as_ref().is_none_or(|(known, _)| *known < sequence) {
						logical_volume = Some((sequence, descriptor));
					}
				}
				Some(POINTER) => {
					sector = u64::from(u32_at(&descriptor, 24)?);
					end = sector + (u64::from(u32_at(&descriptor, 20)?) / SECTOR).min(MAX_VDS_SECTORS);
				}
				Some(TERMINATOR) | None => break,
				Some(_) => {}
			}
		}

		let (_, descriptor) = logical_volume.or_bad()?;
		if u64::from(u32_at(&descriptor, 212)?) != SECTOR {
			return Err(Fail::Bad);
		}

		let file_set = address(&descriptor, 252)?;
		let maps = partition_maps(&descriptor)?;
		let physical = |number: u16| {
			partitions
				.iter()
				.find(|partition| partition.number == number)
				.map(|partition| u64::from(partition.start) * SECTOR)
		};
		let mut parts: Vec<Part> = maps
			.iter()
			.map(|map| match map {
				Map::Physical(number) => physical(*number).map_or(Part::Unsupported, Part::Physical),
				_ => Part::Unsupported,
			})
			.collect();

		for (index, map) in maps.iter().enumerate() {
			let &Map::Metadata { number, main, mirror } = map else {
				continue;
			};
			let Some(home) = physical(number).map(Part::Physical) else {
				continue;
			};

			for location in [main, mirror] {
				match self.metadata(&parts, &home, location) {
					Ok(pieces) => {
						parts[index] = Part::Metadata(pieces);
						break;
					}
					Err(Fail::Bad) => {}
					Err(error) => return Err(error),
				}
			}
		}
		Ok((parts, file_set))
	}

	fn metadata(&mut self, parts: &[Part], home: &Part, location: u32) -> Parsed<Vec<Piece>> {
		let at = self.locate(home, location)?;
		Ok(self.entry(parts, home, at)?.pieces)
	}

	/// Where `length` bytes from `block` of `part` are in the image.
	fn map(&self, part: &Part, block: u32, length: u64) -> Parsed<Vec<Extent>> {
		let start = u64::from(block) * SECTOR;
		let extents = match part {
			Part::Physical(offset) => vec![Extent {
				offset: offset + start,
				length,
			}],
			Part::Metadata(pieces) => {
				let mut logical = start;
				let end = start + length;
				let mut extents = Vec::new();
				while logical < end {
					let piece = pieces
						.iter()
						.find(|piece| piece.logical <= logical && logical < piece.logical + piece.length)
						.or_bad()?;
					let take = (piece.logical + piece.length - logical).min(end - logical);
					extents.push(Extent {
						offset: piece.offset + (logical - piece.logical),
						length: take,
					});
					logical += take;
				}
				extents
			}
			Part::Unsupported => return Err(Fail::Bad),
		};

		if extents.iter().any(|extent| {
			extent
				.offset
				.checked_add(extent.length)
				.is_none_or(|end| end > self.len)
		}) {
			return Err(Fail::Bad);
		}
		Ok(extents)
	}

	fn locate(&self, part: &Part, block: u32) -> Parsed<u64> {
		match self.map(part, block, SECTOR)?.as_slice() {
			[extent] => Ok(extent.offset),
			_ => Err(Fail::Bad),
		}
	}

	/// The File Entry or Extended File Entry at byte `at`, which lives in partition `own`.
	fn entry(&mut self, parts: &[Part], own: &Part, at: u64) -> Parsed<Entry> {
		let descriptor = self.read(at, SECTOR)?;
		let (header, lengths) = match tag(&descriptor) {
			Some(FILE_ENTRY) => (176, 168),
			Some(EXTENDED_FILE_ENTRY) => (216, 208),
			_ => return Err(Fail::Bad),
		};

		let info_len = u64_at(&descriptor, 56)?;
		let start = size(u32_at(&descriptor, lengths)?)?.checked_add(header).or_bad()?;
		let area = descriptor
			.get(start..start.checked_add(size(u32_at(&descriptor, lengths + 4)?)?).or_bad()?)
			.or_bad()?;
		let pieces = match u16_at(&descriptor, 34)? & 7 {
			0 => self.allocated(parts, own, area.to_vec(), false, info_len)?,
			1 => self.allocated(parts, own, area.to_vec(), true, info_len)?,
			3 if info_len == 0 => Vec::new(),
			3 if info_len <= area.len() as u64 => vec![Piece {
				logical: 0,
				offset: at + start as u64,
				length: info_len,
			}],
			_ => return Err(Fail::Bad),
		};
		Ok(Entry { info_len, pieces })
	}

	/// The pieces that short (8-byte) or long (16-byte) allocation descriptors record, up to `info_len`, following
	/// Allocation Extent Descriptors. Unrecorded extents leave a gap.
	fn allocated(
		&mut self,
		parts: &[Part],
		own: &Part,
		first: Vec<u8>,
		long: bool,
		info_len: u64,
	) -> Parsed<Vec<Piece>> {
		let width = if long { 16 } else { 8 };
		let mut descriptors = first;
		let mut at = 0;
		let mut logical = 0;
		let mut pieces = Vec::new();
		let mut continuations = 0;

		while logical < info_len {
			let Some(raw) = descriptors.get(at..at + width) else {
				break;
			};
			at += width;
			let word = u32_at(raw, 0)?;
			let length = u64::from(word & 0x3FFF_FFFF);
			if length == 0 {
				break;
			}

			let block = u32_at(raw, 4)?;
			let part = if long {
				parts.get(usize::from(u16_at(raw, 8)?)).or_bad()?
			} else {
				own
			};
			match word >> 30 {
				0 => {
					for extent in self.map(part, block, length.min(info_len - logical))? {
						pieces.push(Piece {
							logical,
							offset: extent.offset,
							length: extent.length,
						});
						logical += extent.length;
					}
				}
				3 => {
					continuations += 1;
					if continuations > MAX_CONTINUATIONS {
						return Err(Fail::Bad);
					}

					let next = self.locate(part, block)?;
					let next = self.read(next, SECTOR)?;
					expect(&next, ALLOCATION_EXTENT)?;
					let end = 24usize.checked_add(size(u32_at(&next, 20)?)?).or_bad()?;
					descriptors = next.get(24..end).or_bad()?.to_vec();
					at = 0;
				}
				_ => logical += length.min(info_len - logical),
			}

			if pieces.len() > MAX_PIECES {
				return Err(Fail::Bad);
			}
		}
		Ok(pieces)
	}
}

fn partition_maps(descriptor: &[u8]) -> Parsed<Vec<Map>> {
	let table = size(u32_at(descriptor, 264)?)?;
	let count = u32_at(descriptor, 268)?;
	let end = 440usize.checked_add(table).or_bad()?;
	if end > descriptor.len() {
		return Err(Fail::Bad);
	}

	let mut maps = Vec::new();
	let mut at = 440;
	for _ in 0..count.min(MAX_MAPS) {
		let kind = u8_at(descriptor, at)?;
		let length = usize::from(u8_at(descriptor, at + 1)?);
		if length < 6 || at + length > end {
			return Err(Fail::Bad);
		}

		let map = match kind {
			1 => Map::Physical(u16_at(descriptor, at + 4)?),
			2 => {
				let identifier = descriptor.get(at + 5..at + 28).or_bad()?;
				let number = u16_at(descriptor, at + 38)?;
				if identifier == METADATA_PARTITION {
					Map::Metadata {
						number,
						main: u32_at(descriptor, at + 40)?,
						mirror: u32_at(descriptor, at + 44)?,
					}
				} else if identifier == SPARABLE_PARTITION {
					Map::Physical(number)
				} else {
					Map::Unsupported
				}
			}
			_ => Map::Unsupported,
		};
		maps.push(map);
		at += length;
	}
	Ok(maps)
}

fn list(reader: &mut Reader, source: &Path) -> Parsed<DiscFiles> {
	let mut volume = Err(Fail::Bad);
	for (start, length) in reader.anchor()? {
		volume = reader.volume(start, length);
		if !matches!(volume, Err(Fail::Bad)) {
			break;
		}
	}
	let (parts, file_set) = volume?;

	let part = parts.get(usize::from(file_set.part)).or_bad()?;
	let at = reader.locate(part, file_set.block)?;
	let descriptor = reader.read(at, SECTOR)?;
	expect(&descriptor, FILE_SET)?;
	let root = address(&descriptor, 404)?;

	let part = parts.get(usize::from(root.part)).or_bad()?;
	let at = reader.locate(part, root.block)?;
	let entry = reader.entry(&parts, part, at)?;

	let mut walk = Walk {
		parts,
		source: source.to_path_buf(),
		listing: DiscFiles::default(),
		visited: HashSet::from([at]),
		entries: 0,
	};
	walk.directory(reader, &entry, "", 0)?;
	walk.listing.sort();
	Ok(walk.listing)
}

struct Walk {
	parts: Vec<Part>,
	source: PathBuf,
	listing: DiscFiles,
	visited: HashSet<u64>,
	entries: usize,
}

impl Walk {
	fn directory(&mut self, reader: &mut Reader, entry: &Entry, prefix: &str, depth: usize) -> Parsed<()> {
		if depth > MAX_DEPTH || entry.info_len > MAX_DIRECTORY_BYTES {
			return Err(Fail::Bad);
		}

		let mut data = Vec::new();
		for extent in entry.extents()? {
			data.extend(reader.read(extent.offset, extent.length)?);
		}

		let mut at = 0;
		while let Some(fid) = data.get(at..).filter(|rest| rest.len() >= 38) {
			if fid[..16].iter().all(|&byte| byte == 0) {
				break;
			}

			expect(fid, FILE_IDENTIFIER)?;
			let characteristics = u8_at(fid, 18)?;
			let name_start = 38 + usize::from(u16_at(fid, 36)?);
			let name_end = name_start + usize::from(u8_at(fid, 19)?);
			let name = fid.get(name_start..name_end).or_bad()?;
			let icb = address(fid, 24)?;
			at += name_end.next_multiple_of(4);
			if characteristics & (DELETED | PARENT) != 0 {
				continue;
			}

			self.entries += 1;
			if self.entries > MAX_ENTRIES {
				return Err(Fail::Bad);
			}

			let Some(name) = decode(name).filter(|name| usable(name)) else {
				continue;
			};
			let path = if prefix.is_empty() {
				name
			} else {
				format!("{prefix}/{name}")
			};
			let part = self.parts.get(usize::from(icb.part)).or_bad()?;
			let child_at = reader.locate(part, icb.block)?;
			let child = reader.entry(&self.parts, part, child_at)?;

			if characteristics & DIRECTORY != 0 {
				if !self.visited.insert(child_at) {
					return Err(Fail::Bad);
				}

				self.listing.dirs.push(path.clone());
				self.directory(reader, &child, &path, depth + 1)?;
			} else {
				self.listing.files.push(DiscFile {
					path,
					size: child.info_len,
					extents: child.extents()?,
					source: self.source.clone(),
				});
			}
		}
		Ok(())
	}
}

/// An OSTA CS0 compressed Unicode name: 8-bit code points or UTF-16 big-endian after a compression id.
fn decode(bytes: &[u8]) -> Option<String> {
	let (&id, rest) = bytes.split_first()?;
	match id {
		8 | 254 => Some(rest.iter().map(|&byte| char::from(byte)).collect()),
		16 | 255 if rest.len().is_multiple_of(2) => {
			let units = rest.as_chunks::<2>().0.iter().copied().map(u16::from_be_bytes);
			Some(
				char::decode_utf16(units)
					.map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
					.collect(),
			)
		}
		_ => None,
	}
}

fn usable(name: &str) -> bool {
	!name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0'])
}
