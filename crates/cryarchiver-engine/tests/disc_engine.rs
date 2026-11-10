mod support;

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use cryarchiver_engine::decision::Decision;
use cryarchiver_engine::engine::{Engine, Job, JobEvent, Stage};
use cryarchiver_engine::format::OutputFormat;
use cryarchiver_engine::options::{BluRay, JobOptions};
use cryarchiver_engine::outcome::{ErrorKind, JobOutcome, KeepReason};
use cryarchiver_engine::preset::{Overrides, Speed};
use cryarchiver_engine::probe::MediaInfo;
use cryarchiver_engine::sink::CliSink;
use cryarchiver_engine::workspace::CLI_WORK_DIR;
use support::bluray::{self, Playlist};
use support::dvd::{self, Footage, footage};
use support::udf::{Image, RECORDED, UdfRevision};
use support::{chapter_titles, near, probed};
use tokio_util::sync::CancellationToken;

const THREE_D: &str = "3D disc: the second view can't be encoded; kept so 3D survives";

fn job(source: &Path) -> Job {
	Job {
		source: source.to_path_buf(),
		speed: Speed::Fast,
		keep_metadata: false,
		drop_aux: false,
		min_savings: 0.10,
		overrides: Overrides::default(),
		options: JobOptions::default(),
	}
}

async fn run_with_events(job: &Job, out_dir: &Path) -> (JobOutcome, Vec<JobEvent>) {
	let engine = Engine::new(support::toolchain().await);
	let sink = CliSink {
		output_dir: Some(out_dir.to_path_buf()),
		output_file: None,
		overwrite: false,
	};
	let events = Mutex::new(Vec::new());

	let outcome = engine
		.run(
			job,
			&sink,
			&|event| events.lock().unwrap().push(event),
			&CancellationToken::new(),
		)
		.await;
	(outcome, events.into_inner().unwrap())
}

async fn run(job: &Job, out_dir: &Path) -> JobOutcome {
	run_with_events(job, out_dir).await.0
}

fn keeping_metadata(source: &Path) -> Job {
	Job {
		keep_metadata: true,
		..job(source)
	}
}

fn source_metadata(output: &Path, dir: &Path) -> serde_json::Value {
	serde_json::from_slice(&support::attachment(output, "source-metadata.json", dir)).unwrap()
}

/// The output's container date, to the second.
fn recorded(info: &MediaInfo) -> Option<&str> {
	info.format
		.tags
		.get("creation_time")
		.map(|date| date.get(..19).unwrap_or(date))
}

fn image(path: &Path, files: Vec<(String, Vec<u8>)>, dirs: &[&str], revision: UdfRevision) -> PathBuf {
	std::fs::create_dir_all(path.parent().unwrap()).unwrap();
	Image {
		files,
		dirs: dirs.iter().map(|dir| (*dir).to_owned()).collect(),
		revision,
	}
	.write(path);
	path.to_path_buf()
}

/// A DVD image at `path` whose one title set holds `titles`.
fn dvd_image(dir: &Path, path: &Path, titles: &[Footage]) -> PathBuf {
	image(path, dvd::video_ts(dir, titles), &["AUDIO_TS"], UdfRevision::V102)
}

fn blu_ray_image(path: &Path, three_d: bool) -> PathBuf {
	let files = vec![
		("BDMV/index.bdmv".to_owned(), b"INDX0200".repeat(16)),
		("BDMV/MovieObject.bdmv".to_owned(), b"MOBJ0200".repeat(16)),
		("BDMV/PLAYLIST/00000.mpls".to_owned(), b"MPLS0200".repeat(16)),
	];
	let mut dirs = vec!["BDMV/STREAM", "CERTIFICATE"];
	if three_d {
		dirs.push("BDMV/STREAM/SSIF");
	}
	image(path, files, &dirs, UdfRevision::V250)
}

fn stage_at(events: &[JobEvent], stage: Stage) -> Vec<usize> {
	events
		.iter()
		.enumerate()
		.filter(|(_, event)| **event == JobEvent::Stage(stage))
		.map(|(index, _)| index)
		.collect()
}

fn kept_reason(outcome: &JobOutcome) -> &str {
	match outcome {
		JobOutcome::KeptOriginal {
			reason: KeepReason::Unsupported(reason),
		} => reason,
		outcome => panic!("{outcome:?}"),
	}
}

#[tokio::test]
async fn a_two_title_dvd_image_becomes_one_av1_mkv_with_a_chapter_per_title() {
	let dir = tempfile::tempdir().unwrap();
	let iso = dvd_image(
		dir.path(),
		&dir.path().join("Film.iso"),
		&[footage(12, &[6, 6]), footage(11, &[11])],
	);
	let iso_bytes = std::fs::metadata(&iso).unwrap().len();

	let (outcome, events) = run_with_events(&keeping_metadata(&iso), dir.path()).await;

	let JobOutcome::Encoded {
		output,
		src_bytes,
		out_bytes,
		decision,
		..
	} = outcome
	else {
		panic!("{outcome:?}")
	};
	assert_eq!(output, dir.path().join("Film.cry.mkv"));
	assert_eq!(src_bytes, iso_bytes);
	assert_eq!(decision.source_bytes, Some(iso_bytes));
	assert!(out_bytes < iso_bytes / 2);

	let disc = decision.disc.as_ref().expect("a disc record");
	assert_eq!(disc.kind, "DVD");
	assert_eq!(disc.kept.len(), 2, "{disc:?}");
	assert_eq!(disc.kept[0].0, "Title 1");
	assert!(near(disc.kept[1].1, 11.0, 0.1), "{disc:?}");
	assert!(disc.dropped.is_empty() && disc.false_time.is_empty() && disc.reencoded.is_empty());

	let info = probed(&output).await;
	assert_eq!(info.streams[0].codec_name, "av1");
	assert!(near(info.format.duration.unwrap(), 23.0, 0.2), "{:?}", info.format);
	let chapters = chapter_titles(&info);
	let title_two = chapters
		.iter()
		.find(|(_, title)| title == "Title 2")
		.unwrap_or_else(|| panic!("{chapters:?}"));
	assert!(near(title_two.0, 12.0, 0.2), "{chapters:?}");
	assert_eq!(chapters[0].1, "Title 1");
	assert_eq!(recorded(&info), Some(&RECORDED[..19]), "{:?}", info.format.tags);

	let metadata = source_metadata(&output, dir.path());
	let format = &metadata["ffprobe"]["format"];
	assert_eq!(format["filename"], iso.display().to_string());
	assert_eq!(format["size"], iso_bytes.to_string());
	assert_eq!(metadata["disc"]["kind"], "DVD");
	assert_eq!(metadata["disc"]["kept"][1][0], "Title 2");
	assert_eq!(metadata["disc"]["warnings"], serde_json::json!([]));

	let queued = stage_at(&events, Stage::Queued);
	let read = stage_at(&events, Stage::Read);
	assert_eq!(queued.len(), 1, "{events:?}");
	assert_eq!(read.len(), 1, "{events:?}");
	assert!(queued[0] < read[0]);
	assert!(stage_at(&events, Stage::Encode)[0] > read[0]);
	assert!(!dir.path().join(CLI_WORK_DIR).exists());
}

#[tokio::test]
async fn a_dvd_whose_encode_is_kept_back_is_written_as_read() {
	let dir = tempfile::tempdir().unwrap();
	let iso = dvd_image(
		dir.path(),
		&dir.path().join("Film.iso"),
		&[footage(12, &[6, 6]), footage(11, &[11])],
	);
	let unreachable = |write_kept_discs| Job {
		min_savings: 0.999,
		options: JobOptions {
			write_kept_discs,
			..JobOptions::default()
		},
		..job(&iso)
	};

	let left = run(&unreachable(false), dir.path()).await;
	assert!(matches!(left, JobOutcome::KeptOriginal { .. }), "{left:?}");
	assert!(!dir.path().join("Film.cry.mkv").exists());

	let outcome = run(&unreachable(true), dir.path()).await;

	let JobOutcome::Encoded { output, warnings, .. } = outcome else {
		panic!("{outcome:?}")
	};
	assert_eq!(output, dir.path().join("Film.cry.mkv"));
	assert!(
		warnings
			.iter()
			.any(|warning| warning.starts_with("written as read from the disc, not encoded: ")),
		"{warnings:?}"
	);

	let info = probed(&output).await;
	assert_eq!(info.streams[0].codec_name, "mpeg2video");
	assert!(near(info.format.duration.unwrap(), 23.0, 0.2), "{:?}", info.format);
	let chapters = chapter_titles(&info);
	assert!(chapters.iter().any(|(_, title)| title == "Title 2"), "{chapters:?}");
	assert!(!dir.path().join(CLI_WORK_DIR).exists());
}

#[tokio::test]
async fn a_dvd_image_under_a_non_ascii_folder_is_archived() {
	let dir = tempfile::tempdir().unwrap();
	let iso = dvd_image(
		dir.path(),
		&dir.path().join("Čžš").join("Izlet.iso"),
		&[footage(11, &[5, 6])],
	);

	let outcome = run(&job(&iso), dir.path()).await;

	let JobOutcome::Encoded { output, .. } = outcome else {
		panic!("{outcome:?}")
	};
	assert_eq!(output, dir.path().join("Izlet.cry.mkv"));
	let info = probed(&output).await;
	assert!(near(info.format.duration.unwrap(), 11.0, 0.2), "{:?}", info.format);
}

#[tokio::test]
async fn a_title_with_a_false_time_is_archived_at_its_real_length() {
	let dir = tempfile::tempdir().unwrap();
	let dense = Footage {
		bit_rate: "9M",
		..footage(12, &[1, 2])
	};
	let iso = dvd_image(dir.path(), &dir.path().join("dense.iso"), &[dense]);

	let outcome = run(&job(&iso), dir.path()).await;

	let JobOutcome::Encoded { output, decision, .. } = outcome else {
		panic!("{outcome:?}")
	};
	let disc = decision.disc.as_ref().unwrap();
	assert_eq!(disc.false_time, ["Title 1"]);
	assert!(near(disc.kept[0].1, 12.0, 0.2), "{disc:?}");
	let info = probed(&output).await;
	assert!(near(info.format.duration.unwrap(), 12.0, 0.2), "{:?}", info.format);
}

#[tokio::test]
async fn images_that_hold_no_dvd_or_blu_ray_are_skipped() {
	let dir = tempfile::tempdir().unwrap();
	let noise = support::pattern(200_000, 7);
	let random = dir.path().join("random.ISO");
	std::fs::write(&random, noise).unwrap();
	let plain = image(
		&dir.path().join("backup.iso"),
		vec![
			("photos/notes.txt".to_owned(), b"holiday".to_vec()),
			("VIDEO_TS/readme.txt".to_owned(), b"not a DVD".to_vec()),
		],
		&[],
		UdfRevision::V102,
	);

	for iso in [random, plain] {
		let outcome = run(&job(&iso), dir.path()).await;

		let JobOutcome::Skipped { reason } = &outcome else {
			panic!("{}: {outcome:?}", iso.display())
		};
		assert_eq!(reason, "not a DVD or Blu-ray image");
	}
}

fn decision(events: &[JobEvent]) -> &Decision {
	events
		.iter()
		.rev()
		.find_map(|event| match event {
			JobEvent::Decision(decision) => Some(&**decision),
			_ => None,
		})
		.unwrap_or_else(|| panic!("no decision: {events:?}"))
}

#[tokio::test]
async fn a_3d_blu_ray_image_is_always_kept_with_its_disc_recorded() {
	let dir = tempfile::tempdir().unwrap();
	let iso = blu_ray_image(&dir.path().join("3d.iso"), true);

	let (outcome, events) = run_with_events(&job(&iso), dir.path()).await;

	assert_eq!(kept_reason(&outcome), THREE_D);
	assert_eq!(
		decision(&events).disc.as_ref().map(|disc| disc.kind.as_str()),
		Some("Blu-ray")
	);
}

#[tokio::test]
async fn a_blu_ray_image_is_kept_as_is_when_asked() {
	let dir = tempfile::tempdir().unwrap();
	let iso = blu_ray_image(&dir.path().join("film.iso"), false);
	let keep = Job {
		options: JobOptions {
			blu_ray: BluRay::Keep,
			..JobOptions::default()
		},
		..job(&iso)
	};

	let outcome = run(&keep, dir.path()).await;

	assert_eq!(kept_reason(&outcome), "Blu-ray disc: kept as is");
}

#[tokio::test]
async fn a_format_a_disc_cannot_take_fails_before_the_disc_is_read() {
	let dir = tempfile::tempdir().unwrap();
	let iso = dvd_image(dir.path(), &dir.path().join("Film.iso"), &[footage(11, &[11])]);
	let as_image = Job {
		options: JobOptions {
			format: Some(OutputFormat::Jxl),
			..JobOptions::default()
		},
		..job(&iso)
	};

	let (outcome, events) = run_with_events(&as_image, dir.path()).await;

	let JobOutcome::Failed(err) = outcome else {
		panic!("{outcome:?}")
	};
	assert!(matches!(err.kind, ErrorKind::EncodeFailed(_)), "{err:?}");
	assert!(stage_at(&events, Stage::Read).is_empty());
}

/// The files of a Blu-ray with eight seconds of footage whose main playlist, 00800, has entry marks at 0, 3 and 5.5 s.
fn blu_ray_files(dir: &Path) -> Vec<(String, Vec<u8>)> {
	let main = Playlist {
		number: 800,
		declared_s: 200,
		marks_s: &[0.0, 3.0, 5.5],
	};
	let shorter = Playlist {
		number: 1,
		declared_s: 190,
		marks_s: &[0.0],
	};
	bluray::bdmv(dir, 8, &[shorter, main])
}

fn assert_main_feature(decision: &Decision, info: &MediaInfo) {
	let disc = decision.disc.as_ref().expect("a disc record");
	assert_eq!(disc.kind, "Blu-ray");
	assert_eq!(disc.playlist, Some(800));
	assert_eq!(disc.kept.len(), 1, "{disc:?}");
	assert_eq!(disc.kept[0].0, "Main feature");
	assert!(near(disc.kept[0].1, 8.0, 0.1), "{disc:?}");
	assert!(decision.warnings.is_empty(), "{:?}", decision.warnings);

	assert_eq!(info.streams[0].codec_name, "av1");
	assert!(near(info.format.duration.unwrap(), 8.0, 0.2), "{:?}", info.format);
	let chapters = chapter_titles(info);
	let titles: Vec<&str> = chapters.iter().map(|(_, title)| title.as_str()).collect();
	assert_eq!(titles, ["Chapter 1", "Chapter 2", "Chapter 3"]);
	assert!(near(chapters[2].0, 5.5, 0.1), "{chapters:?}");
	let audio = info.streams.iter().find(|stream| stream.is_audio()).unwrap();
	assert_eq!(audio.tag("language"), Some("slv"));
}

#[tokio::test]
async fn a_blu_ray_folder_becomes_one_av1_mkv_of_its_main_feature() {
	let dir = tempfile::tempdir().unwrap();
	let root = dir.path().join("Film.Disc1");
	dvd::write_folder(&root, &blu_ray_files(dir.path()));

	let (outcome, events) = run_with_events(&job(&root), dir.path()).await;

	let JobOutcome::Encoded { output, decision, .. } = outcome else {
		panic!("{outcome:?}")
	};
	assert_eq!(output, dir.path().join("Film.Disc1.cry.mkv"));
	assert_main_feature(&decision, &probed(&output).await);

	let queued = stage_at(&events, Stage::Queued);
	let read = stage_at(&events, Stage::Read);
	assert_eq!((queued.len(), read.len()), (1, 1), "{events:?}");
	assert!(queued[0] < read[0]);
	assert!(stage_at(&events, Stage::Encode)[0] > read[0]);
	assert!(!dir.path().join(CLI_WORK_DIR).exists());
}

#[tokio::test]
async fn a_blu_ray_image_is_converted_by_default() {
	let dir = tempfile::tempdir().unwrap();
	let iso = dir.path().join("Film.iso");
	Image {
		files: blu_ray_files(dir.path()),
		dirs: vec!["CERTIFICATE".to_owned()],
		revision: UdfRevision::V250,
	}
	.write_mastered(&iso);
	let iso_bytes = std::fs::metadata(&iso).unwrap().len();

	let outcome = run(&keeping_metadata(&iso), dir.path()).await;

	let JobOutcome::Encoded {
		output,
		src_bytes,
		decision,
		..
	} = outcome
	else {
		panic!("{outcome:?}")
	};
	assert_eq!(output, dir.path().join("Film.cry.mkv"));
	assert_eq!(src_bytes, iso_bytes);
	let info = probed(&output).await;
	assert_main_feature(&decision, &info);
	assert_eq!(recorded(&info), Some(&RECORDED[..19]), "{:?}", info.format.tags);

	let metadata = source_metadata(&output, dir.path());
	assert_eq!(metadata["ffprobe"]["format"]["filename"], iso.display().to_string());
	assert_eq!(metadata["disc"]["kind"], "Blu-ray");
	assert_eq!(metadata["disc"]["playlist"], 800);
}

#[tokio::test]
async fn converting_a_blu_ray_needs_ffmpeg_with_the_bluray_protocol() {
	let dir = tempfile::tempdir().unwrap();
	let iso = blu_ray_image(&dir.path().join("film.iso"), false);
	let mut tools = support::toolchain().await;
	tools.protocols.remove("bluray");
	let engine = Engine::new(tools);
	let sink = CliSink {
		output_dir: Some(dir.path().to_path_buf()),
		output_file: None,
		overwrite: false,
	};
	let events = Mutex::new(Vec::new());

	let outcome = engine
		.run(
			&job(&iso),
			&sink,
			&|event| events.lock().unwrap().push(event),
			&CancellationToken::new(),
		)
		.await;

	let JobOutcome::Failed(err) = outcome else {
		panic!("{outcome:?}")
	};
	assert_eq!(
		err.kind,
		ErrorKind::EncodeFailed("ffmpeg can't read Blu-ray discs (no bluray protocol)".into())
	);
	assert!(stage_at(&events.into_inner().unwrap(), Stage::Read).is_empty());
}

/// A DVD-like PS of `segments` one-second pieces with LPCM audio joined byte for byte, each with its own
/// timestamps from zero.
fn loose_vob(dir: &Path, name: &str, segments: u32) -> PathBuf {
	dvd::restarting(
		dir,
		name,
		segments,
		&[],
		&[
			"-c:v",
			"mpeg2video",
			"-b:v",
			"16M",
			"-minrate",
			"16M",
			"-maxrate",
			"16M",
			"-bufsize",
			"2M",
			"-c:a",
			"pcm_dvd",
			"-f",
			"dvd",
		],
	)
}

#[tokio::test]
async fn a_vob_whose_timestamps_restart_is_rebuilt_and_archived_at_its_real_length() {
	let dir = tempfile::tempdir().unwrap();
	let vob = loose_vob(dir.path(), "Loose.vob", 12);
	assert!(
		probed(&vob).await.format.duration.unwrap() < 2.0,
		"the fixture must declare a short duration"
	);

	let (outcome, events) = run_with_events(&keeping_metadata(&vob), dir.path()).await;

	let JobOutcome::Encoded {
		output,
		decision,
		warnings,
		..
	} = outcome
	else {
		panic!("{outcome:?}")
	};
	assert_eq!(output, dir.path().join("Loose.cry.mkv"));
	let rebuilt = "the source's timestamps restart; they were rebuilt";
	let once = |warnings: &[String]| warnings.iter().filter(|warning| *warning == rebuilt).count() == 1;
	assert!(once(&decision.warnings), "{:?}", decision.warnings);
	assert!(once(&warnings), "{warnings:?}");
	let emitted: Vec<String> = events
		.iter()
		.filter_map(|event| match event {
			JobEvent::Warning(warning) => Some(warning.clone()),
			_ => None,
		})
		.collect();
	assert!(once(&emitted), "{events:?}");
	assert!(decision.disc.is_none());

	let info = probed(&output).await;
	assert!(near(info.format.duration.unwrap(), 12.0, 0.3), "{:?}", info.format);
	let metadata = source_metadata(&output, dir.path());
	let format = &metadata["ffprobe"]["format"];
	assert_eq!(format["filename"], vob.display().to_string());
	assert_eq!(format["size"], std::fs::metadata(&vob).unwrap().len().to_string());
	assert!(metadata.get("disc").is_none(), "{metadata}");
	assert_eq!(stage_at(&events, Stage::Queued).len(), 1, "{events:?}");
	assert_eq!(stage_at(&events, Stage::Read).len(), 1, "{events:?}");
	assert!(!dir.path().join(CLI_WORK_DIR).exists());
}

#[tokio::test]
async fn a_ps_with_continuous_timestamps_is_not_rebuilt() {
	let dir = tempfile::tempdir().unwrap();
	let ps = support::video(
		dir.path(),
		"Plain.mpg",
		4,
		&["-c:v", "mpeg2video", "-c:a", "mp2", "-f", "mpeg"],
	);

	let (outcome, events) = run_with_events(&job(&ps), dir.path()).await;

	let JobOutcome::Encoded { decision, .. } = outcome else {
		panic!("{outcome:?}")
	};
	assert!(decision.warnings.is_empty(), "{:?}", decision.warnings);
	assert!(stage_at(&events, Stage::Read).is_empty(), "{events:?}");
}

#[tokio::test]
async fn an_encrypted_dvd_image_is_kept_as_it_is() {
	let dir = tempfile::tempdir().unwrap();
	let mut files = dvd::video_ts(dir.path(), &[footage(11, &[11])]);
	for (name, bytes) in &mut files {
		if name == "VIDEO_TS/VTS_01_1.VOB" {
			*bytes = dvd::scrambled(std::mem::take(bytes));
		}
	}
	let iso = image(&dir.path().join("Film.iso"), files, &["AUDIO_TS"], UdfRevision::V102);

	let (outcome, events) = run_with_events(&job(&iso), dir.path()).await;

	assert_eq!(kept_reason(&outcome), "the DVD is encrypted");
	assert!(stage_at(&events, Stage::Read).is_empty(), "{events:?}");
}

#[tokio::test]
async fn a_dvd_whose_angles_cannot_be_told_apart_is_kept_as_it_is() {
	let dir = tempfile::tempdir().unwrap();
	let iso = image(
		&dir.path().join("Angles.iso"),
		dvd::two_angles(dir.path(), footage(12, &[12]), false),
		&["AUDIO_TS"],
		UdfRevision::V102,
	);

	let outcome = run(&job(&iso), dir.path()).await;

	assert_eq!(kept_reason(&outcome), "multi-angle or branching titles aren't read yet");
}

#[tokio::test]
async fn an_encrypted_blu_ray_is_kept_as_it_is() {
	let dir = tempfile::tempdir().unwrap();
	let root = dir.path().join("Film");
	let mut files = blu_ray_files(dir.path());
	files.push(("AACS/Unit_Key_RO.inf".to_owned(), vec![0; 64]));
	dvd::write_folder(&root, &files);

	let (outcome, events) = run_with_events(&job(&root), dir.path()).await;

	assert_eq!(kept_reason(&outcome), "the Blu-ray is encrypted");
	assert!(stage_at(&events, Stage::Read).is_empty(), "{events:?}");
}

#[tokio::test]
async fn a_disc_job_cancelled_while_the_disc_is_read_leaves_no_workspace() {
	let dir = tempfile::tempdir().unwrap();
	let iso = dvd_image(dir.path(), &dir.path().join("Film.iso"), &[footage(12, &[6, 6])]);
	let engine = Engine::new(support::toolchain().await);
	let sink = CliSink {
		output_dir: Some(dir.path().to_path_buf()),
		output_file: None,
		overwrite: false,
	};
	let cancel = CancellationToken::new();

	let outcome = engine
		.run(
			&job(&iso),
			&sink,
			&|event| {
				if event == JobEvent::Stage(Stage::Read) {
					cancel.cancel();
				}
			},
			&cancel,
		)
		.await;

	let JobOutcome::Failed(err) = outcome else {
		panic!("{outcome:?}")
	};
	assert_eq!(err.kind, ErrorKind::Cancelled);
	let work = dir.path().join(CLI_WORK_DIR);
	let left: Vec<_> = std::fs::read_dir(&work)
		.map(|entries| entries.flatten().map(|entry| entry.path()).collect())
		.unwrap_or_default();
	assert!(left.is_empty(), "{left:?}");
	assert!(!dir.path().join("Film.cry.mkv").exists());
}
