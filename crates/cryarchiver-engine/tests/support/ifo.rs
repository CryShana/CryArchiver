const SECTOR: usize = 2048;
const PGC_HEADER: usize = 0xEC;

/// A cell playback entry: inclusive VTS sectors, a 25 fps time, the still time byte and the category byte (block
/// mode and type in its top four bits).
#[derive(Clone, Copy)]
pub struct Cell {
	pub first: u32,
	pub last: u32,
	pub seconds: u32,
	pub frames: u8,
	pub still: u8,
	pub category: u8,
}

impl Cell {
	pub fn new(first: u32, last: u32, seconds: u32, frames: u8) -> Self {
		Self {
			first,
			last,
			seconds,
			frames,
			still: 0,
			category: 0,
		}
	}
}

/// A program chain: `programs` holds each program's entry cell number; the controls are its audio and subpicture
/// stream control words, none when empty.
#[derive(Clone, Default)]
pub struct Pgc {
	pub cells: Vec<Cell>,
	pub programs: Vec<u8>,
	pub palette: [u32; 16],
	pub audio_control: Vec<u16>,
	pub subpicture_control: Vec<u32>,
}

#[derive(Clone, Copy)]
pub enum Audio {
	Ac3 {
		channels: u8,
		language: &'static str,
	},
	Lpcm {
		bits: u8,
		channels: u8,
		language: &'static str,
	},
}

/// A PAL 720x576 MPEG-2 title set; `titles` holds each VTS title's parts of title as `(PGC number, program number)`.
#[derive(Clone)]
pub struct Vts {
	pub wide: bool,
	pub audio: Vec<Audio>,
	pub subpictures: Vec<&'static str>,
	pub pgcs: Vec<Pgc>,
	pub titles: Vec<Vec<(u16, u16)>>,
}

/// `VIDEO_TS.IFO` listing each title as `(VTS number, VTS title number)`.
pub fn vmg(titles: &[(u8, u8)]) -> Vec<u8> {
	let mut ifo = vec![0; SECTOR];
	ifo[..12].copy_from_slice(b"DVDVIDEO-VMG");
	let sets = titles.iter().map(|&(vts, _)| u16::from(vts)).max().unwrap_or(0);
	put16(&mut ifo, 0x3E, sets);
	put32(&mut ifo, 0xC4, 1);

	let mut table = vec![0; 8];
	put16(&mut table, 0, u16::try_from(titles.len()).unwrap());
	for &(vts, vts_title) in titles {
		let mut entry = [0; 12];
		entry[0] = 0x3C;
		entry[1] = 1;
		entry[6] = vts;
		entry[7] = vts_title;
		table.extend(entry);
	}
	let end = u32::try_from(table.len() - 1).unwrap();
	put32(&mut table, 4, end);

	ifo.extend(table);
	pad(ifo)
}

impl Vts {
	pub fn bytes(&self) -> Vec<u8> {
		let mut ifo = vec![0; SECTOR];
		ifo[..12].copy_from_slice(b"DVDVIDEO-VTS");
		ifo[0x200] = 0x40 | 0x10 | if self.wide { 0x0C } else { 0 };

		put16(&mut ifo, 0x202, u16::try_from(self.audio.len()).unwrap());
		for (index, audio) in self.audio.iter().enumerate() {
			let at = 0x204 + 8 * index;
			let (coding, quantization, channels, language) = match *audio {
				Audio::Ac3 { channels, language } => (0, 0, channels, language),
				Audio::Lpcm {
					bits,
					channels,
					language,
				} => (4, (bits - 16) / 4, channels, language),
			};
			ifo[at] = coding << 5 | 0x04;
			ifo[at + 1] = quantization << 6 | (channels - 1);
			ifo[at + 2..at + 4].copy_from_slice(language.as_bytes());
		}

		put16(&mut ifo, 0x254, u16::try_from(self.subpictures.len()).unwrap());
		for (index, language) in self.subpictures.iter().enumerate() {
			let at = 0x256 + 6 * index;
			if !language.is_empty() {
				ifo[at] = 1;
				ifo[at + 2..at + 4].copy_from_slice(language.as_bytes());
			}
		}

		let ptt = sector_of(&ifo);
		put32(&mut ifo, 0xC8, ptt);
		ifo = pad([ifo, self.ptt_table()].concat());
		let pgcit = sector_of(&ifo);
		put32(&mut ifo, 0xCC, pgcit);
		pad([ifo, self.pgc_table()].concat())
	}

	fn ptt_table(&self) -> Vec<u8> {
		let mut table = vec![0; 8 + 4 * self.titles.len()];
		put16(&mut table, 0, u16::try_from(self.titles.len()).unwrap());
		for (index, parts) in self.titles.iter().enumerate() {
			let start = u32::try_from(table.len()).unwrap();
			put32(&mut table, 8 + 4 * index, start);
			for &(pgcn, pgn) in parts {
				table.extend(pgcn.to_be_bytes());
				table.extend(pgn.to_be_bytes());
			}
		}
		let end = u32::try_from(table.len() - 1).unwrap();
		put32(&mut table, 4, end);
		table
	}

	fn pgc_table(&self) -> Vec<u8> {
		let mut table = vec![0; 8 + 8 * self.pgcs.len()];
		put16(&mut table, 0, u16::try_from(self.pgcs.len()).unwrap());
		for (index, pgc) in self.pgcs.iter().enumerate() {
			let title = self
				.titles
				.iter()
				.position(|parts| parts.first().is_some_and(|&(pgcn, _)| usize::from(pgcn) == index + 1));
			let category = title.map_or(0, |title| 0x8000_0000 | (u32::try_from(title + 1).unwrap() << 24));
			let start = u32::try_from(table.len()).unwrap();
			put32(&mut table, 8 + 8 * index, category);
			put32(&mut table, 8 + 8 * index + 4, start);
			table.extend(pgc.bytes());
		}
		let end = u32::try_from(table.len() - 1).unwrap();
		put32(&mut table, 4, end);
		table
	}
}

impl Pgc {
	fn bytes(&self) -> Vec<u8> {
		let mut pgc = vec![0; PGC_HEADER];
		pgc[2] = u8::try_from(self.programs.len()).unwrap();
		pgc[3] = u8::try_from(self.cells.len()).unwrap();
		let seconds = self.cells.iter().map(|cell| cell.seconds).sum();
		pgc[4..8].copy_from_slice(&time(seconds, 0));
		for (index, entry) in self.palette.iter().enumerate() {
			put32(&mut pgc, 0xA4 + 4 * index, *entry);
		}
		for (index, control) in self.audio_control.iter().enumerate() {
			put16(&mut pgc, 0x0C + 2 * index, *control);
		}
		for (index, control) in self.subpicture_control.iter().enumerate() {
			put32(&mut pgc, 0x1C + 4 * index, *control);
		}

		let program_map = pgc.len();
		pgc.extend(&self.programs);
		pgc.resize(pgc.len().next_multiple_of(2), 0);
		let playback = pgc.len();
		for cell in &self.cells {
			let mut entry = [0; 24];
			entry[0] = cell.category;
			entry[2] = cell.still;
			entry[4..8].copy_from_slice(&time(cell.seconds, cell.frames));
			entry[8..12].copy_from_slice(&cell.first.to_be_bytes());
			entry[20..24].copy_from_slice(&cell.last.to_be_bytes());
			pgc.extend(entry);
		}
		let position = pgc.len();
		for (index, _) in self.cells.iter().enumerate() {
			pgc.extend([0, 1, 0, u8::try_from(index + 1).unwrap()]);
		}

		for (at, offset) in [(0xE6, program_map), (0xE8, playback), (0xEA, position)] {
			put16(&mut pgc, at, u16::try_from(offset).unwrap());
		}
		pgc
	}
}

/// BCD `hh mm ss ff` with the 25 fps flag.
pub fn time(seconds: u32, frames: u8) -> [u8; 4] {
	let bcd = |value: u32| u8::try_from(((value / 10) << 4) | (value % 10)).unwrap();
	[
		bcd(seconds / 3600),
		bcd(seconds / 60 % 60),
		bcd(seconds % 60),
		0x40 | bcd(u32::from(frames)),
	]
}

fn put16(bytes: &mut [u8], at: usize, value: u16) {
	bytes[at..at + 2].copy_from_slice(&value.to_be_bytes());
}

fn put32(bytes: &mut [u8], at: usize, value: u32) {
	bytes[at..at + 4].copy_from_slice(&value.to_be_bytes());
}

fn sector_of(bytes: &[u8]) -> u32 {
	u32::try_from(bytes.len() / SECTOR).unwrap()
}

fn pad(mut bytes: Vec<u8>) -> Vec<u8> {
	bytes.resize(bytes.len().next_multiple_of(SECTOR), 0);
	bytes
}
