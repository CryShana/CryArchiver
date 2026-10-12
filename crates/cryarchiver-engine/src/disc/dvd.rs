use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

use super::ps::{self, Reader};
use super::udf::{DiscFile, DiscFiles, Extent};

pub const MAX_DVD_MBITS: f64 = 10.08;
pub const MIN_TITLE_S: f64 = 10.0;
pub const DUPLICATE_COVERAGE: f64 = 0.9;

pub const SECTOR: u64 = 2048;

/// A VOBU lasts at most a second, so this is the most one can hold at the mux maximum.
const VOBU_BYTES: f64 = MAX_DVD_MBITS * 1e6 / 8.0;
const MAX_IFO_BYTES: u64 = 16 << 20;
const MAX_VOB_PARTS: u8 = 9;
const MAX_AUDIO: u16 = 8;
const MAX_SUBPICTURES: u16 = 32;
const ANGLE_BLOCK: u8 = 1;
const FIRST_IN_BLOCK: u8 = 1;
const INTERLEAVED: u8 = 0x04;
const AVAILABLE_AUDIO: u16 = 0x8000;
const AVAILABLE_SUBPICTURE: u32 = 0x8000_0000;
const SAMPLED_CELLS: usize = 3;
const SAMPLED_SECTORS: usize = 64;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Cell {
	pub first_sector: u32,
	pub last_sector: u32,
	pub time_s: f64,
	pub still: bool,
	/// The cell is in an interleaved block: its sectors hold other angles' or branches' units between its own.
	pub interleaved: bool,
	/// An interleaved cell's own units as inclusive sector ranges, in order: empty when they couldn't be followed.
	pub units: Vec<(u32, u32)>,
}

impl Cell {
	/// The inclusive sector ranges the cell plays.
	pub fn ranges(&self) -> Vec<(u32, u32)> {
		if self.units.is_empty() {
			vec![(self.first_sector, self.last_sector)]
		} else {
			self.units.clone()
		}
	}

	pub fn sectors(&self) -> u64 {
		self.ranges()
			.iter()
			.map(|(first, last)| u64::from(last.saturating_sub(*first)) + 1)
			.sum()
	}

	/// An interleaved cell whose own units couldn't be followed: read whole, it would mix in other angles or branches.
	pub fn mixed(&self) -> bool {
		self.interleaved && self.units.is_empty()
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VideoAttr {
	pub width: u32,
	pub height: u32,
	pub wide: bool,
	pub pal: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCoding {
	Ac3,
	Mpeg,
	Lpcm {
		bits: u8,
	},
	Dts,
	/// A coding ffmpeg doesn't read from a DVD, such as SDDS.
	Other,
}

impl AudioCoding {
	/// The id ffmpeg gives stream `number` of this coding.
	fn id(self, number: u8) -> Option<u32> {
		let base = match self {
			Self::Ac3 => 0x80,
			Self::Dts => 0x88,
			Self::Lpcm { .. } => 0xA0,
			Self::Mpeg => 0x1C0,
			Self::Other => return None,
		};
		Some(base + u32::from(number))
	}
}

/// An audio stream of a title set, in its logical order. `id` is the stream ffmpeg finds for it in a title's
/// footage, when the title plays it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioAttr {
	pub coding: AudioCoding,
	pub channels: u8,
	pub language: Option<String>,
	pub id: Option<u32>,
}

/// A subpicture stream of a title set, in its logical order, like `AudioAttr`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Subpicture {
	pub language: Option<String>,
	pub id: Option<u32>,
}

/// A title of the VTS title domain. `palette` holds the IFO's `0x00YYCrCb` entries.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DvdTitle {
	pub number: u16,
	pub vts: u8,
	pub cells: Vec<Cell>,
	pub chapter_starts_s: Vec<f64>,
	pub video: VideoAttr,
	pub audio: Vec<AudioAttr>,
	pub subpictures: Vec<Subpicture>,
	pub palette: [u32; 16],
	/// The title has a multi-angle block: only its first angle is read.
	pub angles: bool,
}

/// A disc's titles and, per title set, the extents of `VTS_xx_1.VOB`..`VTS_xx_N.VOB` in order: cell sectors count
/// from the start of that concatenation.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Dvd {
	pub titles: Vec<DvdTitle>,
	pub vobs: Vec<(u8, Vec<(PathBuf, Extent)>)>,
}

impl Dvd {
	pub fn vob_sectors(&self, vts: u8) -> u64 {
		self.vobs
			.iter()
			.find(|(number, _)| *number == vts)
			.map_or(0, |(_, extents)| {
				extents.iter().map(|(_, extent)| extent.length).sum::<u64>() / SECTOR
			})
	}

	/// The `(source, offset, length)` pieces holding sectors `first_sector..=last_sector` of a title set's VOBs.
	pub fn byte_ranges(&self, vts: u8, first_sector: u32, last_sector: u32) -> Vec<(PathBuf, u64, u64)> {
		let Some((_, extents)) = self.vobs.iter().find(|(number, _)| *number == vts) else {
			return Vec::new();
		};

		let start = u64::from(first_sector) * SECTOR;
		let end = (u64::from(last_sector) + 1) * SECTOR;
		let mut ranges: Vec<(PathBuf, u64, u64)> = Vec::new();
		let mut logical = 0;
		for (source, extent) in extents {
			let from = start.max(logical);
			let to = end.min(logical + extent.length);
			if from < to {
				let offset = extent.offset + (from - logical);
				let length = to - from;
				match ranges.last_mut() {
					Some((path, at, len)) if path == source && *at + *len == offset => *len += length,
					_ => ranges.push((source.clone(), offset, length)),
				}
			}

			logical += extent.length;
			if logical >= end {
				break;
			}
		}
		ranges
	}

	/// The `(source, offset, length)` pieces holding the sectors `cell` plays.
	pub fn cell_ranges(&self, vts: u8, cell: &Cell) -> Vec<(PathBuf, u64, u64)> {
		cell.ranges()
			.into_iter()
			.flat_map(|(first, last)| self.byte_ranges(vts, first, last))
			.collect()
	}

	/// The kept titles of `selection`, in order.
	pub fn kept<'a>(&'a self, selection: &'a Selection) -> impl Iterator<Item = &'a DvdTitle> {
		selection
			.kept
			.iter()
			.filter_map(|kept| self.titles.iter().find(|title| title.number == kept.title))
	}

	/// The bytes of footage the kept titles of `selection` read.
	pub fn footage_bytes(&self, selection: &Selection) -> u64 {
		self.kept(selection)
			.flat_map(|title| title.cells.iter().flat_map(|cell| self.cell_ranges(title.vts, cell)))
			.map(|(_, _, length)| length)
			.sum()
	}

	/// Whether a kept title of `selection` has an interleaved cell whose own units couldn't be followed.
	pub fn mixed(&self, selection: &Selection) -> bool {
		self.kept(selection).any(|title| title.cells.iter().any(Cell::mixed))
	}

	/// Whether the kept titles' footage is scrambled (CSS), from the first sectors of their first cells.
	pub fn encrypted(&self, selection: &Selection) -> io::Result<bool> {
		let mut reader = Reader::default();
		for title in self.kept(selection) {
			for cell in title.cells.iter().take(SAMPLED_CELLS) {
				let mut scrambled = false;
				reader.each_sector(&self.cell_ranges(title.vts, cell), SAMPLED_SECTORS, |_, sector| {
					scrambled = ps::packets(sector).iter().any(|packet| packet.scrambled);
					!scrambled
				})?;
				if scrambled {
					return Ok(true);
				}
			}
		}
		Ok(false)
	}

	fn readable(&self, title: &DvdTitle) -> Vec<Cell> {
		let sectors = self.vob_sectors(title.vts);
		title
			.cells
			.iter()
			.filter(|cell| cell.first_sector <= cell.last_sector && u64::from(cell.last_sector) < sectors)
			.cloned()
			.collect()
	}

	/// An interleaved cell's own units, each found from the DSI of the nav pack that starts the one before it;
	/// empty when the chain breaks or leaves the cell, an error when a sector can't be read.
	fn units(&self, reader: &mut Reader, vts: u8, cell: &Cell) -> io::Result<Vec<(u32, u32)>> {
		let mut units = Vec::new();
		let mut at = cell.first_sector;
		let mut sector = vec![0; ps::SECTOR];
		for _ in 0..=cell.last_sector.saturating_sub(cell.first_sector) {
			self.read_sector(reader, vts, at, &mut sector)?;
			let Some(dsi) = ps::interleave(&sector, at).filter(|dsi| dsi.in_unit) else {
				return Ok(Vec::new());
			};
			let Some(end) = at.checked_add(dsi.unit_end).filter(|end| *end <= cell.last_sector) else {
				return Ok(Vec::new());
			};

			units.push((at, end));
			if end == cell.last_sector {
				return Ok(units);
			}

			match at.checked_add(dsi.next_unit) {
				Some(next) if next > end && next <= cell.last_sector => at = next,
				_ => return Ok(Vec::new()),
			}
		}
		Ok(Vec::new())
	}

	fn read_sector(&self, reader: &mut Reader, vts: u8, number: u32, sector: &mut [u8]) -> io::Result<()> {
		let mut filled: usize = 0;
		for (path, offset, length) in self.byte_ranges(vts, number, number) {
			let end = usize::try_from(length)
				.ok()
				.and_then(|length| filled.checked_add(length))
				.filter(|end| *end <= sector.len())
				.ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
			reader.read(&path, offset, &mut sector[filled..end])?;
			filled = end;
		}

		if filled == sector.len() {
			Ok(())
		} else {
			Err(io::ErrorKind::UnexpectedEof.into())
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dropped {
	Menu,
	Still,
	Short,
	Duplicate,
	Unreadable,
}

impl Dropped {
	pub fn reason(self) -> &'static str {
		match self {
			Self::Menu => "menu",
			Self::Still => "still picture",
			Self::Short => "under 10 s",
			Self::Duplicate => "duplicate",
			Self::Unreadable => "unreadable",
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kept {
	pub title: u16,
	pub false_time: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Selection {
	pub kept: Vec<Kept>,
	pub dropped: Vec<(u16, Dropped)>,
}

/// The titles worth archiving, in VMG order: still loops, titles under `MIN_TITLE_S` and titles whose sectors kept
/// titles already cover are dropped. A title's time is false when its bytes over it exceed `MAX_DVD_MBITS`.
#[allow(clippy::cast_precision_loss)]
pub fn select(dvd: &Dvd) -> Selection {
	let mut selection = Selection::default();
	let mut covered: BTreeMap<u8, Vec<(u64, u64)>> = BTreeMap::new();

	for title in &dvd.titles {
		let cells = dvd.readable(title);
		if cells.is_empty() {
			selection.dropped.push((title.number, Dropped::Unreadable));
			continue;
		}

		let bytes = cells.iter().map(|cell| cell.sectors() * SECTOR).sum::<u64>() as f64;
		let time: f64 = cells.iter().map(|cell| cell.time_s).sum();
		let at_mux_max_s = bytes * 8.0 / (MAX_DVD_MBITS * 1e6);
		let false_time = time <= 0.0 || at_mux_max_s > time;
		let length = if false_time { at_mux_max_s } else { time };

		if let [cell] = cells.as_slice()
			&& (bytes <= VOBU_BYTES || (cell.still && at_mux_max_s < MIN_TITLE_S))
		{
			selection.dropped.push((title.number, Dropped::Still));
			continue;
		}

		if length < MIN_TITLE_S {
			selection.dropped.push((title.number, Dropped::Short));
			continue;
		}

		let ranges = merge(
			cells
				.iter()
				.flat_map(Cell::ranges)
				.map(|(first, last)| (u64::from(first), u64::from(last) + 1)),
		);
		let kept = covered.entry(title.vts).or_default();
		let total: u64 = ranges.iter().map(|(from, to)| to - from).sum();
		if overlap(&ranges, kept) as f64 >= DUPLICATE_COVERAGE * total as f64 {
			selection.dropped.push((title.number, Dropped::Duplicate));
			continue;
		}

		*kept = merge(kept.iter().copied().chain(ranges));
		selection.kept.push(Kept {
			title: title.number,
			false_time,
		});
	}
	selection
}

/// Half-open ranges sorted and joined where they touch or overlap.
fn merge(ranges: impl IntoIterator<Item = (u64, u64)>) -> Vec<(u64, u64)> {
	let mut ranges: Vec<(u64, u64)> = ranges.into_iter().filter(|(from, to)| from < to).collect();
	ranges.sort_unstable();

	let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
	for (from, to) in ranges {
		match merged.last_mut() {
			Some(last) if from <= last.1 => last.1 = last.1.max(to),
			_ => merged.push((from, to)),
		}
	}
	merged
}

fn overlap(a: &[(u64, u64)], b: &[(u64, u64)]) -> u64 {
	a.iter()
		.flat_map(|&(a_from, a_to)| {
			b.iter()
				.map(move |&(b_from, b_to)| a_to.min(b_to).saturating_sub(a_from.max(b_from)))
		})
		.sum()
}

/// The disc's titles from `VIDEO_TS.IFO` and the `VTS_xx_0.IFO`s, each falling back to its `.BUP`. A title set
/// whose IFOs both fail leaves its titles without cells. Cells outside their title set's VOBs are left out.
pub fn parse(files: &DiscFiles) -> Result<Dvd, String> {
	let entries = with_backup(files, "VIDEO_TS/VIDEO_TS", vmg_titles)?;

	let mut numbers: Vec<u8> = entries.iter().map(|&(vts, _)| vts).filter(|&vts| vts > 0).collect();
	numbers.sort_unstable();
	numbers.dedup();

	let mut dvd = Dvd::default();
	let mut sets = BTreeMap::new();
	for vts in numbers {
		dvd.vobs.push((vts, vob_extents(files, vts)));
		let set = with_backup(files, &format!("VIDEO_TS/VTS_{vts:02}_0"), title_set).ok();
		sets.insert(vts, set);
	}

	for (index, &(vts, vts_title)) in entries.iter().enumerate() {
		let number = u16::try_from(index + 1).map_err(|_| "the disc lists too many titles".to_owned())?;
		let sectors = dvd.vob_sectors(vts);
		let title = sets
			.get(&vts)
			.and_then(Option::as_ref)
			.and_then(|set| set.title(number, vts, vts_title, sectors))
			.unwrap_or(DvdTitle {
				number,
				vts,
				..DvdTitle::default()
			});
		dvd.titles.push(title);
	}

	let mut titles = std::mem::take(&mut dvd.titles);
	let mut reader = Reader::default();
	for title in &mut titles {
		let sectors = dvd.vob_sectors(title.vts);
		for cell in title
			.cells
			.iter_mut()
			.filter(|cell| cell.interleaved && u64::from(cell.last_sector) < sectors)
		{
			cell.units = dvd
				.units(&mut reader, title.vts, cell)
				.map_err(|err| format!("an interleaved cell of title {} can't be read: {err}", title.number))?;
		}
	}
	dvd.titles = titles;
	Ok(dvd)
}

fn vob_extents(files: &DiscFiles, vts: u8) -> Vec<(PathBuf, Extent)> {
	let mut extents = Vec::new();
	for part in 1..=MAX_VOB_PARTS {
		let Some(file) = files.find(&format!("VIDEO_TS/VTS_{vts:02}_{part}.VOB")) else {
			break;
		};
		extents.extend(file.extents.iter().map(|extent| (file.source.clone(), *extent)));
	}
	extents
}

fn with_backup<T>(files: &DiscFiles, stem: &str, parse: fn(&[u8]) -> Result<T, String>) -> Result<T, String> {
	let mut reasons = Vec::new();
	for extension in ["IFO", "BUP"] {
		let name = format!("{stem}.{extension}");
		let Some(file) = files.find(&name) else {
			reasons.push(format!("{name} is missing"));
			continue;
		};

		match read_file(file).and_then(|bytes| parse(&bytes)) {
			Ok(parsed) => return Ok(parsed),
			Err(reason) => reasons.push(format!("{name}: {reason}")),
		}
	}
	Err(reasons.join("; "))
}

fn read_file(file: &DiscFile) -> Result<Vec<u8>, String> {
	if file.size > MAX_IFO_BYTES {
		return Err("it is too large for an IFO".to_owned());
	}

	file.read().map_err(|error| format!("it can't be read: {error}"))
}

fn get<const N: usize>(ifo: &[u8], at: usize) -> Result<[u8; N], String> {
	at.checked_add(N)
		.and_then(|end| ifo.get(at..end))
		.and_then(|slice| slice.try_into().ok())
		.ok_or_else(|| format!("it ends before byte {at}"))
}

fn byte(ifo: &[u8], at: usize) -> Result<u8, String> {
	Ok(get::<1>(ifo, at)?[0])
}

fn be16(ifo: &[u8], at: usize) -> Result<u16, String> {
	Ok(u16::from_be_bytes(get(ifo, at)?))
}

fn be32(ifo: &[u8], at: usize) -> Result<u32, String> {
	Ok(u32::from_be_bytes(get(ifo, at)?))
}

fn offset(base: usize, relative: u32) -> Result<usize, String> {
	usize::try_from(relative)
		.ok()
		.and_then(|relative| base.checked_add(relative))
		.ok_or_else(|| "a table offset is out of range".to_owned())
}

/// The byte offset of the table whose sector number is stored at `at`.
fn table(ifo: &[u8], at: usize) -> Result<usize, String> {
	let sector = u64::from(be32(ifo, at)?) * SECTOR;
	usize::try_from(sector)
		.ok()
		.filter(|&start| start > 0 && start < ifo.len())
		.ok_or_else(|| format!("the table pointer at byte {at} is out of range"))
}

fn magic(ifo: &[u8], expected: &[u8; 12]) -> Result<(), String> {
	if &get::<12>(ifo, 0)? == expected {
		Ok(())
	} else {
		Err(format!("it is not a {} IFO", String::from_utf8_lossy(&expected[9..])))
	}
}

/// Each title's `(VTS number, VTS title number)` from the title search pointer table.
fn vmg_titles(ifo: &[u8]) -> Result<Vec<(u8, u8)>, String> {
	magic(ifo, b"DVDVIDEO-VMG")?;
	let table = table(ifo, 0xC4)?;
	let count = usize::from(be16(ifo, table)?);

	let mut titles = Vec::with_capacity(count);
	for index in 0..count {
		let at = table + 8 + 12 * index;
		titles.push((byte(ifo, at + 6)?, byte(ifo, at + 7)?));
	}
	Ok(titles)
}

struct TitleSet {
	video: VideoAttr,
	audio: Vec<AudioAttr>,
	subpictures: Vec<Subpicture>,
	ptts: Vec<Vec<(u16, u16)>>,
	pgcs: Vec<Pgc>,
}

struct Pgc {
	programs: Vec<u8>,
	cells: Vec<PgcCell>,
	palette: [u32; 16],
	audio_control: [u16; 8],
	subpicture_control: [u32; 32],
}

impl Pgc {
	/// The stream number logical audio stream `index` plays as; every stream its index when the PGC marks none.
	fn audio_stream(&self, index: usize) -> Option<u8> {
		if self.audio_control.iter().all(|control| control & AVAILABLE_AUDIO == 0) {
			return u8::try_from(index).ok();
		}

		let control = *self.audio_control.get(index)?;
		(control & AVAILABLE_AUDIO != 0).then_some(control.to_be_bytes()[0] & 7)
	}

	/// Like `audio_stream`, for subpictures: a wide picture plays each one's wide variant.
	fn subpicture_stream(&self, index: usize, wide: bool) -> Option<u8> {
		if self
			.subpicture_control
			.iter()
			.all(|control| control & AVAILABLE_SUBPICTURE == 0)
		{
			return u8::try_from(index).ok();
		}

		let control = self.subpicture_control.get(index)?.to_be_bytes();
		let number = if wide { control[1] } else { control[0] };
		(control[0] & 0x80 != 0).then_some(number & 0x1F)
	}
}

struct PgcCell {
	cell: Cell,
	block_mode: u8,
	block_type: u8,
}

impl PgcCell {
	fn plays(&self) -> bool {
		self.block_type != ANGLE_BLOCK || self.block_mode == FIRST_IN_BLOCK
	}
}

fn title_set(ifo: &[u8]) -> Result<TitleSet, String> {
	magic(ifo, b"DVDVIDEO-VTS")?;
	let video = video_attr(get(ifo, 0x200)?);

	let mut audio = Vec::new();
	for index in 0..be16(ifo, 0x202)?.min(MAX_AUDIO) {
		audio.push(audio_attr(get(ifo, 0x204 + 8 * usize::from(index))?));
	}

	let mut subpictures = Vec::new();
	for index in 0..be16(ifo, 0x254)?.min(MAX_SUBPICTURES) {
		let attr: [u8; 6] = get(ifo, 0x256 + 6 * usize::from(index))?;
		subpictures.push(Subpicture {
			language: language(attr[0] & 3, [attr[2], attr[3]]),
			id: None,
		});
	}

	Ok(TitleSet {
		video,
		audio,
		subpictures,
		ptts: ptts(ifo)?,
		pgcs: pgcs(ifo)?,
	})
}

fn video_attr(attr: [u8; 2]) -> VideoAttr {
	let pal = (attr[0] >> 4) & 3 == 1;
	let full = if pal { 576 } else { 480 };
	let (width, height) = match (attr[1] >> 3) & 7 {
		1 => (704, full),
		2 => (352, full),
		3 => (352, full / 2),
		_ => (720, full),
	};
	VideoAttr {
		width,
		height,
		wide: (attr[0] >> 2) & 3 == 3,
		pal,
	}
}

fn audio_attr(attr: [u8; 8]) -> AudioAttr {
	let coding = match attr[0] >> 5 {
		0 => AudioCoding::Ac3,
		2 | 3 => AudioCoding::Mpeg,
		4 => AudioCoding::Lpcm {
			bits: match attr[1] >> 6 {
				1 => 20,
				2 => 24,
				_ => 16,
			},
		},
		6 => AudioCoding::Dts,
		_ => AudioCoding::Other,
	};
	AudioAttr {
		coding,
		channels: (attr[1] & 7) + 1,
		language: language((attr[0] >> 2) & 3, [attr[2], attr[3]]),
		id: None,
	}
}

fn language(kind: u8, code: [u8; 2]) -> Option<String> {
	(kind == 1 && code.iter().all(u8::is_ascii_alphabetic)).then(|| String::from_utf8_lossy(&code).to_ascii_lowercase())
}

/// Each VTS title's parts of title as `(PGC number, program number)`.
fn ptts(ifo: &[u8]) -> Result<Vec<Vec<(u16, u16)>>, String> {
	let table = table(ifo, 0xC8)?;
	let count = usize::from(be16(ifo, table)?);
	let end = offset(table, be32(ifo, table + 4)?)?.saturating_add(1).min(ifo.len());

	let mut starts = Vec::with_capacity(count);
	for index in 0..count {
		starts.push(offset(table, be32(ifo, table + 8 + 4 * index)?)?);
	}

	let mut titles = Vec::with_capacity(count);
	for (index, &start) in starts.iter().enumerate() {
		let stop = starts.get(index + 1).copied().unwrap_or(end).min(ifo.len());
		let mut parts = Vec::new();
		let mut at = start;
		while at + 4 <= stop {
			parts.push((be16(ifo, at)?, be16(ifo, at + 2)?));
			at += 4;
		}
		titles.push(parts);
	}
	Ok(titles)
}

fn pgcs(ifo: &[u8]) -> Result<Vec<Pgc>, String> {
	let table = table(ifo, 0xCC)?;
	let count = usize::from(be16(ifo, table)?);

	let mut pgcs = Vec::with_capacity(count);
	for index in 0..count {
		let at = offset(table, be32(ifo, table + 8 + 8 * index + 4)?)?;
		pgcs.push(pgc(ifo, at)?);
	}
	Ok(pgcs)
}

fn pgc(ifo: &[u8], at: usize) -> Result<Pgc, String> {
	let program_count = usize::from(byte(ifo, at + 2)?);
	let cell_count = usize::from(byte(ifo, at + 3)?);
	let still = byte(ifo, at + 0xA2)? != 0;

	let mut palette = [0; 16];
	for (index, entry) in palette.iter_mut().enumerate() {
		*entry = be32(ifo, at + 0xA4 + 4 * index)?;
	}

	let mut audio_control = [0; 8];
	for (index, control) in audio_control.iter_mut().enumerate() {
		*control = be16(ifo, at + 0x0C + 2 * index)?;
	}
	let mut subpicture_control = [0; 32];
	for (index, control) in subpicture_control.iter_mut().enumerate() {
		*control = be32(ifo, at + 0x1C + 4 * index)?;
	}

	let program_map = offset(at, u32::from(be16(ifo, at + 0xE6)?))?;
	let mut programs = Vec::with_capacity(program_count);
	for index in 0..program_count {
		programs.push(byte(ifo, program_map + index)?);
	}

	let playback = offset(at, u32::from(be16(ifo, at + 0xE8)?))?;
	let mut cells = Vec::with_capacity(cell_count);
	for index in 0..cell_count {
		let entry = playback + 24 * index;
		let category = byte(ifo, entry)?;
		cells.push(PgcCell {
			cell: Cell {
				first_sector: be32(ifo, entry + 8)?,
				last_sector: be32(ifo, entry + 20)?,
				time_s: playback_time(get(ifo, entry + 4)?),
				still: byte(ifo, entry + 2)? != 0 || (still && cell_count == 1),
				interleaved: category & INTERLEAVED != 0,
				units: Vec::new(),
			},
			block_mode: category >> 6,
			block_type: (category >> 4) & 3,
		});
	}

	Ok(Pgc {
		programs,
		cells,
		palette,
		audio_control,
		subpicture_control,
	})
}

/// BCD hours, minutes, seconds and frames, the frame rate in the top two bits of the frames byte.
fn playback_time(time: [u8; 4]) -> f64 {
	let bcd = |value: u8| {
		let (high, low) = (value >> 4, value & 0x0F);
		(high < 10 && low < 10).then_some(f64::from(high * 10 + low))
	};
	let fps = match time[3] >> 6 {
		1 => Some(25.0),
		3 => Some(30.0),
		_ => None,
	};

	match (bcd(time[0]), bcd(time[1]), bcd(time[2]), bcd(time[3] & 0x3F)) {
		(Some(hours), Some(minutes), Some(seconds), Some(frames)) => {
			hours * 3600.0 + minutes * 60.0 + seconds + fps.map_or(0.0, |fps| frames / fps)
		}
		_ => 0.0,
	}
}

impl TitleSet {
	/// VMG title `number`: the cells of the PGCs its parts of title name, in order, with a chapter at each part's
	/// program. Cells past `sectors` are left out.
	fn title(&self, number: u16, vts: u8, vts_title: u8, sectors: u64) -> Option<DvdTitle> {
		let parts = self.ptts.get(usize::from(vts_title).checked_sub(1)?)?;
		let mut pgc_numbers: Vec<u16> = Vec::new();
		for &(pgcn, _) in parts {
			if !pgc_numbers.contains(&pgcn) {
				pgc_numbers.push(pgcn);
			}
		}

		let mut cells = Vec::new();
		let mut chapter_starts_s: Vec<f64> = Vec::new();
		let mut elapsed = 0.0;
		let mut first = None;
		let mut angles = false;
		for pgcn in pgc_numbers {
			let pgc = self.pgcs.get(usize::from(pgcn).checked_sub(1)?)?;
			first.get_or_insert(pgc);

			let mut cell_starts = Vec::with_capacity(pgc.cells.len());
			for entry in &pgc.cells {
				cell_starts.push(elapsed);
				angles |= entry.block_type == ANGLE_BLOCK;
				let cell = &entry.cell;
				if entry.plays() && cell.first_sector <= cell.last_sector && u64::from(cell.last_sector) < sectors {
					elapsed += cell.time_s;
					cells.push(cell.clone());
				}
			}

			for &(_, program) in parts.iter().filter(|(part_pgcn, _)| *part_pgcn == pgcn) {
				let start = usize::from(program)
					.checked_sub(1)
					.and_then(|program| pgc.programs.get(program))
					.and_then(|&entry| usize::from(entry).checked_sub(1))
					.and_then(|entry| cell_starts.get(entry));
				chapter_starts_s.extend(start);
			}
		}

		chapter_starts_s.sort_by(f64::total_cmp);
		chapter_starts_s.dedup();

		let first = first?;
		let audio = self
			.audio
			.iter()
			.enumerate()
			.map(|(index, attr)| AudioAttr {
				id: first.audio_stream(index).and_then(|number| attr.coding.id(number)),
				..attr.clone()
			})
			.collect();
		let subpictures = self
			.subpictures
			.iter()
			.enumerate()
			.map(|(index, subpicture)| Subpicture {
				id: first
					.subpicture_stream(index, self.video.wide)
					.map(|number| 0x20 + u32::from(number)),
				..subpicture.clone()
			})
			.collect();
		Some(DvdTitle {
			number,
			vts,
			cells,
			chapter_starts_s,
			video: self.video,
			audio,
			subpictures,
			palette: first.palette,
			angles,
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const MBYTE_SECTORS: u32 = 512;

	fn cell(first_sector: u32, last_sector: u32, time_s: f64) -> Cell {
		Cell {
			first_sector,
			last_sector,
			time_s,
			..Cell::default()
		}
	}

	fn title(number: u16, cells: Vec<Cell>) -> DvdTitle {
		DvdTitle {
			number,
			vts: 1,
			cells,
			..DvdTitle::default()
		}
	}

	fn disc(sectors: u64, titles: Vec<DvdTitle>) -> Dvd {
		Dvd {
			titles,
			vobs: vec![(
				1,
				vec![(
					PathBuf::from("VTS_01_1.VOB"),
					Extent {
						offset: 0,
						length: sectors * SECTOR,
					},
				)],
			)],
		}
	}

	fn kept(title: u16) -> Kept {
		Kept {
			title,
			false_time: false,
		}
	}

	fn first() -> DvdTitle {
		title(1, vec![cell(0, 60 * MBYTE_SECTORS - 1, 60.0)])
	}

	fn second() -> DvdTitle {
		title(
			2,
			vec![
				cell(60 * MBYTE_SECTORS, 90 * MBYTE_SECTORS - 1, 30.0),
				cell(90 * MBYTE_SECTORS, 120 * MBYTE_SECTORS - 1, 30.0),
			],
		)
	}

	fn play_all(number: u16) -> DvdTitle {
		title(number, vec![cell(0, 120 * MBYTE_SECTORS - 1, 120.0)])
	}

	#[test]
	fn two_distinct_titles_are_both_kept_in_order() {
		let selection = select(&disc(200 * u64::from(MBYTE_SECTORS), vec![first(), second()]));

		assert_eq!(selection.kept, [kept(1), kept(2)]);
		assert!(selection.dropped.is_empty());
	}

	#[test]
	fn a_play_all_title_after_its_parts_is_a_duplicate() {
		let selection = select(&disc(
			200 * u64::from(MBYTE_SECTORS),
			vec![first(), second(), play_all(3)],
		));

		assert_eq!(selection.kept, [kept(1), kept(2)]);
		assert_eq!(selection.dropped, [(3, Dropped::Duplicate)]);
	}

	#[test]
	fn a_play_all_title_first_makes_its_parts_duplicates() {
		let mut one = first();
		one.number = 2;
		let mut two = second();
		two.number = 3;

		let selection = select(&disc(200 * u64::from(MBYTE_SECTORS), vec![play_all(1), one, two]));

		assert_eq!(selection.kept, [kept(1)]);
		assert_eq!(selection.dropped, [(2, Dropped::Duplicate), (3, Dropped::Duplicate)]);
	}

	#[test]
	fn a_partly_covered_title_is_kept() {
		let overlapping = title(3, vec![cell(50 * MBYTE_SECTORS, 150 * MBYTE_SECTORS - 1, 100.0)]);

		let selection = select(&disc(200 * u64::from(MBYTE_SECTORS), vec![first(), overlapping]));

		assert_eq!(selection.kept, [kept(1), kept(3)]);
	}

	#[test]
	fn a_five_second_title_is_short() {
		let short = title(2, vec![cell(100, 3000, 5.0)]);

		let selection = select(&disc(200 * u64::from(MBYTE_SECTORS), vec![first(), short]));

		assert_eq!(selection.kept, [kept(1)]);
		assert_eq!(selection.dropped, [(2, Dropped::Short)]);
	}

	#[test]
	fn a_title_with_a_false_time_is_kept_and_flagged() {
		let sectors = 1_800_000_000 / 2048;
		let selection = select(&disc(sectors, vec![title(1, vec![cell(0, 878_905, 28.8)])]));

		assert_eq!(
			selection.kept,
			[Kept {
				title: 1,
				false_time: true
			}]
		);
		assert!(selection.dropped.is_empty());
	}

	#[test]
	fn a_title_past_the_vob_set_is_unreadable() {
		let beyond = title(2, vec![cell(300 * MBYTE_SECTORS, 400 * MBYTE_SECTORS, 100.0)]);

		let selection = select(&disc(200 * u64::from(MBYTE_SECTORS), vec![first(), beyond]));

		assert_eq!(selection.kept, [kept(1)]);
		assert_eq!(selection.dropped, [(2, Dropped::Unreadable)]);
	}

	#[test]
	fn a_title_in_a_set_without_vobs_is_unreadable() {
		let mut elsewhere = first();
		elsewhere.vts = 2;

		let selection = select(&disc(200 * u64::from(MBYTE_SECTORS), vec![elsewhere]));

		assert_eq!(selection.dropped, [(1, Dropped::Unreadable)]);
	}

	#[test]
	fn single_cell_still_loops_are_dropped() {
		let tiny = title(2, vec![cell(10, 40, 30.0)]);
		let mut still = cell(1000, 3000, 0.0);
		still.still = true;
		let held = title(3, vec![still]);

		let selection = select(&disc(200 * u64::from(MBYTE_SECTORS), vec![first(), tiny, held]));

		assert_eq!(selection.kept, [kept(1)]);
		assert_eq!(selection.dropped, [(2, Dropped::Still), (3, Dropped::Still)]);
	}

	#[test]
	fn a_long_single_cell_title_that_holds_its_last_frame_is_kept() {
		let mut held = first().cells[0].clone();
		held.still = true;

		let selection = select(&disc(200 * u64::from(MBYTE_SECTORS), vec![title(1, vec![held])]));

		assert_eq!(selection.kept, [kept(1)]);
	}

	#[test]
	fn byte_ranges_follow_the_extents_across_their_boundaries() {
		let vobs = vec![
			(
				PathBuf::from("a.iso"),
				Extent {
					offset: 10 * SECTOR,
					length: 4 * SECTOR,
				},
			),
			(
				PathBuf::from("a.iso"),
				Extent {
					offset: 20 * SECTOR,
					length: 4 * SECTOR,
				},
			),
			(
				PathBuf::from("b.vob"),
				Extent {
					offset: 0,
					length: 4 * SECTOR,
				},
			),
		];
		let dvd = Dvd {
			titles: Vec::new(),
			vobs: vec![(3, vobs)],
		};

		assert_eq!(
			dvd.byte_ranges(3, 1, 2),
			[(PathBuf::from("a.iso"), 11 * SECTOR, 2 * SECTOR)]
		);
		assert_eq!(
			dvd.byte_ranges(3, 3, 8),
			[
				(PathBuf::from("a.iso"), 13 * SECTOR, SECTOR),
				(PathBuf::from("a.iso"), 20 * SECTOR, 4 * SECTOR),
				(PathBuf::from("b.vob"), 0, SECTOR),
			]
		);
		assert_eq!(
			dvd.byte_ranges(3, 11, 20),
			[(PathBuf::from("b.vob"), 3 * SECTOR, SECTOR)]
		);
		assert!(dvd.byte_ranges(4, 0, 1).is_empty());
		assert_eq!(dvd.vob_sectors(3), 12);
	}

	#[test]
	fn playback_times_honour_the_frame_rate() {
		assert!((playback_time([0x01, 0x02, 0x03, 0x40 | 0x12]) - (3723.0 + 12.0 / 25.0)).abs() < 1e-9);
		assert!((playback_time([0x00, 0x00, 0x28, 0xC0 | 0x15]) - (28.0 + 15.0 / 30.0)).abs() < 1e-9);
		assert!((playback_time([0x00, 0x00, 0x10, 0x05]) - 10.0).abs() < 1e-9);
		assert!(playback_time([0x0A, 0x00, 0x00, 0x40]).abs() < 1e-9);
	}
}
