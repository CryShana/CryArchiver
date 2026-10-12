mod support;

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use cryarchiver_engine::engine::{Engine, Job, JobEvent, Stage};
use cryarchiver_engine::format::OutputFormat;
use cryarchiver_engine::options::{BluRay, JobOptions};
use cryarchiver_engine::outcome::{ErrorKind, JobOutcome, KeepReason};
use cryarchiver_engine::preset::{Overrides, Speed};
use cryarchiver_engine::probe::{MediaInfo, probe};
use cryarchiver_engine::sink::CliSink;
use cryarchiver_engine::workspace::CLI_WORK_DIR;
use support::dvd::{self, Footage, footage};
use support::udf::{Image, UdfRevision};
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

async fn probed(path: &Path) -> MediaInfo {
	probe(&support::toolchain().await, path, &CancellationToken::new())
		.await
		.unwrap()
}

fn chapter_titles(info: &MediaInfo) -> Vec<(f64, String)> {
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

fn near(actual: f64, expected: f64, tolerance: f64) -> bool {
	(actual - expected).abs() <= tolerance
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

	let (outcome, events) = run_with_events(&job(&iso), dir.path()).await;

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

	let queued = stage_at(&events, Stage::Queued);
	let read = stage_at(&events, Stage::Disc);
	assert_eq!(queued.len(), 1, "{events:?}");
	assert_eq!(read.len(), 1, "{events:?}");
	assert!(queued[0] < read[0]);
	assert!(stage_at(&events, Stage::Encode)[0] > read[0]);
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
	let mut state = 7u32;
	let noise: Vec<u8> = (0..200_000)
		.map(|_| {
			state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
			state.to_be_bytes()[1]
		})
		.collect();
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

#[tokio::test]
async fn a_3d_blu_ray_image_is_always_kept() {
	let dir = tempfile::tempdir().unwrap();
	let iso = blu_ray_image(&dir.path().join("3d.iso"), true);

	let outcome = run(&job(&iso), dir.path()).await;

	assert_eq!(kept_reason(&outcome), THREE_D);
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
	assert!(stage_at(&events, Stage::Disc).is_empty());
}
