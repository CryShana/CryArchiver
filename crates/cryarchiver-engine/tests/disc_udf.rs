mod support;

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use cryarchiver_engine::disc::udf::{DiscFile, Extent, is_udf, read_folder, read_image};
use support::pattern;
use support::udf::{Image, UdfRevision};

fn read_back(file: &DiscFile) -> Vec<u8> {
	let mut source = File::open(&file.source).unwrap();
	let mut bytes = Vec::new();
	for extent in &file.extents {
		let mut chunk = vec![0; usize::try_from(extent.length).unwrap()];
		source.seek(SeekFrom::Start(extent.offset)).unwrap();
		source.read_exact(&mut chunk).unwrap();
		bytes.extend(chunk);
	}
	bytes
}

fn written(path: &Path, image: &Image) -> Vec<u8> {
	image.write(path);
	std::fs::read(path).unwrap()
}

#[test]
fn a_dvd_image_lists_its_files_with_exact_sizes_and_extents() {
	let dir = tempfile::tempdir().unwrap();
	let path = dir.path().join("dvd.iso");
	let ifo = pattern(4096, 1);
	let vob = pattern(3 << 20, 2);
	Image {
		files: vec![
			("VIDEO_TS/VIDEO_TS.IFO".to_owned(), ifo.clone()),
			("VIDEO_TS/VTS_01_1.VOB".to_owned(), vob.clone()),
		],
		dirs: vec!["AUDIO_TS".to_owned()],
		revision: UdfRevision::V102,
	}
	.write(&path);

	assert!(is_udf(&mut File::open(&path).unwrap()).unwrap());
	let files = read_image(&path).unwrap().expect("a UDF image");
	assert_eq!(files.files.len(), 2);

	let listed = files.find("video_ts/video_ts.ifo").unwrap();
	assert_eq!(listed.path, "VIDEO_TS/VIDEO_TS.IFO");
	assert_eq!(listed.size, 4096);
	assert_eq!(listed.source, path);
	assert_eq!(read_back(listed), ifo);

	let listed = files.find("/VIDEO_TS/VTS_01_1.VOB").unwrap();
	assert_eq!(listed.size, 3 << 20);
	assert!(listed.extents.len() > 1, "{:?}", listed.extents);
	assert_eq!(read_back(listed), vob);

	assert!(files.dir_exists("VIDEO_TS"));
	assert!(files.dir_exists("audio_ts"));
	assert!(!files.dir_exists("VIDEO_TS/VIDEO_TS.IFO"));
	assert!(files.find("VIDEO_TS/VTS_02_1.VOB").is_none());
}

#[test]
fn a_blu_ray_image_with_a_metadata_partition_lists_its_files_and_folders() {
	let dir = tempfile::tempdir().unwrap();
	let path = dir.path().join("bd.iso");
	let index = pattern(120, 3);
	let stream = pattern(1_500_000, 4);
	let playlists: Vec<(String, Vec<u8>)> = (0..40)
		.map(|n| (format!("BDMV/PLAYLIST/{n:05}.mpls"), pattern(300, n)))
		.collect();
	let mut files = vec![
		("BDMV/index.bdmv".to_owned(), index.clone()),
		("BDMV/STREAM/00000.m2ts".to_owned(), stream.clone()),
	];
	files.extend(playlists.clone());
	Image {
		files,
		dirs: vec!["BDMV/STREAM/SSIF".to_owned(), "CERTIFICATE".to_owned()],
		revision: UdfRevision::V250,
	}
	.write(&path);

	let listed = read_image(&path).unwrap().expect("a UDF image");
	assert_eq!(listed.files.len(), 42);
	assert_eq!(read_back(listed.find("BDMV/index.bdmv").unwrap()), index);
	assert_eq!(read_back(listed.find("bdmv/stream/00000.M2TS").unwrap()), stream);
	for (name, bytes) in &playlists {
		assert_eq!(&read_back(listed.find(name).unwrap()), bytes, "{name}");
	}

	assert!(listed.dir_exists("BDMV/STREAM/SSIF"));
	assert!(listed.dir_exists("bdmv/stream/ssif/"));
	assert!(listed.dir_exists("CERTIFICATE"));
	assert!(!listed.dir_exists("BDMV/STREAM/SSIF/00000.ssif"));
}

#[test]
fn non_ascii_names_round_trip_in_both_revisions() {
	let dir = tempfile::tempdir().unwrap();
	for revision in [UdfRevision::V102, UdfRevision::V250] {
		let path = dir.path().join(format!("{revision:?}.iso"));
		let wide = pattern(5000, 5);
		let narrow = pattern(10, 6);
		Image {
			files: vec![
				("Mapa ž/Čžš.txt".to_owned(), wide.clone()),
				("café.txt".to_owned(), narrow.clone()),
			],
			dirs: Vec::new(),
			revision,
		}
		.write(&path);

		let listed = read_image(&path).unwrap().expect("a UDF image");
		let file = listed.find("mapa Ž/čŽš.TXT").unwrap();
		assert_eq!(file.path, "Mapa ž/Čžš.txt");
		assert_eq!(read_back(file), wide);
		assert_eq!(read_back(listed.find("CAFÉ.txt").unwrap()), narrow);
		assert!(listed.dir_exists("Mapa ž"));
	}
}

#[test]
fn random_bytes_and_zeros_are_not_udf() {
	let dir = tempfile::tempdir().unwrap();
	let noise = dir.path().join("noise.iso");
	std::fs::write(&noise, pattern(1 << 20, 7)).unwrap();
	let zeros = dir.path().join("zeros.iso");
	std::fs::write(&zeros, vec![0; 40 << 10]).unwrap();

	assert_eq!(read_image(&noise).unwrap(), None);
	assert_eq!(read_image(&zeros).unwrap(), None);
	assert!(!is_udf(&mut File::open(&zeros).unwrap()).unwrap());
}

#[test]
fn a_plain_mkv_is_not_udf() {
	let dir = tempfile::tempdir().unwrap();
	let mkv = support::mpeg4(dir.path(), "plain.mkv", 2);

	assert!(!is_udf(&mut File::open(&mkv).unwrap()).unwrap());
	assert_eq!(read_image(&mkv).unwrap(), None);
}

#[test]
fn a_truncated_image_is_not_read() {
	let dir = tempfile::tempdir().unwrap();
	let path = dir.path().join("dvd.iso");
	let bytes = written(
		&path,
		&Image {
			files: vec![("VIDEO_TS/VTS_01_1.VOB".to_owned(), pattern(3 << 20, 8))],
			dirs: Vec::new(),
			revision: UdfRevision::V102,
		},
	);

	std::fs::write(&path, &bytes[..bytes.len() - 4096]).unwrap();
	assert_eq!(read_image(&path).unwrap(), None);
}

#[test]
fn corrupted_images_never_panic() {
	let dir = tempfile::tempdir().unwrap();
	for revision in [UdfRevision::V102, UdfRevision::V250] {
		let path = dir.path().join("damaged.iso");
		let image = Image {
			files: vec![
				("BDMV/index.bdmv".to_owned(), pattern(200, 9)),
				("BDMV/STREAM/00001.m2ts".to_owned(), pattern(1_200_000, 10)),
				("VIDEO_TS/Čžš.IFO".to_owned(), pattern(3000, 11)),
			],
			dirs: vec!["BDMV/STREAM/SSIF".to_owned()],
			revision,
		};
		let clean = written(&path, &image);
		let structure: Vec<usize> = (0..clean.len().min(300 << 10)).filter(|&at| clean[at] != 0).collect();

		let picks = pattern(4 * 400, 12);
		for round in picks.chunks(4) {
			let mut damaged = clean.clone();
			let index = usize::from(u16::from_be_bytes([round[0], round[1]]));
			let at = structure[index * structure.len() / 65_536];
			damaged[at] = round[2] ^ round[3].max(1);

			std::fs::write(&path, &damaged).unwrap();
			read_image(&path).unwrap();
		}

		for length in [0, 2048, 32_768 + 7, 256 * 2048 + 100, clean.len() / 2] {
			std::fs::write(&path, &clean[..length]).unwrap();
			read_image(&path).unwrap();
		}
	}
}

#[test]
fn a_folder_lists_its_nested_files_as_whole_extents() {
	let dir = tempfile::tempdir().unwrap();
	let root = dir.path().join("disc");
	std::fs::create_dir_all(root.join("BDMV/STREAM/SSIF")).unwrap();
	std::fs::create_dir_all(root.join("BDMV/PLAYLIST")).unwrap();
	std::fs::write(root.join("BDMV/index.bdmv"), pattern(100, 13)).unwrap();
	std::fs::write(root.join("BDMV/PLAYLIST/00000.mpls"), pattern(2500, 14)).unwrap();
	std::fs::write(root.join("Čžš.txt"), b"").unwrap();

	let listed = read_folder(&root).unwrap();
	assert_eq!(listed.files.len(), 3);

	let playlist = listed.find("bdmv/playlist/00000.MPLS").unwrap();
	assert_eq!(playlist.path, "BDMV/PLAYLIST/00000.mpls");
	assert_eq!(playlist.size, 2500);
	assert_eq!(
		playlist.extents,
		[Extent {
			offset: 0,
			length: 2500
		}]
	);
	assert_eq!(playlist.source, root.join("BDMV").join("PLAYLIST").join("00000.mpls"));
	assert_eq!(read_back(playlist), pattern(2500, 14));

	assert_eq!(listed.find("Čžš.txt").unwrap().size, 0);
	assert!(listed.dir_exists("BDMV/STREAM/SSIF"));
	assert!(listed.dir_exists("BDMV"));
	assert!(!listed.dir_exists("BDMV/CLIPINF"));
}
