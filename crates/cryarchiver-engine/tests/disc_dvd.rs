mod support;

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use cryarchiver_engine::disc::SECTOR;
use cryarchiver_engine::disc::dvd::{
	AudioAttr, AudioCoding, Cell, Dropped, Dvd, DvdTitle, Kept, Subpicture, VideoAttr, parse, select,
};
use cryarchiver_engine::disc::udf::{DiscFiles, read_folder, read_image};
use support::ifo::{self, Audio, Pgc, Vts};
use support::udf::{Image, UdfRevision};

const FIRST_VOB_SECTORS: usize = 1536;
const SECOND_VOB_SECTORS: usize = 512;

const PALETTE: [u32; 16] = [
	0x0010_8080,
	0x00EB_8080,
	0x0051_5AF0,
	0x0090_3622,
	0x0029_F06E,
	0x00D2_1092,
	0x006A_CADE,
	0x00A9_A610,
	0x0010_8080,
	0x0080_8080,
	0x0040_8080,
	0x00C0_8080,
	0x0020_8080,
	0x0060_8080,
	0x00A0_8080,
	0x00E0_8080,
];

fn pattern(length: usize, seed: u32) -> Vec<u8> {
	let mut state = seed;
	(0..length)
		.map(|_| {
			state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
			state.to_be_bytes()[1]
		})
		.collect()
}

fn title_set() -> Vts {
	Vts {
		wide: true,
		audio: vec![
			Audio::Ac3 {
				channels: 6,
				language: "en",
			},
			Audio::Lpcm {
				bits: 16,
				channels: 2,
				language: "sl",
			},
		],
		subpictures: vec!["sl"],
		pgcs: vec![
			Pgc {
				cells: vec![
					ifo::Cell::new(0, 399, 20, 0),
					ifo::Cell::new(400, 899, 30, 0),
					ifo::Cell::new(900, 1299, 25, 12),
				],
				programs: vec![1, 2, 3],
				palette: PALETTE,
				..Pgc::default()
			},
			Pgc {
				cells: vec![ifo::Cell::new(1300, 2047, 40, 0)],
				programs: vec![1],
				palette: [0x0080_8080; 16],
				..Pgc::default()
			},
		],
		titles: vec![vec![(1, 1), (1, 2), (1, 3)], vec![(2, 1)]],
	}
}

fn vobs() -> Vec<u8> {
	pattern((FIRST_VOB_SECTORS + SECOND_VOB_SECTORS) * 2048, 1)
}

fn disc(vmg: &[u8], vts: &[u8]) -> Vec<(String, Vec<u8>)> {
	let vobs = vobs();
	let (first, second) = vobs.split_at(FIRST_VOB_SECTORS * 2048);
	vec![
		("VIDEO_TS/VIDEO_TS.IFO".to_owned(), vmg.to_vec()),
		("VIDEO_TS/VIDEO_TS.BUP".to_owned(), vmg.to_vec()),
		("VIDEO_TS/VTS_01_0.IFO".to_owned(), vts.to_vec()),
		("VIDEO_TS/VTS_01_0.BUP".to_owned(), vts.to_vec()),
		("VIDEO_TS/VTS_01_1.VOB".to_owned(), first.to_vec()),
		("VIDEO_TS/VTS_01_2.VOB".to_owned(), second.to_vec()),
	]
}

fn image(dir: &Path, files: Vec<(String, Vec<u8>)>) -> DiscFiles {
	let path = dir.join("dvd.iso");
	Image {
		files,
		dirs: vec!["AUDIO_TS".to_owned()],
		revision: UdfRevision::V102,
	}
	.write(&path);
	read_image(&path).unwrap().expect("a UDF image")
}

fn folder(dir: &Path, files: &[(String, Vec<u8>)]) -> DiscFiles {
	let root = dir.join("disc");
	for (name, bytes) in files {
		let path = root.join(name);
		std::fs::create_dir_all(path.parent().unwrap()).unwrap();
		std::fs::write(path, bytes).unwrap();
	}
	read_folder(&root).unwrap()
}

fn expected_titles() -> Vec<DvdTitle> {
	let video = VideoAttr {
		width: 720,
		height: 576,
		wide: true,
		pal: true,
	};
	let audio = vec![
		AudioAttr {
			coding: AudioCoding::Ac3,
			channels: 6,
			language: Some("en".to_owned()),
			id: Some(0x80),
		},
		AudioAttr {
			coding: AudioCoding::Lpcm { bits: 16 },
			channels: 2,
			language: Some("sl".to_owned()),
			id: Some(0xA1),
		},
	];
	let cell = |first_sector, last_sector, time_s| Cell {
		first_sector,
		last_sector,
		time_s,
		..Cell::default()
	};
	let subpictures = vec![Subpicture {
		language: Some("sl".to_owned()),
		id: Some(0x20),
	}];

	vec![
		DvdTitle {
			number: 1,
			vts: 1,
			cells: vec![
				cell(0, 399, 20.0),
				cell(400, 899, 30.0),
				cell(900, 1299, 25.0 + 12.0 / 25.0),
			],
			chapter_starts_s: vec![0.0, 20.0, 50.0],
			video,
			audio: audio.clone(),
			subpictures: subpictures.clone(),
			palette: PALETTE,
			angles: false,
		},
		DvdTitle {
			number: 2,
			vts: 1,
			cells: vec![cell(1300, 2047, 40.0)],
			chapter_starts_s: vec![0.0],
			video,
			audio,
			subpictures,
			palette: [0x0080_8080; 16],
			angles: false,
		},
	]
}

fn read_ranges(dvd: &Dvd, first_sector: u32, last_sector: u32) -> Vec<u8> {
	let mut bytes = Vec::new();
	for (source, offset, length) in dvd.byte_ranges(1, first_sector, last_sector) {
		let mut chunk = vec![0; usize::try_from(length).unwrap()];
		let mut file = File::open(source).unwrap();
		file.seek(SeekFrom::Start(offset)).unwrap();
		file.read_exact(&mut chunk).unwrap();
		bytes.extend(chunk);
	}
	bytes
}

fn sectors(bytes: &[u8], first: usize, last: usize) -> &[u8] {
	&bytes[first * 2048..(last + 1) * 2048]
}

#[test]
fn an_ifo_set_in_an_image_parses_back_to_its_titles() {
	let dir = tempfile::tempdir().unwrap();
	let files = image(dir.path(), disc(&ifo::vmg(&[(1, 1), (1, 2)]), &title_set().bytes()));

	let dvd = parse(&files).unwrap();

	assert_eq!(dvd.titles, expected_titles());
	assert_eq!(dvd.vob_sectors(1), 2048);
	assert_eq!(
		select(&dvd).kept,
		[
			Kept {
				title: 1,
				false_time: false
			},
			Kept {
				title: 2,
				false_time: false
			}
		]
	);
}

#[test]
fn byte_ranges_read_the_cells_across_extents_and_vob_parts() {
	let dir = tempfile::tempdir().unwrap();
	let files = image(dir.path(), disc(&ifo::vmg(&[(1, 1), (1, 2)]), &title_set().bytes()));
	let dvd = parse(&files).unwrap();
	let vobs = vobs();

	let ranges = dvd.byte_ranges(1, 1300, 2047);
	assert!(ranges.len() > 1, "{ranges:?}");
	assert_eq!(read_ranges(&dvd, 1300, 2047), sectors(&vobs, 1300, 2047));
	assert_eq!(read_ranges(&dvd, 500, 530), sectors(&vobs, 500, 530));
	assert_eq!(read_ranges(&dvd, 0, 0), sectors(&vobs, 0, 0));
	assert_eq!(dvd.byte_ranges(1, 2047, 2047)[0].2, SECTOR);
}

#[test]
fn a_folder_parses_like_an_image() {
	let dir = tempfile::tempdir().unwrap();
	let files = folder(dir.path(), &disc(&ifo::vmg(&[(1, 1), (1, 2)]), &title_set().bytes()));

	let dvd = parse(&files).unwrap();

	assert_eq!(dvd.titles, expected_titles());
	assert_eq!(read_ranges(&dvd, 1500, 1600), sectors(&vobs(), 1500, 1600));
	assert_eq!(dvd.byte_ranges(1, 1500, 1600).len(), 2);
}

#[test]
fn damaged_ifos_fall_back_to_their_backups() {
	let dir = tempfile::tempdir().unwrap();
	let mut files = disc(&ifo::vmg(&[(1, 1), (1, 2)]), &title_set().bytes());
	files[0].1[..12].copy_from_slice(b"DVDVIDEO-XXX");
	files[2].1.truncate(100);

	let dvd = parse(&folder(dir.path(), &files)).unwrap();

	assert_eq!(dvd.titles, expected_titles());
}

#[test]
fn a_title_set_whose_ifos_both_fail_leaves_its_titles_unreadable() {
	let dir = tempfile::tempdir().unwrap();
	let mut files = disc(&ifo::vmg(&[(1, 1), (2, 1)]), &title_set().bytes());
	files.push(("VIDEO_TS/VTS_02_0.IFO".to_owned(), pattern(4096, 2)));
	files.push(("VIDEO_TS/VTS_02_1.VOB".to_owned(), pattern(8192, 3)));

	let dvd = parse(&folder(dir.path(), &files)).unwrap();

	assert_eq!(dvd.titles[1].number, 2);
	assert_eq!(dvd.titles[1].vts, 2);
	assert!(dvd.titles[1].cells.is_empty());
	assert_eq!(select(&dvd).dropped, [(2, Dropped::Unreadable)]);
}

#[test]
fn a_disc_without_a_readable_title_table_is_an_error() {
	let dir = tempfile::tempdir().unwrap();
	let mut files = disc(&ifo::vmg(&[(1, 1)]), &title_set().bytes());
	files.retain(|(name, _)| !name.starts_with("VIDEO_TS/VIDEO_TS"));
	files.push(("VIDEO_TS/VIDEO_TS.IFO".to_owned(), pattern(2048, 4)));

	let reason = parse(&folder(dir.path(), &files)).unwrap_err();

	assert!(reason.contains("VIDEO_TS.IFO"), "{reason}");
	assert!(reason.contains("VIDEO_TS.BUP is missing"), "{reason}");
}

#[test]
fn cells_past_the_vobs_are_left_out() {
	let dir = tempfile::tempdir().unwrap();
	let mut vts = title_set();
	vts.pgcs[0].cells.push(ifo::Cell::new(2048, 3000, 15, 0));
	vts.pgcs[0].programs.push(4);
	vts.titles[0].push((1, 4));

	let dvd = parse(&folder(dir.path(), &disc(&ifo::vmg(&[(1, 1)]), &vts.bytes()))).unwrap();

	assert_eq!(dvd.titles[0].cells, expected_titles()[0].cells);
	assert_eq!(
		dvd.titles[0].chapter_starts_s,
		[0.0, 20.0, 50.0, 50.0 + (25.0 + 12.0 / 25.0)]
	);
}

#[test]
fn only_the_first_angle_of_an_angle_block_plays() {
	let dir = tempfile::tempdir().unwrap();
	let mut vts = title_set();
	let angle = |first, last, mode: u8| ifo::Cell {
		category: mode << 6 | 0x10,
		..ifo::Cell::new(first, last, 30, 0)
	};
	vts.pgcs[0].cells[1] = angle(400, 599, 1);
	vts.pgcs[0].cells.insert(2, angle(600, 799, 2));
	vts.pgcs[0].cells.insert(3, angle(800, 899, 3));
	vts.pgcs[0].programs = vec![1, 2, 5];

	let dvd = parse(&folder(dir.path(), &disc(&ifo::vmg(&[(1, 1)]), &vts.bytes()))).unwrap();

	let firsts: Vec<u32> = dvd.titles[0].cells.iter().map(|cell| cell.first_sector).collect();
	assert_eq!(firsts, [0, 400, 900]);
	assert_eq!(dvd.titles[0].chapter_starts_s, [0.0, 20.0, 50.0]);
	assert!(dvd.titles[0].angles);
}

#[test]
fn the_pgc_stream_controls_give_each_stream_its_id() {
	let dir = tempfile::tempdir().unwrap();
	let mut vts = title_set();
	vts.audio.insert(
		1,
		Audio::Ac3 {
			channels: 2,
			language: "de",
		},
	);
	vts.subpictures.push("hr");
	vts.pgcs[0].audio_control = vec![0x8000, 0, 0x8000 | 2 << 8];
	vts.pgcs[0].subpicture_control = vec![0x8000_0000 | 3 << 16 | 1 << 24, 0x8000_0000 | 5 << 16];

	let dvd = parse(&folder(dir.path(), &disc(&ifo::vmg(&[(1, 1), (1, 2)]), &vts.bytes()))).unwrap();

	let ids = |title: &DvdTitle| {
		(
			title.audio.iter().map(|attr| attr.id).collect::<Vec<_>>(),
			title
				.subpictures
				.iter()
				.map(|subpicture| subpicture.id)
				.collect::<Vec<_>>(),
		)
	};
	assert_eq!(
		ids(&dvd.titles[0]),
		(vec![Some(0x80), None, Some(0xA2)], vec![Some(0x23), Some(0x25)])
	);
	assert_eq!(
		ids(&dvd.titles[1]),
		(vec![Some(0x80), Some(0x81), Some(0xA2)], vec![Some(0x20), Some(0x21)]),
		"a PGC that marks no stream plays each by its index"
	);
}

#[test]
fn a_title_spanning_two_pgcs_joins_their_cells() {
	let dir = tempfile::tempdir().unwrap();
	let mut vts = title_set();
	vts.titles = vec![vec![(1, 1), (1, 3), (2, 1)]];

	let dvd = parse(&folder(dir.path(), &disc(&ifo::vmg(&[(1, 1)]), &vts.bytes()))).unwrap();

	assert_eq!(dvd.titles[0].cells.len(), 4);
	assert_eq!(dvd.titles[0].chapter_starts_s, [0.0, 50.0, 50.0 + (25.0 + 12.0 / 25.0)]);
	assert_eq!(dvd.titles[0].palette, PALETTE);
}

#[test]
fn damaged_ifos_never_panic() {
	let dir = tempfile::tempdir().unwrap();
	let clean = disc(&ifo::vmg(&[(1, 1), (1, 2), (1, 3)]), &title_set().bytes());
	let root = dir.path().join("disc");
	folder(dir.path(), &clean);

	let picks = pattern(4 * 600, 5);
	for (round, pick) in picks.chunks(4).enumerate() {
		let index = 2 * (round % 2);
		let bytes = &clean[index].1;
		let structure: Vec<usize> = (0..bytes.len()).filter(|&at| bytes[at] != 0).collect();
		let mut damaged = bytes.clone();
		let at = structure[usize::from(u16::from_be_bytes([pick[0], pick[1]])) * structure.len() / 65_536];
		damaged[at] = pick[2] ^ pick[3].max(1);
		if round % 7 == 0 {
			damaged.truncate(at);
		}

		for (name, _) in &clean[index..index + 2] {
			std::fs::write(root.join(name), &damaged).unwrap();
		}
		if let Ok(dvd) = parse(&read_folder(&root).unwrap()) {
			select(&dvd);
		}
		for (name, bytes) in &clean[index..index + 2] {
			std::fs::write(root.join(name), bytes).unwrap();
		}
	}
}
