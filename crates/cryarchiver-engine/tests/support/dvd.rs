use std::path::Path;

use super::ifo::{self, Audio, Pgc, Vts};

const SECTOR: usize = 2048;

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

#[derive(Clone, Copy)]
pub struct Footage {
	pub size: &'static str,
	pub seconds: u32,
	pub audio: &'static str,
	pub subtitles: bool,
	/// Each cell's IFO playback time.
	pub cells: &'static [u32],
	pub bit_rate: &'static str,
}

pub const fn footage(seconds: u32, cells: &'static [u32]) -> Footage {
	Footage {
		size: "720x576",
		seconds,
		audio: "ac3",
		subtitles: false,
		cells,
		bit_rate: "4M",
	}
}

/// An MPEG-2 PS like a DVD's VOB, padded to whole sectors.
pub fn vob(dir: &Path, index: usize, footage: Footage) -> Vec<u8> {
	let picture = format!("testsrc2=s={}:r=25:d={},setfield=tff", footage.size, footage.seconds);
	let tone = format!(
		"sine=frequency={}:duration={}:sample_rate=48000",
		440 + 110 * index,
		footage.seconds
	);
	let path = dir.join(format!("title-{index}.vob"));
	let subtitles = footage.subtitles.then(|| super::pgs_box(dir, 1000, 3000));

	let mut args = vec!["-copyts", "-f", "lavfi", "-i", &picture, "-f", "lavfi", "-i", &tone];
	if let Some(subtitles) = &subtitles {
		args.extend(["-fix_sub_duration", "-i", subtitles.to_str().unwrap()]);
	}
	args.extend(["-map", "0:v", "-map", "1:a"]);
	if subtitles.is_some() {
		args.extend(["-map", "2:s", "-c:s", "dvdsub"]);
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

	let mut bytes = std::fs::read(&path).unwrap();
	bytes.resize(bytes.len().next_multiple_of(SECTOR), 0);
	bytes
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
