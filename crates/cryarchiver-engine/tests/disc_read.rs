mod support;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Command;

use cryarchiver_engine::disc::Reader;
use cryarchiver_engine::disc::bluray::{build_bluray, main_feature};
use cryarchiver_engine::disc::dvd::{Dvd, Selection, parse, select};
use cryarchiver_engine::disc::read::{ANGLES_LEFT_OUT, Built, BuiltTitle, build_dvd, normalise_timestamps};
use cryarchiver_engine::disc::udf::{read_folder, read_image};
use cryarchiver_engine::outcome::{ErrorKind, JobError};
use cryarchiver_engine::probe::{MediaInfo, probe};
use cryarchiver_engine::workspace::Workspace;
use support::bluray::{self, Playlist};
use support::dvd::{self, Footage, footage};
use support::udf::{Image, UdfRevision};
use tokio_util::sync::CancellationToken;

const NAME: &str = "Družinski izlet";

/// A `VIDEO_TS` folder in `root` whose one title set holds `titles` in order, each in its own PGC.
fn disc(dir: &Path, root: &Path, titles: &[Footage]) -> (Dvd, Selection) {
	dvd::write_folder(root, &dvd::video_ts(dir, titles));

	let dvd = parse(&read_folder(root).unwrap(), &mut Reader::default()).unwrap();
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

fn packet_times(path: &Path, stream: &str) -> Vec<f64> {
	let out = Command::new("ffprobe")
		.args([
			"-v",
			"error",
			"-select_streams",
			stream,
			"-show_entries",
			"packet=pts_time",
			"-of",
			"csv=p=0",
		])
		.arg(path)
		.output()
		.unwrap();
	String::from_utf8_lossy(&out.stdout)
		.lines()
		.filter_map(|line| line.trim().trim_end_matches(',').parse().ok())
		.collect()
}

fn first_from(times: &[f64], from: f64) -> f64 {
	*times
		.iter()
		.find(|time| **time >= from)
		.unwrap_or_else(|| panic!("nothing from {from}: {times:?}"))
}

/// The hashes of the first audio track's packets, in order.
fn audio_hashes(path: &Path) -> Vec<String> {
	let out = Command::new("ffmpeg")
		.args(["-v", "error", "-i"])
		.arg(path)
		.args(["-map", "0:a:0", "-c", "copy", "-f", "framemd5", "-"])
		.output()
		.unwrap();
	String::from_utf8_lossy(&out.stdout)
		.lines()
		.filter(|line| !line.starts_with('#'))
		.filter_map(|line| line.rsplit(',').next().map(|hash| hash.trim().to_owned()))
		.collect()
}

fn codecs(info: &MediaInfo) -> Vec<&str> {
	info.streams.iter().map(|stream| stream.codec_name.as_str()).collect()
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
	let second = built.titles[1].start_s;
	let video = first_from(&packet_times(&built.path, "v:0"), second - 0.05);
	let audio = first_from(&packet_times(&built.path, "a:0"), second - 0.05);
	assert!(
		near(video, audio, 0.2),
		"title 2 starts its video at {video} and its audio at {audio}"
	);
	assert!(built.warnings.is_empty(), "{:?}", built.warnings);

	assert_eq!(codecs(&info), ["mpeg2video", "ac3", "dvd_subtitle"]);
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
	assert_eq!(codecs(&info), ["mpeg2video", "pcm_s16le"]);
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

fn main_playlist() -> Playlist {
	Playlist {
		number: 800,
		declared_s: 200,
		marks_s: &[0.0, 3.0, 5.5],
	}
}

/// A Blu-ray folder at `root` with eight seconds of footage and two playlists: the main one and a shorter one.
fn blu_ray_folder(dir: &Path, root: &Path) {
	let shorter = Playlist {
		number: 1,
		declared_s: 190,
		marks_s: &[0.0, 1.0],
	};
	dvd::write_folder(root, &bluray::bdmv(dir, 8, &[shorter, main_playlist()]));
}

#[tokio::test]
async fn a_blu_ray_main_feature_is_copied_with_its_playlists_marks_as_chapters() {
	let dir = tempfile::tempdir().unwrap();
	let root = dir.path().join("Film");
	blu_ray_folder(dir.path(), &root);
	let tools = support::toolchain().await;
	let cancel = CancellationToken::new();
	let workspace = workspace(dir.path());
	let fractions = RefCell::new(Vec::new());

	let main = main_feature(&tools, &root, &read_folder(&root).unwrap(), &cancel)
		.await
		.unwrap()
		.expect("a disc that is not encrypted");
	let built = build_bluray(
		&tools,
		&main,
		NAME,
		&workspace,
		|fraction| fractions.borrow_mut().push(fraction),
		&cancel,
	)
	.await
	.unwrap();

	assert_eq!(main.playlist, Some(800));
	assert!(main.warnings.is_empty(), "{:?}", main.warnings);
	let clip = std::fs::metadata(root.join("BDMV/STREAM/00001.m2ts")).unwrap().len();
	assert_eq!(main.bytes, clip);
	assert!(built.path.starts_with(workspace.dir()), "{}", built.path.display());
	assert_eq!(built.titles.len(), 1);
	assert!(near(built.titles[0].length_s, 8.0, 0.1), "{:?}", built.titles);
	assert!(fractions.into_inner().last().is_some_and(|last| *last > 0.9));

	let info = probed(&built.path).await;
	assert_eq!(info.format.tags.get("title").map(String::as_str), Some(NAME));
	assert_eq!(codecs(&info), ["mpeg2video", "pcm_s24le", "hdmv_pgs_subtitle"]);
	assert_eq!(info.streams[1].tag("language"), Some("slv"));
	assert_eq!(info.streams[2].tag("language"), Some("eng"));

	let chapters = chapter_starts(&info);
	let titles: Vec<&str> = chapters.iter().map(|(_, title)| title.as_str()).collect();
	assert_eq!(titles, ["Chapter 1", "Chapter 2", "Chapter 3"]);
	for ((start, _), expected) in chapters.iter().zip([0.0, 3.0, 5.5]) {
		assert!(near(*start, expected, 0.05), "{chapters:?}");
	}
}

#[tokio::test]
async fn a_blu_ray_playlist_that_cannot_be_read_leaves_the_main_feature_without_chapters() {
	let dir = tempfile::tempdir().unwrap();
	let root = dir.path().join("Film");
	blu_ray_folder(dir.path(), &root);
	let mut files = read_folder(&root).unwrap();
	files.files.retain(|file| !file.path.ends_with("00800.mpls"));
	let tools = support::toolchain().await;
	let cancel = CancellationToken::new();
	let workspace = workspace(dir.path());

	let main = main_feature(&tools, &root, &files, &cancel)
		.await
		.unwrap()
		.expect("a disc that is not encrypted");
	let built = build_bluray(&tools, &main, NAME, &workspace, |_| {}, &cancel)
		.await
		.unwrap();

	assert_eq!(main.playlist, Some(800));
	assert_eq!(
		main.warnings,
		["playlist 00800.mpls can't be read (it is missing), so the main feature has no chapters"]
	);
	let info = probed(&built.path).await;
	assert!(info.chapters.is_empty(), "{:?}", info.chapters);
	assert!(near(info.format.duration.unwrap(), 8.0, 0.1), "{:?}", info.format);
}

#[tokio::test]
async fn a_blu_ray_image_under_a_non_ascii_folder_reads() {
	let dir = tempfile::tempdir().unwrap();
	let iso = dir.path().join("Čžš").join("Film.iso");
	std::fs::create_dir_all(iso.parent().unwrap()).unwrap();
	Image {
		files: bluray::bdmv(dir.path(), 8, &[main_playlist()]),
		dirs: vec!["CERTIFICATE".to_owned()],
		revision: UdfRevision::V250,
	}
	.write_mastered(&iso);
	let tools = support::toolchain().await;
	let cancel = CancellationToken::new();
	let workspace = workspace(dir.path());

	let main = main_feature(&tools, &iso, &read_image(&iso).unwrap().unwrap(), &cancel)
		.await
		.unwrap()
		.expect("a disc that is not encrypted");
	let built = build_bluray(&tools, &main, NAME, &workspace, |_| {}, &cancel)
		.await
		.unwrap();

	assert_eq!(main.playlist, Some(800));
	assert!(main.warnings.is_empty(), "{:?}", main.warnings);
	let info = probed(&built.path).await;
	assert_eq!(chapter_starts(&info).len(), 3, "{:?}", info.chapters);
	assert!(near(info.format.duration.unwrap(), 8.0, 0.1), "{:?}", info.format);
}

#[tokio::test]
async fn a_subtitle_track_that_starts_late_in_its_title_is_still_read() {
	let dir = tempfile::tempdir().unwrap();
	let late = Footage {
		size: "352x288",
		bit_rate: "1M",
		subtitles: true,
		subtitle_ms: (70_000, 72_000),
		..footage(75, &[75])
	};
	let (dvd, selection) = disc(dir.path(), &dir.path().join("disc"), &[late]);
	let workspace = workspace(dir.path());

	let built = build(&dvd, &selection, &workspace).await.unwrap();

	assert!(built.warnings.is_empty(), "{:?}", built.warnings);
	let info = probed(&built.path).await;
	assert_eq!(codecs(&info), ["mpeg2video", "ac3", "dvd_subtitle"]);
	assert_eq!(info.streams[2].tag("language"), Some("deu"));
	let shown = first_from(&packet_times(&built.path, "s:0"), 0.0);
	assert!(near(shown, 70.0, 0.5), "{shown}");
}

#[tokio::test]
async fn a_title_with_a_false_time_and_a_late_subtitle_is_timed_by_its_counted_packets() {
	let dir = tempfile::tempdir().unwrap();
	let late = Footage {
		size: "352x288",
		bit_rate: "2M",
		subtitles: true,
		subtitle_ms: (70_000, 72_000),
		..footage(75, &[1, 2])
	};
	let (dvd, selection) = disc(dir.path(), &dir.path().join("disc"), &[late]);
	assert!(selection.kept[0].false_time);
	let workspace = workspace(dir.path());

	let built = build(&dvd, &selection, &workspace).await.unwrap();

	assert!(built.warnings.is_empty(), "{:?}", built.warnings);
	assert!(near(built.titles[0].length_s, 75.0, 0.2), "{:?}", built.titles);
	let info = probed(&built.path).await;
	assert_eq!(codecs(&info), ["mpeg2video", "ac3", "dvd_subtitle"]);
}

#[tokio::test]
async fn tracks_a_title_has_beyond_the_main_title_are_left_out_with_a_warning() {
	let dir = tempfile::tempdir().unwrap();
	let more = Footage {
		audio_tracks: 2,
		..footage(11, &[11])
	};
	let (dvd, selection) = disc(dir.path(), &dir.path().join("disc"), &[footage(12, &[6, 6]), more]);
	let workspace = workspace(dir.path());

	let built = build(&dvd, &selection, &workspace).await.unwrap();

	assert_eq!(
		built.warnings,
		["Title 2: audio track 2 was left out to match the main title"]
	);
	assert!(built.reencoded.is_empty(), "{:?}", built.reencoded);
	let info = probed(&built.path).await;
	assert_eq!(codecs(&info), ["mpeg2video", "ac3"]);
	assert!(near(info.format.duration.unwrap(), 23.0, 0.1), "{:?}", info.format);
	let pieces: Vec<String> = std::fs::read_dir(workspace.dir())
		.unwrap()
		.map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
		.filter(|name| {
			name.starts_with("title-") && Path::new(name).extension().is_some_and(|extension| extension == "mkv")
		})
		.collect();
	assert!(pieces.is_empty(), "{pieces:?}");
}

#[tokio::test]
async fn only_the_first_angle_of_an_interleaved_block_is_read() {
	let dir = tempfile::tempdir().unwrap();
	let root = dir.path().join("disc");
	dvd::write_folder(&root, &dvd::two_angles(dir.path(), footage(12, &[12]), true));
	let dvd = parse(&read_folder(&root).unwrap(), &mut Reader::default()).unwrap();
	let selection = select(&dvd);
	let workspace = workspace(dir.path());

	let built = build(&dvd, &selection, &workspace).await.unwrap();

	assert!(dvd.titles[0].cells[0].units.len() > 1, "{:?}", dvd.titles[0].cells);
	assert!(!dvd.mixed(&selection));
	assert_eq!(built.warnings, [ANGLES_LEFT_OUT]);
	let angle_one = audio_hashes(&dir.path().join("title-0.vob"));
	assert!(angle_one.len() > 100, "{}", angle_one.len());
	assert_eq!(audio_hashes(&built.path), angle_one);
	let info = probed(&built.path).await;
	assert!(near(info.format.duration.unwrap(), 12.0, 0.1), "{:?}", info.format);
}

#[tokio::test]
async fn an_interleaved_block_whose_units_cannot_be_followed_is_mixed() {
	let dir = tempfile::tempdir().unwrap();
	let root = dir.path().join("disc");
	dvd::write_folder(&root, &dvd::two_angles(dir.path(), footage(12, &[12]), false));

	let dvd = parse(&read_folder(&root).unwrap(), &mut Reader::default()).unwrap();

	assert!(dvd.mixed(&select(&dvd)));
}

#[tokio::test]
async fn an_interleaved_block_that_cannot_be_read_is_an_error_not_mixed() {
	let dir = tempfile::tempdir().unwrap();
	let root = dir.path().join("disc");
	dvd::write_folder(&root, &dvd::two_angles(dir.path(), footage(12, &[12]), true));
	let listing = read_folder(&root).unwrap();
	std::fs::remove_file(root.join("VIDEO_TS").join("VTS_01_1.VOB")).unwrap();

	let reason = parse(&listing, &mut Reader::default()).unwrap_err();

	assert!(reason.contains("can't be read"), "{reason}");
}

async fn rebuilt(source: &Path, workspace: &Workspace) -> (MediaInfo, Vec<String>) {
	let tools = support::toolchain().await;
	let info = probed(source).await;
	let (path, warnings) = normalise_timestamps(&tools, source, &info, workspace, |_| {}, &CancellationToken::new())
		.await
		.unwrap();
	(probed(&path).await, warnings)
}

#[tokio::test]
async fn a_rebuilt_vob_turns_its_dvd_lpcm_into_plain_pcm() {
	let dir = tempfile::tempdir().unwrap();
	let vob = dvd::restarting(
		dir.path(),
		"Loose.vob",
		3,
		&[],
		&["-c:v", "mpeg2video", "-c:a", "pcm_dvd", "-f", "dvd"],
	);
	assert!(codecs(&probed(&vob).await).contains(&"pcm_dvd"));

	let (info, warnings) = rebuilt(&vob, &workspace(dir.path())).await;

	assert_eq!(codecs(&info), ["mpeg2video", "pcm_s16le"]);
	assert!(warnings.is_empty(), "{warnings:?}");
	assert!(near(info.format.duration.unwrap(), 3.0, 0.2), "{:?}", info.format);
}

#[tokio::test]
async fn a_rebuilt_ts_keeps_its_dvb_subtitles() {
	let dir = tempfile::tempdir().unwrap();
	let boxed = support::pgs_box(dir.path(), 200, 800);
	let ts = dvd::restarting(
		dir.path(),
		"Broadcast.ts",
		3,
		&["-fix_sub_duration", "-i", boxed.to_str().unwrap()],
		&[
			"-map",
			"0:v",
			"-map",
			"1:a",
			"-map",
			"2:s",
			"-c:v",
			"mpeg2video",
			"-c:a",
			"mp2",
			"-c:s",
			"dvbsub",
			"-f",
			"mpegts",
		],
	);
	assert!(codecs(&probed(&ts).await).contains(&"dvb_subtitle"));

	let (info, warnings) = rebuilt(&ts, &workspace(dir.path())).await;

	assert_eq!(codecs(&info), ["mpeg2video", "mp2", "dvb_subtitle"]);
	assert!(warnings.is_empty(), "{warnings:?}");
}
