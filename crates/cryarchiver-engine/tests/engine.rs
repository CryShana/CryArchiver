mod support;

use std::fs::{self, File};
use std::path::Path;
use std::time::{Duration, SystemTime};

use cryarchiver_engine::engine::{Engine, Job, JobEvent, Stage};
use cryarchiver_engine::outcome::{ErrorClass, ErrorKind, JobOutcome, KeepReason};
use cryarchiver_engine::preset::Speed;
use cryarchiver_engine::probe::{Stream, probe};
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
async fn unreadable_covers_are_warned() {
	let dir = tempfile::tempdir().unwrap();
	let tone = support::tone(dir.path(), "tone.flac", "stereo", 2, 5, &["-c:a", "flac"]);
	let cover = dir.path().join("cover.png");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"color=c=red:s=64x64",
		"-frames:v",
		"1",
		"-update",
		"1",
		cover.to_str().unwrap(),
	]);
	let mka = dir.path().join("x.mka");
	support::ffmpeg(&[
		"-i",
		tone.to_str().unwrap(),
		"-attach",
		cover.to_str().unwrap(),
		"-metadata:s:t",
		"mimetype=image/png",
		"-map",
		"0:a",
		"-c:a",
		"flac",
		mka.to_str().unwrap(),
	]);

	let info = probe(&support::toolchain().await, &mka, &CancellationToken::new())
		.await
		.unwrap();
	assert!(info.streams.iter().any(Stream::is_attached_pic), "{info:?}");

	let outcome = run(&job(&mka), dir.path()).await;

	let JobOutcome::Encoded { warnings, .. } = outcome else {
		panic!("{outcome:?}")
	};
	assert!(
		warnings.iter().any(|warning| warning.contains("cover art")),
		"{warnings:?}"
	);
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

fn fast(source: &Path) -> Job {
	Job {
		speed: Speed::Fast,
		..job(source)
	}
}

fn fast_keeping_any_saving(source: &Path) -> Job {
	Job {
		min_savings: 0.0,
		..fast(source)
	}
}

#[tokio::test]
async fn video_becomes_verified_av1_in_mkv() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::mpeg4(dir.path(), "clip.mp4", 3);

	let outcome = run(&fast(&clip), dir.path()).await;

	let JobOutcome::Encoded {
		output,
		out_bytes,
		src_bytes,
		..
	} = outcome
	else {
		panic!("{outcome:?}")
	};
	assert_eq!(output, dir.path().join("clip.cry.mkv"));
	assert!(out_bytes < src_bytes);
	let info = probe(&support::toolchain().await, &output, &CancellationToken::new())
		.await
		.unwrap();
	assert_eq!(info.streams[0].codec_name, "av1");
	assert_eq!(info.streams[0].pix_fmt.as_deref(), Some("yuv420p10le"));
	assert_eq!(info.streams[0].color_primaries.as_deref(), Some("bt470bg"));
	assert_eq!(info.streams[1].codec_name, "opus");
	assert!(!dir.path().join(CLI_WORK_DIR).exists());
}

#[tokio::test]
async fn hdr10_keeps_its_transfer_and_mastering_metadata() {
	let dir = tempfile::tempdir().unwrap();
	let hdr = dir.path().join("hdr.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=320x240:r=25:d=2,setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc",
		"-c:v",
		"libx265",
		"-pix_fmt",
		"yuv420p10le",
		"-crf",
		"4",
		"-x265-params",
		"master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1):max-cll=1000,400:log-level=error",
		hdr.to_str().unwrap(),
	]);

	let outcome = run(&fast_keeping_any_saving(&hdr), dir.path()).await;

	let JobOutcome::Encoded { output, .. } = outcome else {
		panic!("{outcome:?}")
	};
	let info = probe(&support::toolchain().await, &output, &CancellationToken::new())
		.await
		.unwrap();
	assert_eq!(info.streams[0].color_transfer.as_deref(), Some("smpte2084"));
	assert_eq!(info.streams[0].color_primaries.as_deref(), Some("bt2020"));
	assert!(
		info.streams[0].side_data("Mastering display metadata").is_some(),
		"{:?}",
		info.streams[0]
	);
}

#[tokio::test]
async fn interlaced_video_is_deinterlaced_to_double_rate() {
	let dir = tempfile::tempdir().unwrap();
	let interlaced = dir.path().join("tff.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=720x576:r=50:d=4,interlace=scan=tff,setfield=tff",
		"-c:v",
		"mpeg2video",
		"-b:v",
		"12M",
		"-flags",
		"+ilme+ildct",
		interlaced.to_str().unwrap(),
	]);

	let outcome = run(&fast(&interlaced), dir.path()).await;

	let JobOutcome::Encoded { output, .. } = outcome else {
		panic!("{outcome:?}")
	};
	let info = probe(&support::toolchain().await, &output, &CancellationToken::new())
		.await
		.unwrap();
	assert!(
		(info.streams[0].fps().unwrap() - 50.0).abs() < 0.5,
		"{:?}",
		info.streams[0]
	);
	assert_eq!(info.streams[0].color_primaries.as_deref(), Some("bt470bg"));
}

#[tokio::test]
async fn transparent_video_stays_vp9_with_alpha() {
	let dir = tempfile::tempdir().unwrap();
	let alpha = dir.path().join("alpha.mov");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=320x240:r=25:d=2,format=rgba,geq=r='r(X,Y)':g='g(X,Y)':b='b(X,Y)':a='if(lt(X,160),255,0)'",
		"-c:v",
		"qtrle",
		alpha.to_str().unwrap(),
	]);

	let outcome = run(&fast(&alpha), dir.path()).await;

	let JobOutcome::Encoded { output, .. } = outcome else {
		panic!("{outcome:?}")
	};
	let info = probe(&support::toolchain().await, &output, &CancellationToken::new())
		.await
		.unwrap();
	assert_eq!(info.streams[0].codec_name, "vp9");
	assert_eq!(info.streams[0].tag("alpha_mode"), Some("1"));
}

#[tokio::test]
async fn truncated_mp4_fails_before_encoding() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::video(
		dir.path(),
		"cut.mp4",
		4,
		&[
			"-map",
			"0:v",
			"-map",
			"1:a",
			"-c:v",
			"mpeg4",
			"-c:a",
			"aac",
			"-movflags",
			"+faststart",
		],
	);
	let bytes = fs::read(&clip).unwrap();
	fs::write(&clip, &bytes[..bytes.len() / 2]).unwrap();
	let stages = std::sync::Mutex::new(Vec::new());

	let outcome = Engine::new(support::toolchain().await)
		.run(
			&fast(&clip),
			&sink(dir.path()),
			&|event| stages.lock().unwrap().push(event),
			&CancellationToken::new(),
		)
		.await;

	assert!(
		matches!(&outcome, JobOutcome::Failed(err) if matches!(&err.kind, ErrorKind::SourceDamaged(reason) if reason.contains("truncated"))),
		"{outcome:?}"
	);
	assert!(!stages.lock().unwrap().contains(&JobEvent::Stage(Stage::Encode)));
	assert!(!dir.path().join(CLI_WORK_DIR).exists());
}

#[tokio::test]
async fn unencodable_audio_is_copied_and_the_video_still_encoded() {
	let dir = tempfile::tempdir().unwrap();
	let clip = dir.path().join("wide.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=320x240:r=25:d=3",
		"-f",
		"lavfi",
		"-i",
		"anoisesrc=d=3:a=0.1",
		"-filter_complex",
		"[1:a]pan=10c|c0=c0|c1=c0|c2=c0|c3=c0|c4=c0|c5=c0|c6=c0|c7=c0|c8=c0|c9=c0[wide]",
		"-map",
		"0:v",
		"-map",
		"[wide]",
		"-c:v",
		"mpeg4",
		"-q:v",
		"2",
		"-c:a",
		"pcm_s16le",
		clip.to_str().unwrap(),
	]);

	let outcome = run(&fast_keeping_any_saving(&clip), dir.path()).await;

	let JobOutcome::Encoded { output, warnings, .. } = outcome else {
		panic!("{outcome:?}")
	};
	assert!(
		warnings.iter().any(|warning| warning.contains("copied as-is")),
		"{warnings:?}"
	);
	let info = probe(&support::toolchain().await, &output, &CancellationToken::new())
		.await
		.unwrap();
	assert_eq!(info.streams[1].codec_name, "pcm_s16le");
}

#[tokio::test]
async fn audio_longer_than_video_is_not_damaged() {
	let dir = tempfile::tempdir().unwrap();
	let clip = dir.path().join("long-audio.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=320x240:r=25:d=3",
		"-f",
		"lavfi",
		"-i",
		"sine=duration=6",
		"-map",
		"0:v",
		"-map",
		"1:a",
		"-c:v",
		"mpeg4",
		"-q:v",
		"2",
		"-c:a",
		"flac",
		clip.to_str().unwrap(),
	]);

	let outcome = run(&fast(&clip), dir.path()).await;

	assert!(matches!(outcome, JobOutcome::Encoded { .. }), "{outcome:?}");
}

#[tokio::test]
async fn efficient_av1_is_kept_without_encoding() {
	let dir = tempfile::tempdir().unwrap();
	let av1 = dir.path().join("small.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=640x360:r=25:d=3",
		"-c:v",
		"libsvtav1",
		"-preset",
		"12",
		"-svtav1-params",
		"crf=63",
		av1.to_str().unwrap(),
	]);

	let outcome = run(&fast(&av1), dir.path()).await;

	assert!(
		matches!(
			outcome,
			JobOutcome::KeptOriginal {
				reason: KeepReason::AlreadyEfficient(_)
			}
		),
		"{outcome:?}"
	);
}

#[tokio::test]
async fn keep_metadata_attaches_the_source_probe() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::mpeg4(dir.path(), "meta.mp4", 2);

	let outcome = run(
		&Job {
			keep_metadata: true,
			..fast(&clip)
		},
		dir.path(),
	)
	.await;

	let JobOutcome::Encoded { output, .. } = outcome else {
		panic!("{outcome:?}")
	};
	let info = probe(&support::toolchain().await, &output, &CancellationToken::new())
		.await
		.unwrap();
	assert!(
		info.streams
			.iter()
			.any(|stream| stream.tag("filename") == Some("source-metadata.json")),
		"{info:?}"
	);
}

#[tokio::test]
async fn cancel_mid_video_encode_leaves_nothing_behind() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::video(
		dir.path(),
		"long.mkv",
		120,
		&[
			"-map", "0:v", "-map", "1:a", "-c:v", "mpeg4", "-q:v", "2", "-c:a", "flac",
		],
	);
	let engine = Engine::new(support::toolchain().await);
	let cancel = CancellationToken::new();
	let trigger = cancel.clone();
	let on_event = move |event: JobEvent| {
		if event == JobEvent::Stage(Stage::Encode) {
			let trigger = trigger.clone();
			tokio::spawn(async move {
				tokio::time::sleep(Duration::from_millis(300)).await;
				trigger.cancel();
			});
		}
	};

	let outcome = engine.run(&job(&clip), &sink(dir.path()), &on_event, &cancel).await;

	assert!(
		matches!(&outcome, JobOutcome::Failed(err) if err.kind == ErrorKind::Cancelled),
		"{outcome:?}"
	);
	assert!(!dir.path().join(CLI_WORK_DIR).exists());
	assert!(!dir.path().join("long.cry.mkv").exists());
}

#[tokio::test]
async fn cancel_while_sampling_leaves_nothing_behind() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::mpeg4(dir.path(), "clip.mp4", 125);
	let engine = Engine::new(support::toolchain().await);
	let cancel = CancellationToken::new();
	let trigger = cancel.clone();
	let on_event = move |event: JobEvent| {
		if event == JobEvent::Stage(Stage::Sample) {
			let trigger = trigger.clone();
			tokio::spawn(async move {
				tokio::time::sleep(Duration::from_millis(300)).await;
				trigger.cancel();
			});
		}
	};

	let outcome = engine.run(&fast(&clip), &sink(dir.path()), &on_event, &cancel).await;

	assert!(
		matches!(&outcome, JobOutcome::Failed(err) if err.kind == ErrorKind::Cancelled),
		"{outcome:?}"
	);
	assert!(!dir.path().join(CLI_WORK_DIR).exists());
	assert!(!dir.path().join("clip.cry.mkv").exists());
}

#[tokio::test]
async fn one_video_slot_queues_the_second_video() {
	let dir = tempfile::tempdir().unwrap();
	let first = support::mpeg4(dir.path(), "first.mp4", 3);
	let second = support::mpeg4(dir.path(), "second.mp4", 3);
	let engine = Engine::new(support::toolchain().await);
	let queued = std::sync::atomic::AtomicUsize::new(0);
	let on_event = |event: JobEvent| {
		if event == JobEvent::Stage(Stage::Queued) {
			queued.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
		}
	};
	let cancel = CancellationToken::new();
	let out_sink = sink(dir.path());
	let (first_job, second_job) = (fast(&first), fast(&second));

	let (a, b) = tokio::join!(
		engine.run(&first_job, &out_sink, &on_event, &cancel),
		engine.run(&second_job, &out_sink, &on_event, &cancel),
	);

	assert!(matches!(a, JobOutcome::Encoded { .. }), "{a:?}");
	assert!(matches!(b, JobOutcome::Encoded { .. }), "{b:?}");
	assert_eq!(queued.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn delayed_audio_stays_in_sync() {
	let dir = tempfile::tempdir().unwrap();
	let clip = dir.path().join("delayed.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=320x240:r=25:d=3",
		"-itsoffset",
		"0.5",
		"-f",
		"lavfi",
		"-i",
		"sine=frequency=440:duration=3",
		"-map",
		"0:v",
		"-map",
		"1:a",
		"-c:v",
		"mpeg4",
		"-q:v",
		"2",
		"-c:a",
		"aac",
		clip.to_str().unwrap(),
	]);

	let outcome = run(&fast_keeping_any_saving(&clip), dir.path()).await;

	let JobOutcome::Encoded { output, .. } = outcome else {
		panic!("{outcome:?}")
	};
	let info = probe(&support::toolchain().await, &output, &CancellationToken::new())
		.await
		.unwrap();
	let start = |kind: &str| {
		info.streams
			.iter()
			.find(|stream| stream.codec_type == kind)
			.and_then(|stream| stream.start_time)
			.unwrap()
	};
	let delay = start("audio") - start("video");
	assert!((delay - 0.5).abs() <= 0.05, "{delay} {info:?}");
}

#[tokio::test]
async fn trimmed_mp4_is_encoded() {
	let dir = tempfile::tempdir().unwrap();
	let full = support::video(
		dir.path(),
		"full.mp4",
		6,
		&[
			"-map", "0:v", "-map", "1:a", "-c:v", "libx264", "-crf", "8", "-pix_fmt", "yuv420p", "-g", "50", "-c:a",
			"aac",
		],
	);
	let trimmed = dir.path().join("trimmed.mp4");
	support::ffmpeg(&[
		"-ss",
		"1.3",
		"-i",
		full.to_str().unwrap(),
		"-c",
		"copy",
		trimmed.to_str().unwrap(),
	]);

	let outcome = run(&fast_keeping_any_saving(&trimmed), dir.path()).await;

	assert!(matches!(outcome, JobOutcome::Encoded { .. }), "{outcome:?}");
}

#[tokio::test]
async fn a_video_that_would_not_shrink_is_kept_before_encoding() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::video(
		dir.path(),
		"low.mp4",
		125,
		&["-map", "0:v", "-c:v", "libx264", "-crf", "45", "-preset", "veryslow"],
	);
	let stages = std::sync::Mutex::new(Vec::new());

	let outcome = Engine::new(support::toolchain().await)
		.run(
			&fast(&clip),
			&sink(dir.path()),
			&|event| stages.lock().unwrap().push(event),
			&CancellationToken::new(),
		)
		.await;

	assert!(
		matches!(
			outcome,
			JobOutcome::KeptOriginal {
				reason: KeepReason::PredictedNotSmaller { .. }
			}
		),
		"{outcome:?}"
	);
	let stages = stages.lock().unwrap();
	assert!(stages.contains(&JobEvent::Stage(Stage::Sample)), "{stages:?}");
	assert!(!stages.contains(&JobEvent::Stage(Stage::Encode)), "{stages:?}");
	assert!(!dir.path().join("low.cry.mkv").exists());
	assert!(!dir.path().join(CLI_WORK_DIR).exists());
}

#[tokio::test]
async fn a_video_that_shrinks_passes_the_prediction() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::mpeg4(dir.path(), "clip.mp4", 125);
	let stages = std::sync::Mutex::new(Vec::new());

	let outcome = Engine::new(support::toolchain().await)
		.run(
			&fast(&clip),
			&sink(dir.path()),
			&|event| stages.lock().unwrap().push(event),
			&CancellationToken::new(),
		)
		.await;

	assert!(matches!(outcome, JobOutcome::Encoded { .. }), "{outcome:?}");
	assert!(stages.lock().unwrap().contains(&JobEvent::Stage(Stage::Sample)));
}
