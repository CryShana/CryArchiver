mod support;

use std::fs::{self, File};
use std::path::Path;
use std::time::{Duration, SystemTime};

use cryarchiver_engine::engine::{Engine, Job, JobEvent, Stage};
use cryarchiver_engine::outcome::{ErrorClass, ErrorKind, JobOutcome, KeepReason};
use cryarchiver_engine::preset::Speed;
use cryarchiver_engine::sink::CliSink;
use cryarchiver_engine::workspace::CLI_WORK_DIR;
use tokio_util::sync::CancellationToken;

fn job(source: &Path) -> Job {
	Job {
		source: source.to_path_buf(),
		speed: Speed::Moderate,
		keep_metadata: false,
		min_savings: 0.10,
	}
}

fn sink(out_dir: &Path) -> CliSink {
	CliSink {
		output_dir: Some(out_dir.to_path_buf()),
		overwrite: false,
	}
}

async fn run(job: &Job, out_dir: &Path) -> JobOutcome {
	let engine = Engine::new(support::toolchain().await);
	engine
		.run(job, &sink(out_dir), &|_| {}, &CancellationToken::new())
		.await
}

#[tokio::test]
async fn flac_is_encoded_verified_and_committed() {
	let dir = tempfile::tempdir().unwrap();
	let flac = support::tone(dir.path(), "a.flac", "stereo", 2, 10, &["-c:a", "flac"]);

	let outcome = run(&job(&flac), dir.path()).await;

	let JobOutcome::Encoded {
		output,
		src_bytes,
		out_bytes,
		..
	} = outcome
	else {
		panic!("{outcome:?}")
	};
	assert_eq!(output, dir.path().join("a.cry.opus"));
	assert!(out_bytes < src_bytes);
	assert!(!dir.path().join(CLI_WORK_DIR).exists());
}

#[tokio::test]
async fn efficient_and_unsupported_audio_keep_the_original() {
	let dir = tempfile::tempdir().unwrap();
	let opus = support::tone(
		dir.path(),
		"low.opus",
		"mono",
		1,
		3,
		&["-c:a", "libopus", "-b:a", "32k"],
	);
	let wav = support::tone(dir.path(), "wide.wav", "10c", 10, 2, &["-c:a", "pcm_s16le"]);

	assert!(matches!(
		run(&job(&opus), dir.path()).await,
		JobOutcome::KeptOriginal {
			reason: KeepReason::AlreadyEfficient(_)
		}
	));
	assert!(matches!(
		run(&job(&wav), dir.path()).await,
		JobOutcome::KeptOriginal {
			reason: KeepReason::Unsupported(_)
		}
	));
}

#[tokio::test]
async fn outputs_that_are_not_smaller_keep_the_original() {
	let dir = tempfile::tempdir().unwrap();
	let mp3 = dir.path().join("low.mp3");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"anoisesrc=d=10:c=white:a=0.3",
		"-ac",
		"2",
		"-c:a",
		"libmp3lame",
		"-b:a",
		"64k",
		mp3.to_str().unwrap(),
	]);

	let outcome = run(&job(&mp3), dir.path()).await;

	assert!(
		matches!(
			outcome,
			JobOutcome::KeptOriginal {
				reason: KeepReason::NotSmaller { .. }
			}
		),
		"{outcome:?}"
	);
	assert!(!dir.path().join("low.cry.opus").exists());
}

#[tokio::test]
async fn text_and_rerun_are_skipped() {
	let dir = tempfile::tempdir().unwrap();
	let text = dir.path().join("notes.mp3");
	fs::write(&text, b"plain text, not audio").unwrap();
	let flac = support::tone(dir.path(), "a.flac", "stereo", 2, 5, &["-c:a", "flac"]);

	assert!(matches!(run(&job(&text), dir.path()).await, JobOutcome::Skipped { .. }));
	assert!(matches!(run(&job(&flac), dir.path()).await, JobOutcome::Encoded { .. }));
	assert!(
		matches!(run(&job(&flac), dir.path()).await, JobOutcome::Skipped { reason } if reason.contains("already exists"))
	);
}

#[tokio::test]
async fn truncated_flac_is_source_damaged() {
	let dir = tempfile::tempdir().unwrap();
	let flac = support::tone(dir.path(), "cut.flac", "stereo", 2, 10, &["-c:a", "flac"]);
	let bytes = fs::read(&flac).unwrap();
	fs::write(&flac, &bytes[..bytes.len() * 2 / 5]).unwrap();

	let outcome = run(&job(&flac), dir.path()).await;

	assert!(
		matches!(&outcome, JobOutcome::Failed(err) if matches!(err.kind, ErrorKind::SourceDamaged(_))),
		"{outcome:?}"
	);
	assert!(!dir.path().join("cut.cry.opus").exists());
}

#[tokio::test]
async fn keep_metadata_copies_the_source_modified_time() {
	let dir = tempfile::tempdir().unwrap();
	let flac = support::tone(dir.path(), "a.flac", "stereo", 2, 5, &["-c:a", "flac"]);
	let old = SystemTime::UNIX_EPOCH + Duration::from_mins(20_000_000);
	File::options()
		.write(true)
		.open(&flac)
		.unwrap()
		.set_modified(old)
		.unwrap();

	let outcome = run(
		&Job {
			keep_metadata: true,
			..job(&flac)
		},
		dir.path(),
	)
	.await;

	let JobOutcome::Encoded { output, .. } = outcome else {
		panic!("{outcome:?}")
	};
	assert_eq!(fs::metadata(output).unwrap().modified().unwrap(), old);
}

#[tokio::test]
async fn cancel_mid_encode_leaves_nothing_behind() {
	let dir = tempfile::tempdir().unwrap();
	let flac = support::tone(dir.path(), "long.flac", "stereo", 2, 600, &["-c:a", "flac"]);
	let engine = Engine::new(support::toolchain().await);
	let cancel = CancellationToken::new();
	let trigger = cancel.clone();
	let on_event = move |event: JobEvent| {
		if event == JobEvent::Stage(Stage::Encode) {
			let trigger = trigger.clone();
			tokio::spawn(async move {
				tokio::time::sleep(Duration::from_millis(200)).await;
				trigger.cancel();
			});
		}
	};

	let outcome = engine.run(&job(&flac), &sink(dir.path()), &on_event, &cancel).await;

	assert!(
		matches!(&outcome, JobOutcome::Failed(err) if err.kind == ErrorKind::Cancelled),
		"{outcome:?}"
	);
	assert!(!dir.path().join(CLI_WORK_DIR).exists());
	assert!(!dir.path().join("long.cry.opus").exists());
}

#[tokio::test]
async fn source_changed_during_encode_is_retryable() {
	let dir = tempfile::tempdir().unwrap();
	let flac = support::tone(dir.path(), "a.flac", "stereo", 2, 10, &["-c:a", "flac"]);
	let engine = Engine::new(support::toolchain().await);
	let touched = flac.clone();
	let on_event = move |event: JobEvent| {
		if event == JobEvent::Stage(Stage::Encode) {
			let later = SystemTime::now() + Duration::from_secs(3600);
			File::options()
				.write(true)
				.open(&touched)
				.unwrap()
				.set_modified(later)
				.unwrap();
		}
	};

	let outcome = engine
		.run(&job(&flac), &sink(dir.path()), &on_event, &CancellationToken::new())
		.await;

	let JobOutcome::Failed(err) = outcome else {
		panic!("{outcome:?}")
	};
	assert_eq!(err.kind, ErrorKind::SourceChanged);
	assert_eq!(err.kind.class(), ErrorClass::AutoRetry);
	assert!(!dir.path().join("a.cry.opus").exists());
}
