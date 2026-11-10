use std::path::{Path, PathBuf};
use std::sync::Mutex;

use cryarchiver_engine::disc::SECTOR_LEN as SECTOR;

use super::ifo::{self, Audio, Pgc, Vts};

pub const PALETTE: [u32; 16] = [
	0x0010_8080,
	0x00EB_8080,
	0x0051_5AF0,
	0x0090_3622,
	0x0010_8080,
	0x0010_8080,
	0x0010_8080,
	0x0010_8080,
	0x0010_8080,
	0x0010_8080,
	0x0010_8080,
	0x0010_8080,
	0x0010_8080,
	0x0010_8080,
	0x0010_8080,
	0x0010_8080,
];

#[derive(Clone, Copy, PartialEq)]
pub struct Footage {
	pub size: &'static str,
	pub seconds: u32,
	pub audio: &'static str,
	/// The audio tracks the footage holds, each the same tone.
	pub audio_tracks: usize,
	pub subtitles: bool,
	/// When the one subtitle shows, in milliseconds.
	pub subtitle_ms: (u32, u32),
	/// Each cell's IFO playback time.
	pub cells: &'static [u32],
	pub bit_rate: &'static str,
	/// The colour primaries, transfer and matrix its sequence headers name, when they name one.
	pub colour: Option<&'static str>,
}

pub const fn footage(seconds: u32, cells: &'static [u32]) -> Footage {
	Footage {
		size: "720x576",
		seconds,
		audio: "ac3",
		audio_tracks: 1,
		subtitles: false,
		subtitle_ms: (1000, 3000),
		cells,
		bit_rate: "4M",
		colour: None,
	}
}

/// An MPEG-2 PS like a DVD's VOB, padded to whole sectors, also written unpadded to `title-{index}.vob` in `dir`. Its
/// tone's pitch follows `index`. Each one is made once per test binary.
pub fn vob(dir: &Path, index: usize, footage: Footage) -> Vec<u8> {
	static MADE: Mutex<Vec<(usize, Footage, Vec<u8>)>> = Mutex::new(Vec::new());

	let path = dir.join(format!("title-{index}.vob"));
	let footage = Footage { cells: &[], ..footage };
	let made = MADE
		.lock()
		.unwrap()
		.iter()
		.find(|(own, made, _)| *own == index && *made == footage)
		.map(|(_, _, bytes)| bytes.clone());
	let mut bytes = if let Some(bytes) = made {
		std::fs::write(&path, &bytes).unwrap();
		bytes
	} else {
		let bytes = encoded_vob(dir, &path, index, footage);
		MADE.lock().unwrap().push((index, footage, bytes.clone()));
		bytes
	};

	bytes.resize(bytes.len().next_multiple_of(SECTOR), 0);
	bytes
}

fn encoded_vob(dir: &Path, path: &Path, index: usize, footage: Footage) -> Vec<u8> {
	let picture = format!("testsrc2=s={}:r=25:d={},setfield=tff", footage.size, footage.seconds);
	let tone = format!(
		"sine=frequency={}:duration={}:sample_rate=48000",
		440 + 110 * index,
		footage.seconds
	);
	let (from_ms, to_ms) = footage.subtitle_ms;
	let subtitles = footage.subtitles.then(|| super::pgs_box(dir, from_ms, to_ms));

	let mut args = vec!["-copyts", "-f", "lavfi", "-i", &picture, "-f", "lavfi", "-i", &tone];
	if let Some(subtitles) = &subtitles {
		args.extend(["-fix_sub_duration", "-i", subtitles.to_str().unwrap()]);
	}
	args.extend(["-map", "0:v"]);
	for _ in 0..footage.audio_tracks {
		args.extend(["-map", "1:a"]);
	}
	if subtitles.is_some() {
		args.extend(["-map", "2:s", "-c:s", "dvdsub"]);
	}
	if let Some(colour) = footage.colour {
		args.extend(["-color_primaries", colour, "-color_trc", colour, "-colorspace", colour]);
	}
	args.extend([
		"-c:v",
		"mpeg2video",
		"-b:v",
		footage.bit_rate,
		"-minrate",
		footage.bit_rate,
		"-maxrate",
		footage.bit_rate,
		"-bufsize",
		"1835k",
		"-flags",
		"+ilme+ildct",
		"-aspect",
		"16:9",
		"-c:a",
		footage.audio,
		"-f",
		"dvd",
		path.to_str().unwrap(),
	]);
	super::ffmpeg(&args);
	std::fs::read(path).unwrap()
}

/// The files of a `VIDEO_TS` folder, as paths from the disc root, whose one title set holds `titles` in order, each
/// in its own PGC. The footage is generated in `dir`.
pub fn video_ts(dir: &Path, titles: &[Footage]) -> Vec<(String, Vec<u8>)> {
	let mut vobs = Vec::new();
	let mut pgcs = Vec::new();
	for (index, title) in titles.iter().enumerate() {
		let bytes = vob(dir, index, *title);
		let first = vobs.len() / SECTOR;
		let sectors = bytes.len() / SECTOR;
		vobs.extend(bytes);

		let total: u32 = title.cells.iter().sum();
		let mut cells = Vec::new();
		let mut elapsed = 0;
		for &seconds in title.cells {
			let from = first + sectors * elapsed as usize / total as usize;
			elapsed += seconds;
			let to = first + sectors * elapsed as usize / total as usize - 1;
			cells.push(ifo::Cell::new(
				u32::try_from(from).unwrap(),
				u32::try_from(to).unwrap(),
				seconds,
				0,
			));
		}
		pgcs.push(Pgc {
			programs: (1..=u8::try_from(cells.len()).unwrap()).collect(),
			cells,
			palette: PALETTE,
			..Pgc::default()
		});
	}

	let audio = match titles[0].audio {
		"pcm_dvd" => Audio::Lpcm {
			bits: 16,
			channels: 1,
			language: "sl",
		},
		_ => Audio::Ac3 {
			channels: 1,
			language: "en",
		},
	};
	let vts = Vts {
		wide: true,
		audio: vec![audio],
		subpictures: if titles[0].subtitles { vec!["de"] } else { Vec::new() },
		titles: pgcs
			.iter()
			.enumerate()
			.map(|(index, pgc)| {
				let number = u16::try_from(index + 1).unwrap();
				(1..=u16::try_from(pgc.programs.len()).unwrap())
					.map(|program| (number, program))
					.collect()
			})
			.collect(),
		pgcs,
	};
	let vmg: Vec<(u8, u8)> = (1..=u8::try_from(titles.len()).unwrap())
		.map(|title| (1, title))
		.collect();

	vec![
		("VIDEO_TS/VIDEO_TS.IFO".to_owned(), ifo::vmg(&vmg)),
		("VIDEO_TS/VTS_01_0.IFO".to_owned(), vts.bytes()),
		("VIDEO_TS/VTS_01_1.VOB".to_owned(), vobs),
	]
}

/// `files` written under `root`.
pub fn write_folder(root: &Path, files: &[(String, Vec<u8>)]) {
	for (path, bytes) in files {
		let path = root.join(path);
		std::fs::create_dir_all(path.parent().unwrap()).unwrap();
		std::fs::write(path, bytes).unwrap();
	}
}

/// The sectors of `vob` that are nav packs: each starts a VOBU.
pub fn nav_packs(vob: &[u8]) -> Vec<usize> {
	(0..vob.len() / SECTOR)
		.filter(|&sector| {
			let bytes = &vob[sector * SECTOR..(sector + 1) * SECTOR];
			bytes[38..42] == [0, 0, 1, 0xBF] && bytes[1024..1028] == [0, 0, 1, 0xBF]
		})
		.collect()
}

/// A `VIDEO_TS` folder whose one title is a two-angle block of `footage`: the angles' VOBUs are interleaved one by
/// one, and when `linked`, each unit's nav pack names the next unit of its angle. Angle 1 is `title-0.vob` in `dir`,
/// angle 2, of another tone, `title-1.vob`.
pub fn two_angles(dir: &Path, footage: Footage, linked: bool) -> Vec<(String, Vec<u8>)> {
	let angles = [vob(dir, 0, footage), vob(dir, 1, footage)];
	let units: Vec<Vec<(usize, usize)>> = angles
		.iter()
		.map(|angle| {
			let starts = nav_packs(angle);
			let ends = starts.iter().skip(1).copied().chain([angle.len() / SECTOR]);
			starts.iter().copied().zip(ends).collect()
		})
		.collect();

	let mut interleaved = Vec::new();
	let mut placed: [Vec<(usize, usize)>; 2] = [Vec::new(), Vec::new()];
	for index in 0..units[0].len().max(units[1].len()) {
		for angle in 0..2 {
			let Some(&(from, to)) = units[angle].get(index) else {
				continue;
			};
			let start = interleaved.len() / SECTOR;
			interleaved.extend(&angles[angle][from * SECTOR..to * SECTOR]);
			placed[angle].push((start, start + to - from - 1));
		}
	}

	if linked {
		for angle in &placed {
			for (index, &(start, end)) in angle.iter().enumerate() {
				let next = angle.get(index + 1).map_or(0x7FFF_FFFF, |&(next, _)| next - start);
				let dsi = start * SECTOR + 1031;
				let put = |bytes: &mut Vec<u8>, at: usize, value: &[u8]| {
					bytes[dsi + at..dsi + at + value.len()].copy_from_slice(value);
				};
				put(&mut interleaved, 4, &u32::try_from(start).unwrap().to_be_bytes());
				put(&mut interleaved, 32, &0x7000_u16.to_be_bytes());
				put(&mut interleaved, 34, &u32::try_from(end - start).unwrap().to_be_bytes());
				put(&mut interleaved, 38, &u32::try_from(next).unwrap().to_be_bytes());
			}
		}
	}

	let cell = |angle: usize, mode: u8| {
		let first = placed[angle].first().unwrap().0;
		let last = placed[angle].last().unwrap().1;
		ifo::Cell {
			category: mode << 6 | 0x10 | 0x04,
			..ifo::Cell::new(
				u32::try_from(first).unwrap(),
				u32::try_from(last).unwrap(),
				footage.seconds,
				0,
			)
		}
	};
	let vts = Vts {
		wide: true,
		audio: vec![Audio::Ac3 {
			channels: 1,
			language: "en",
		}],
		subpictures: Vec::new(),
		pgcs: vec![Pgc {
			cells: vec![cell(0, 1), cell(1, 3)],
			programs: vec![1],
			palette: PALETTE,
			..Pgc::default()
		}],
		titles: vec![vec![(1, 1)]],
	};

	vec![
		("VIDEO_TS/VIDEO_TS.IFO".to_owned(), ifo::vmg(&[(1, 1)])),
		("VIDEO_TS/VTS_01_0.IFO".to_owned(), vts.bytes()),
		("VIDEO_TS/VTS_01_1.VOB".to_owned(), interleaved),
	]
}

/// `vob` with the scrambling bits of its video packets set, as a CSS-encrypted disc has them.
pub fn scrambled(mut vob: Vec<u8>) -> Vec<u8> {
	for sector in vob.as_chunks_mut::<SECTOR>().0 {
		if sector[..4] != [0, 0, 1, 0xBA] {
			continue;
		}

		let mut at = 14 + usize::from(sector[13] & 7);
		while at + 7 <= SECTOR && sector[at..at + 3] == [0, 0, 1] {
			if (0xE0..=0xEF).contains(&sector[at + 3]) {
				sector[at + 6] |= 0x10;
			}
			at += 6 + usize::from(u16::from_be_bytes([sector[at + 4], sector[at + 5]]));
		}
	}
	vob
}

/// A stream of `segments` one-second pieces joined byte for byte, each with its own timestamps from zero: a test
/// picture, a tone, and `extra` inputs, written with `args`.
pub fn restarting(dir: &Path, name: &str, segments: u32, extra: &[&str], args: &[&str]) -> PathBuf {
	let mut bytes = Vec::new();
	for index in 0..segments {
		let picture = "testsrc2=s=320x240:r=25:d=1";
		let tone = format!("sine=frequency={}:duration=1:sample_rate=48000", 440 + 20 * index);
		let segment = dir.join(format!("segment-{index}"));
		let mut command = vec!["-f", "lavfi", "-i", picture, "-f", "lavfi", "-i", &tone];
		command.extend(extra);
		command.extend(args);
		command.push(segment.to_str().unwrap());
		super::ffmpeg(&command);
		bytes.extend(std::fs::read(&segment).unwrap());
		std::fs::remove_file(&segment).unwrap();
	}

	std::fs::create_dir_all(dir.join("restarting")).unwrap();
	let path = dir.join("restarting").join(name);
	std::fs::write(&path, bytes).unwrap();
	path
}
