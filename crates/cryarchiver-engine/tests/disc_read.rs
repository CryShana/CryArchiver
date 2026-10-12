mod support;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Command;

use cryarchiver_engine::disc::dvd::{Dvd, Selection, parse, select};
use cryarchiver_engine::disc::read::{Built, BuiltTitle, build_dvd};
use cryarchiver_engine::disc::udf::read_folder;
use cryarchiver_engine::outcome::{ErrorKind, JobError};
use cryarchiver_engine::probe::{MediaInfo, probe};
use cryarchiver_engine::workspace::Workspace;
use support::ifo::{self, Audio, Pgc, Vts};
use tokio_util::sync::CancellationToken;

const SECTOR: usize = 2048;
const NAME: &str = "Družinski izlet";

const PALETTE: [u32; 16] = [
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
struct Footage {
	size: &'static str,
	seconds: u32,
	audio: &'static str,
	subtitles: bool,
	/// Each cell's IFO playback time.
	cells: &'static [u32],
	bit_rate: &'static str,
}

const fn footage(seconds: u32, cells: &'static [u32]) -> Footage {
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
fn vob(dir: &Path, index: usize, footage: Footage) -> Vec<u8> {
	let picture = format!("testsrc2=s={}:r=25:d={},setfield=tff", footage.size, footage.seconds);
	let tone = format!(
		"sine=frequency={}:duration={}:sample_rate=48000",
		440 + 110 * index,
		footage.seconds
	);
	let path = dir.join(format!("title-{index}.vob"));
	let subtitles = footage.subtitles.then(|| support::pgs_box(dir, 1000, 3000));

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
	support::ffmpeg(&args);

	let mut bytes = std::fs::read(&path).unwrap();
	bytes.resize(bytes.len().next_multiple_of(SECTOR), 0);
	bytes
}

/// A `VIDEO_TS` folder in `root` whose one title set holds `titles` in order, each in its own PGC.
fn disc(dir: &Path, root: &Path, titles: &[Footage]) -> (Dvd, Selection) {
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

	let video_ts = root.join("VIDEO_TS");
	std::fs::create_dir_all(&video_ts).unwrap();
	std::fs::write(video_ts.join("VIDEO_TS.IFO"), ifo::vmg(&vmg)).unwrap();
	std::fs::write(video_ts.join("VTS_01_0.IFO"), vts.bytes()).unwrap();
	std::fs::write(video_ts.join("VTS_01_1.VOB"), vobs).unwrap();

	let dvd = parse(&read_folder(root).unwrap()).unwrap();
	let selection = select(&dvd);
	assert_eq!(selection.kept.len(), titles.len(), "{selection:?}");
	(dvd, selection)
}

async fn build(dvd: &Dvd, selection: &Selection, workspace: &Workspace) -> Result<Built, JobError> {
	let tools = support::toolchain().await;
	build_dvd(
		&tools,
		dvd,
		selection,
		NAME,
		workspace,
		|_| {},
		&CancellationToken::new(),
	)
	.await
}

async fn probed(path: &Path) -> MediaInfo {
	probe(&support::toolchain().await, path, &CancellationToken::new())
		.await
		.unwrap()
}

fn workspace(dir: &Path) -> Workspace {
	Workspace::create(&dir.join("work"), &dir.join("disc")).unwrap()
}

fn near(actual: f64, expected: f64, tolerance: f64) -> bool {
	(actual - expected).abs() <= tolerance
}

fn chapter_starts(info: &MediaInfo) -> Vec<(f64, String)> {
	info.chapters
		.iter()
		.map(|chapter| {
			(
				chapter["start_time"].as_str().unwrap().parse().unwrap(),
				chapter["tags"]["title"].as_str().unwrap_or_default().to_owned(),
			)
		})
		.collect()
}

fn stream_durations(info: &MediaInfo) -> Vec<f64> {
	info.streams
		.iter()
		.filter(|stream| stream.is_video() || stream.is_audio())
		.map(|stream| stream.declared_duration().unwrap())
		.collect()
}

fn frame_sizes(path: &PathBuf) -> Vec<String> {
	let out = Command::new("ffprobe")
		.args([
			"-v",
			"error",
			"-select_streams",
			"v:0",
			"-show_entries",
			"frame=width,height",
			"-of",
			"csv=p=0",
		])
		.arg(path)
		.output()
		.unwrap();
	String::from_utf8_lossy(&out.stdout)
		.lines()
		.map(|line| line.trim().trim_end_matches(',').to_owned())
		.collect()
}

#[tokio::test]
async fn two_titles_of_one_layout_join_into_one_mkv_with_their_chapters() {
	let dir = tempfile::tempdir().unwrap();
	let first = Footage {
		subtitles: true,
		..footage(12, &[6, 6])
	};
	let second = Footage {
		subtitles: true,
		..footage(11, &[11])
	};
	let (dvd, selection) = disc(dir.path(), &dir.path().join("disc"), &[first, second]);
	let workspace = workspace(dir.path());

	let built = build(&dvd, &selection, &workspace).await.unwrap();

	assert!(built.path.starts_with(workspace.dir()), "{}", built.path.display());
	assert!(built.reencoded.is_empty());
	assert_eq!(built.titles.len(), 2);
	assert_eq!(built.titles[0].title, 1);
	assert_eq!(built.titles[1].title, 2);
	assert!(near(built.titles[1].start_s, 12.0, 0.1), "{:?}", built.titles);
	assert!(near(built.titles[1].length_s, 11.0, 0.1), "{:?}", built.titles);

	let info = probed(&built.path).await;
	assert!(near(info.format.duration.unwrap(), 23.0, 0.1), "{:?}", info.format);
	assert_eq!(info.format.tags.get("title").map(String::as_str), Some(NAME));

	let chapters = chapter_starts(&info);
	let titles: Vec<&str> = chapters.iter().map(|(_, title)| title.as_str()).collect();
	assert_eq!(titles, ["Title 1", "Chapter 2", "Title 2"]);
	for ((start, _), expected) in chapters.iter().zip([0.0, 6.0, 12.0]) {
		assert!(near(*start, expected, 0.1), "{chapters:?}");
	}

	let durations = stream_durations(&info);
	assert_eq!(durations.len(), 2, "{:?}", info.streams);
	assert!(near(durations[0], durations[1], 0.1), "{durations:?}");

	let codecs: Vec<&str> = info.streams.iter().map(|stream| stream.codec_name.as_str()).collect();
	assert_eq!(codecs, ["mpeg2video", "ac3", "dvd_subtitle"]);
	assert_eq!(info.streams[1].tag("language"), Some("eng"));
	assert_eq!(info.streams[2].tag("language"), Some("deu"));

	let bytes = std::fs::read(&built.path).unwrap();
	let palette = b"palette: 000000, ffffff";
	assert!(bytes.windows(palette.len()).any(|window| window == palette));
}

#[tokio::test]
async fn dvd_lpcm_becomes_plain_pcm() {
	let dir = tempfile::tempdir().unwrap();
	let lpcm = Footage {
		audio: "pcm_dvd",
		..footage(11, &[11])
	};
	let (dvd, selection) = disc(dir.path(), &dir.path().join("disc"), &[lpcm]);
	let workspace = workspace(dir.path());

	let built = build(&dvd, &selection, &workspace).await.unwrap();

	let info = probed(&built.path).await;
	let codecs: Vec<&str> = info.streams.iter().map(|stream| stream.codec_name.as_str()).collect();
	assert_eq!(codecs, ["mpeg2video", "pcm_s16le"]);
	assert_eq!(info.streams[1].tag("language"), Some("slv"));
	assert!(near(info.format.duration.unwrap(), 11.0, 0.1), "{:?}", info.format);
}

#[tokio::test]
async fn a_title_of_another_shape_is_reencoded_to_the_main_one() {
	let dir = tempfile::tempdir().unwrap();
	let small = Footage {
		size: "352x288",
		..footage(11, &[11])
	};
	let (dvd, selection) = disc(dir.path(), &dir.path().join("disc"), &[footage(12, &[6, 6]), small]);
	let workspace = workspace(dir.path());
	let fractions = RefCell::new(Vec::new());

	let built = build_dvd(
		&support::toolchain().await,
		&dvd,
		&selection,
		NAME,
		&workspace,
		|fraction| fractions.borrow_mut().push(fraction),
		&CancellationToken::new(),
	)
	.await
	.unwrap();

	let fractions = fractions.into_inner();
	assert!(
		fractions.windows(2).all(|pair| pair[0] <= pair[1] + 1e-9),
		"{fractions:?}"
	);
	assert!(
		fractions.iter().all(|fraction| (0.0..=1.0).contains(fraction)),
		"{fractions:?}"
	);
	assert!(fractions.last().is_some_and(|last| *last > 0.9), "{fractions:?}");
	assert_eq!(built.reencoded, [2]);
	assert_eq!(built.titles.iter().map(|title| title.title).collect::<Vec<_>>(), [1, 2]);

	let info = probed(&built.path).await;
	assert!(near(info.format.duration.unwrap(), 23.0, 0.1), "{:?}", info.format);
	let chapters = chapter_starts(&info);
	assert_eq!(chapters.len(), 3, "{chapters:?}");
	assert!(near(chapters[2].0, 12.0, 0.1), "{chapters:?}");
	assert!(near(built.titles[1].start_s, chapters[2].0, 1e-3), "{:?}", built.titles);

	let durations = stream_durations(&info);
	assert!(near(durations[0], durations[1], 0.1), "{durations:?}");

	let sizes = frame_sizes(&built.path);
	assert!(sizes.len() > 500, "{}", sizes.len());
	assert!(
		sizes.iter().all(|size| size == "720,576"),
		"{:?}",
		sizes.iter().find(|size| *size != "720,576")
	);
}

#[tokio::test]
async fn a_disc_under_a_non_ascii_folder_reads() {
	let dir = tempfile::tempdir().unwrap();
	let root = dir.path().join("Čžš").join("disc");
	let (dvd, selection) = disc(dir.path(), &root, &[footage(11, &[5, 6])]);
	let workspace = workspace(dir.path());

	let built = build(&dvd, &selection, &workspace).await.unwrap();

	assert_eq!(
		built.titles,
		[BuiltTitle {
			title: 1,
			start_s: 0.0,
			length_s: 11.0
		}]
	);
	let info = probed(&built.path).await;
	assert!(near(info.format.duration.unwrap(), 11.0, 0.1), "{:?}", info.format);
	assert_eq!(chapter_starts(&info).len(), 2);
}

#[tokio::test]
async fn a_cancelled_build_stops() {
	let dir = tempfile::tempdir().unwrap();
	let (dvd, selection) = disc(dir.path(), &dir.path().join("disc"), &[footage(11, &[11])]);
	let workspace = workspace(dir.path());
	let cancel = CancellationToken::new();
	cancel.cancel();

	let err = build_dvd(
		&support::toolchain().await,
		&dvd,
		&selection,
		NAME,
		&workspace,
		|_| {},
		&cancel,
	)
	.await
	.unwrap_err();

	assert_eq!(err.kind, ErrorKind::Cancelled);
}

#[tokio::test]
async fn a_title_with_a_false_time_is_timed_by_its_footage() {
	let dir = tempfile::tempdir().unwrap();
	let dense = Footage {
		bit_rate: "9M",
		..footage(12, &[1, 2])
	};
	let (dvd, selection) = disc(dir.path(), &dir.path().join("disc"), &[dense]);
	assert!(selection.kept[0].false_time);
	let workspace = workspace(dir.path());

	let built = build(&dvd, &selection, &workspace).await.unwrap();

	assert!(near(built.titles[0].length_s, 12.0, 0.1), "{:?}", built.titles);
	let info = probed(&built.path).await;
	assert!(near(info.format.duration.unwrap(), 12.0, 0.1), "{:?}", info.format);
	let chapters = chapter_starts(&info);
	assert_eq!(chapters.len(), 1, "{chapters:?}");
	assert!(near(
		info.chapters[0]["end_time"].as_str().unwrap().parse().unwrap(),
		12.0,
		0.1
	));
}
