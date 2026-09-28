mod support;

use std::fs::{self, File};
use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use cryarchiver_engine::engine::{Encode, Engine, Job, JobEvent, MemoryGate, Stage};
use cryarchiver_engine::format::OutputFormat;
use cryarchiver_engine::images::plan::worst_case_memory;
use cryarchiver_engine::options::JobOptions;
use cryarchiver_engine::outcome::{ErrorClass, ErrorKind, JobOutcome, KeepReason};
use cryarchiver_engine::preset::{Overrides, Speed, VmafOverride};
use cryarchiver_engine::probe::{Stream, probe};
use cryarchiver_engine::quality::memory::{GroupStats, SharedMemory};
use cryarchiver_engine::sink::{ArchivalSink, CliSink};
use cryarchiver_engine::workspace::CLI_WORK_DIR;
use lofty::config::ParseOptions;
use lofty::file::AudioFile;
use lofty::ogg::{OggPictureStorage, OpusFile};
use tokio_util::sync::CancellationToken;

fn job(source: &Path) -> Job {
	Job {
		source: source.to_path_buf(),
		speed: Speed::Moderate,
		keep_metadata: false,
		drop_aux: false,
		min_savings: 0.10,
		overrides: Overrides::default(),
		options: JobOptions::default(),
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
async fn archival_jobs_are_staged_with_the_source_times() {
	let dir = tempfile::tempdir().unwrap();
	let flac = support::tone(dir.path(), "a.flac", "stereo", 2, 10, &["-c:a", "flac"]);
	let modified = SystemTime::UNIX_EPOCH + Duration::from_mins(25_000_000);
	File::options()
		.write(true)
		.open(&flac)
		.unwrap()
		.set_modified(modified)
		.unwrap();
	fs::create_dir(dir.path().join("archive")).unwrap();
	let sink = ArchivalSink::new(&dir.path().join("archive"));
	let engine: Box<dyn Encode> = Box::new(Engine::new(support::toolchain().await));
	let archival = Job {
		keep_metadata: true,
		..job(&flac)
	};

	let outcome = engine
		.encode(&archival, &sink, &|_| {}, &CancellationToken::new())
		.await;

	let JobOutcome::Encoded { output, .. } = outcome else {
		panic!("{outcome:?}")
	};
	assert_eq!(output.parent().unwrap(), sink.staged_dir());
	assert_eq!(output.extension().unwrap(), "opus");
	assert_eq!(fs::metadata(&output).unwrap().modified().unwrap(), modified);
	assert_eq!(fs::read_dir(sink.archive_work_root()).unwrap().count(), 1);
}

#[tokio::test]
async fn an_archival_job_never_works_or_removes_anything_through_a_linked_work_area() {
	let dir = tempfile::tempdir().unwrap();
	let flac = support::tone(dir.path(), "a.flac", "stereo", 2, 10, &["-c:a", "flac"]);
	let archive = dir.path().join("archive");
	let elsewhere = dir.path().join("elsewhere");
	fs::create_dir(&archive).unwrap();
	fs::create_dir_all(elsewhere.join("work")).unwrap();
	#[cfg(windows)]
	let linked = std::process::Command::new("cmd")
		.args(["/C", "mklink", "/J"])
		.arg(archive.join(".cryarchiver"))
		.arg(&elsewhere)
		.output()
		.is_ok_and(|out| out.status.success());
	#[cfg(unix)]
	let linked = std::os::unix::fs::symlink(&elsewhere, archive.join(".cryarchiver")).is_ok();
	if !linked {
		eprintln!("skipped: no folder link can be made here");
		return;
	}
	let engine: Box<dyn Encode> = Box::new(Engine::new(support::toolchain().await));

	let outcome = engine
		.encode(
			&job(&flac),
			&ArchivalSink::new(&archive),
			&|_| {},
			&CancellationToken::new(),
		)
		.await;

	assert!(
		matches!(&outcome, JobOutcome::Failed(err) if matches!(err.kind, ErrorKind::UnsafeDestination(_))),
		"{outcome:?}"
	);
	assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 1);
	assert_eq!(fs::read_dir(elsewhere.join("work")).unwrap().count(), 0);
}

#[derive(Default)]
struct CountingGate {
	refusals: AtomicU32,
	requested: AtomicU64,
	reserved: AtomicU64,
}

impl MemoryGate for CountingGate {
	fn try_reserve(&self, bytes: u64) -> bool {
		self.requested.store(bytes, Ordering::SeqCst);
		if self
			.refusals
			.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| left.checked_sub(1))
			.is_ok()
		{
			return false;
		}

		self.reserved.fetch_add(bytes, Ordering::SeqCst);
		true
	}

	fn release(&self, bytes: u64) {
		self.reserved.fetch_sub(bytes, Ordering::SeqCst);
	}
}

#[tokio::test]
async fn image_jobs_wait_for_memory_and_release_it() {
	let dir = tempfile::tempdir().unwrap();
	let png = support::photo_png(dir.path(), "a.png", 64, 48);
	let gate = Arc::new(CountingGate {
		refusals: AtomicU32::new(2),
		..CountingGate::default()
	});
	let engine = Engine::new(support::toolchain().await).with_memory_gate(gate.clone());
	let stages = Mutex::new(Vec::new());
	let on_event = |event: JobEvent| {
		if let JobEvent::Stage(stage) = event {
			stages.lock().unwrap().push(stage);
		}
	};

	let outcome = engine
		.run(&job(&png), &sink(dir.path()), &on_event, &CancellationToken::new())
		.await;

	assert!(!matches!(outcome, JobOutcome::Failed(_)), "{outcome:?}");
	assert!(stages.lock().unwrap().contains(&Stage::WaitingForMemory));
	assert_eq!(gate.requested.load(Ordering::SeqCst), worst_case_memory(64, 48));
	assert_eq!(gate.refusals.load(Ordering::SeqCst), 0);
	assert_eq!(gate.reserved.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_image_override_is_the_target_the_still_is_encoded_against() {
	let dir = tempfile::tempdir().unwrap();
	let png = support::photo_png(dir.path(), "a.png", 320, 240);
	let memory = Arc::new(SharedMemory::default());
	let engine = Engine::new(support::toolchain().await).with_quality_memory(memory.clone());
	let custom = Job {
		overrides: Overrides {
			image: Some(60.0),
			..Overrides::default()
		},
		..fast(&png)
	};

	let outcome = engine
		.run(&custom, &sink(dir.path()), &|_| {}, &CancellationToken::new())
		.await;

	assert!(matches!(outcome, JobOutcome::Encoded { .. }), "{outcome:?}");
	let entries = memory.entries();
	assert_eq!(entries.len(), 1, "{entries:?}");
	assert_eq!(entries[0].0, "image/png/photo/opaque/8bit/mp1|ravif-0.13.0|p9|t60.0");
}

#[tokio::test]
async fn a_job_waiting_for_memory_can_be_cancelled() {
	let dir = tempfile::tempdir().unwrap();
	let png = support::photo_png(dir.path(), "a.png", 64, 48);
	let gate = Arc::new(CountingGate {
		refusals: AtomicU32::new(u32::MAX),
		..CountingGate::default()
	});
	let engine = Engine::new(support::toolchain().await).with_memory_gate(gate);
	let cancel = CancellationToken::new();
	let canceller = cancel.clone();
	tokio::spawn(async move {
		tokio::time::sleep(Duration::from_millis(600)).await;
		canceller.cancel();
	});

	let outcome = engine.run(&job(&png), &sink(dir.path()), &|_| {}, &cancel).await;

	assert!(
		matches!(&outcome, JobOutcome::Failed(err) if err.kind == ErrorKind::Cancelled),
		"{outcome:?}"
	);
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
async fn hi_res_hexagonal_and_junk_prefixed_audio_is_encoded() {
	let dir = tempfile::tempdir().unwrap();
	let hi_res = support::tone(
		dir.path(),
		"hi-res.flac",
		"stereo",
		2,
		3,
		&[
			"-ar",
			"96000",
			"-c:a",
			"flac",
			"-sample_fmt",
			"s32",
			"-bits_per_raw_sample",
			"24",
		],
	);
	// FLAC itself can't flag a hexagonal layout and ffmpeg says so at error level; Matroska still carries it.
	let hexagonal = support::tone(
		dir.path(),
		"hexagonal.mka",
		"hexagonal",
		6,
		3,
		&["-loglevel", "fatal", "-c:a", "flac"],
	);
	let mp3 = support::tone(
		dir.path(),
		"song.mp3",
		"stereo",
		2,
		3,
		&["-c:a", "libmp3lame", "-b:a", "320k"],
	);
	let junk: Vec<u8> = (0..4096u32).map(|i| u8::try_from(i * 7919 % 251).unwrap()).collect();
	let prefixed = dir.path().join("prefixed.mp3");
	fs::write(&prefixed, [junk, fs::read(&mp3).unwrap()].concat()).unwrap();

	for source in [&hi_res, &hexagonal, &prefixed] {
		let outcome = run(&job(source), dir.path()).await;
		assert!(
			matches!(&outcome, JobOutcome::Encoded { output, .. } if output.extension().unwrap() == "opus"),
			"{}: {outcome:?}",
			source.display()
		);
	}
}

#[tokio::test]
async fn covers_of_mp3_and_alac_sources_are_carried_into_the_opus() {
	let dir = tempfile::tempdir().unwrap();
	let mp3 = support::mp3_with_cover(dir.path(), "song.mp3", "Title");
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
	let alac = dir.path().join("lossless.m4a");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"anoisesrc=d=3:c=pink:a=0.3",
		"-i",
		cover.to_str().unwrap(),
		"-map",
		"0:a",
		"-map",
		"1:v",
		"-ac",
		"2",
		"-c:a",
		"alac",
		"-c:v",
		"copy",
		"-disposition:v",
		"attached_pic",
		alac.to_str().unwrap(),
	]);

	for source in [&mp3, &alac] {
		let outcome = run(&job(source), dir.path()).await;

		let JobOutcome::Encoded { output, .. } = outcome else {
			panic!("{}: {outcome:?}", source.display())
		};
		let opus = OpusFile::read_from(&mut File::open(&output).unwrap(), ParseOptions::new()).unwrap();
		assert_eq!(opus.vorbis_comments().pictures().len(), 1, "{}", source.display());
	}
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

async fn encoded_info(outcome: JobOutcome) -> cryarchiver_engine::probe::MediaInfo {
	let JobOutcome::Encoded { output, .. } = outcome else {
		panic!("{outcome:?}")
	};
	probe(&support::toolchain().await, &output, &CancellationToken::new())
		.await
		.unwrap()
}

#[tokio::test]
async fn a_rotated_hlg_phone_clip_is_encoded_upright_and_stays_hlg() {
	let dir = tempfile::tempdir().unwrap();
	let upright = support::video(
		dir.path(),
		"upright.mp4",
		2,
		&[
			"-map",
			"0:v",
			"-map",
			"1:a",
			"-vf",
			"setparams=color_primaries=bt2020:color_trc=arib-std-b67:colorspace=bt2020nc",
			"-c:v",
			"libx265",
			"-pix_fmt",
			"yuv420p10le",
			"-crf",
			"4",
			"-x265-params",
			"colorprim=bt2020:transfer=arib-std-b67:colormatrix=bt2020nc:log-level=error",
			"-c:a",
			"aac",
		],
	);
	let rotated = dir.path().join("phone clip.mov");
	support::ffmpeg(&[
		"-display_rotation:v:0",
		"90",
		"-i",
		upright.to_str().unwrap(),
		"-c",
		"copy",
		rotated.to_str().unwrap(),
	]);

	let info = encoded_info(run(&fast_keeping_any_saving(&rotated), dir.path()).await).await;

	let video = &info.streams[0];
	assert_eq!((video.width, video.height), (Some(240), Some(320)), "{video:?}");
	assert_eq!(video.color_transfer.as_deref(), Some("arib-std-b67"));
	assert!(video.side_data("Display Matrix").is_none(), "{video:?}");
}

#[tokio::test]
async fn full_range_and_bt2020_sdr_colour_is_kept() {
	let dir = tempfile::tempdir().unwrap();
	let full_range = support::video(
		dir.path(),
		"full-range.mp4",
		2,
		&[
			"-map",
			"0:v",
			"-map",
			"1:a",
			"-vf",
			"scale=out_range=pc,format=yuv420p",
			"-color_range",
			"pc",
			"-c:v",
			"libx264",
			"-crf",
			"12",
			"-c:a",
			"aac",
		],
	);
	let wide_gamut = support::video(
		dir.path(),
		"bt2020.mkv",
		2,
		&[
			"-map",
			"0:v",
			"-map",
			"1:a",
			"-vf",
			"setparams=color_primaries=bt2020:color_trc=bt2020-10:colorspace=bt2020nc",
			"-c:v",
			"libx265",
			"-pix_fmt",
			"yuv420p10le",
			"-crf",
			"8",
			"-x265-params",
			"colorprim=bt2020:transfer=bt2020-10:colormatrix=bt2020nc:log-level=error",
			"-c:a",
			"flac",
		],
	);

	let full = encoded_info(run(&fast_keeping_any_saving(&full_range), dir.path()).await).await;
	let wide = encoded_info(run(&fast_keeping_any_saving(&wide_gamut), dir.path()).await).await;

	assert_eq!(
		full.streams[0].color_range.as_deref(),
		Some("pc"),
		"{:?}",
		full.streams[0]
	);
	assert_eq!(
		wide.streams[0].color_transfer.as_deref(),
		Some("bt2020-10"),
		"{:?}",
		wide.streams[0]
	);
}

#[tokio::test]
async fn mp4_subtitles_chapters_cover_and_timecode_are_carried_over() {
	let dir = tempfile::tempdir().unwrap();
	let subs = dir.path().join("subs.srt");
	fs::write(&subs, "1\n00:00:00,000 --> 00:00:01,000\nHello\n").unwrap();
	let chapters = dir.path().join("chapters.txt");
	fs::write(
		&chapters,
		";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=1000\ntitle=One\n\
		 [CHAPTER]\nTIMEBASE=1/1000\nSTART=1000\nEND=2000\ntitle=Two\n\
		 [CHAPTER]\nTIMEBASE=1/1000\nSTART=2000\nEND=3000\ntitle=Three\n",
	)
	.unwrap();
	let cover = dir.path().join("cover.jpg");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=64x64",
		"-frames:v",
		"1",
		cover.to_str().unwrap(),
	]);
	let clip = support::video(
		dir.path(),
		"everything.mp4",
		3,
		&[
			"-i",
			subs.to_str().unwrap(),
			"-f",
			"ffmetadata",
			"-i",
			chapters.to_str().unwrap(),
			"-i",
			cover.to_str().unwrap(),
			"-map",
			"0:v",
			"-map",
			"1:a",
			"-map",
			"2:s",
			"-map",
			"4:v",
			"-map_chapters",
			"3",
			"-c:v:0",
			"mpeg4",
			"-q:v",
			"2",
			"-c:v:1",
			"copy",
			"-disposition:v:1",
			"attached_pic",
			"-c:a",
			"aac",
			"-c:s",
			"mov_text",
			"-timecode",
			"00:00:10:00",
			"-write_tmcd",
			"1",
		],
	);

	let info = encoded_info(run(&fast_keeping_any_saving(&clip), dir.path()).await).await;

	let codecs: Vec<&str> = info.streams.iter().map(|stream| stream.codec_name.as_str()).collect();
	assert!(codecs.contains(&"subrip"), "{codecs:?}");
	assert!(!codecs.contains(&"bin_data"), "{codecs:?}");
	assert_eq!(info.chapters.len(), 3);
	assert!(
		info.streams
			.iter()
			.any(|stream| stream.tag("filename") == Some("cover.jpg")),
		"{info:?}"
	);
}

#[tokio::test]
async fn silent_variable_rate_and_misnamed_videos_are_encoded() {
	let dir = tempfile::tempdir().unwrap();
	let silent = dir.path().join("silent.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=320x240:r=25:d=2",
		"-c:v",
		"mpeg4",
		"-q:v",
		"2",
		silent.to_str().unwrap(),
	]);
	let variable = dir.path().join("variable.mkv");
	support::ffmpeg(&[
		"-filter_complex",
		"testsrc2=s=320x240:r=25:d=1[a];testsrc2=s=320x240:r=50:d=1[b];[a][b]concat=n=2:v=1",
		"-fps_mode",
		"passthrough",
		"-c:v",
		"libx264",
		"-crf",
		"12",
		variable.to_str().unwrap(),
	]);
	let misnamed = dir.path().join("actually-video.mp3");
	fs::rename(support::mpeg4(dir.path(), "clip.mp4", 2), &misnamed).unwrap();

	for source in [&silent, &variable, &misnamed] {
		let outcome = run(&fast_keeping_any_saving(source), dir.path()).await;
		assert!(
			matches!(&outcome, JobOutcome::Encoded { output, .. } if output.extension().unwrap() == "mkv"),
			"{}: {outcome:?}",
			source.display()
		);
	}
}

#[tokio::test]
async fn truncated_matroska_and_webm_fail_before_encoding() {
	let dir = tempfile::tempdir().unwrap();
	let matroska = support::video(
		dir.path(),
		"cut.mkv",
		4,
		&["-map", "0:v", "-map", "1:a", "-c:v", "mpeg4", "-c:a", "flac"],
	);
	let webm = support::video(
		dir.path(),
		"cut.webm",
		4,
		&[
			"-map",
			"0:v",
			"-map",
			"1:a",
			"-c:v",
			"libvpx-vp9",
			"-deadline",
			"realtime",
			"-cpu-used",
			"8",
			"-c:a",
			"libopus",
		],
	);

	for (source, tenths) in [(&matroska, 6), (&webm, 5)] {
		let bytes = fs::read(source).unwrap();
		fs::write(source, &bytes[..bytes.len() * tenths / 10]).unwrap();

		let outcome = run(&fast(source), dir.path()).await;

		assert!(
			matches!(&outcome, JobOutcome::Failed(err) if matches!(err.kind, ErrorKind::SourceDamaged(_))),
			"{}: {outcome:?}",
			source.display()
		);
	}
	assert!(!dir.path().join(CLI_WORK_DIR).exists());
}

#[tokio::test]
async fn garbage_in_the_middle_of_an_mp4_is_carried_over_with_a_warning() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::mpeg4(dir.path(), "glitched.mp4", 6);
	let mut bytes = fs::read(&clip).unwrap();
	let middle = bytes.len() / 2;
	bytes[middle..middle + 16 * 1024].fill(0x55);
	fs::write(&clip, bytes).unwrap();

	let outcome = run(&fast_keeping_any_saving(&clip), dir.path()).await;

	let JobOutcome::Encoded { warnings, .. } = outcome else {
		panic!("{outcome:?}")
	};
	assert!(
		warnings.iter().any(|warning| warning.contains("decoding errors")),
		"{warnings:?}"
	);
}

#[tokio::test]
async fn interlaced_ntsc_with_surround_keeps_double_rate_and_5_1() {
	let dir = tempfile::tempdir().unwrap();
	let interlaced = dir.path().join("ntsc.mpg");
	let surround = (0..6)
		.map(|i| format!("0.3*sin(2*PI*{}*t)", 220 + i * 110))
		.collect::<Vec<_>>()
		.join("|");
	let surround = format!("aevalsrc={surround}:c=5.1(side):s=48000:d=3");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=720x480:r=60000/1001:d=3,interlace=scan=tff,setfield=tff",
		"-f",
		"lavfi",
		"-i",
		surround.as_str(),
		"-c:v",
		"mpeg2video",
		"-b:v",
		"12M",
		"-flags",
		"+ilme+ildct",
		"-c:a",
		"ac3",
		"-b:a",
		"448k",
		interlaced.to_str().unwrap(),
	]);

	let info = encoded_info(run(&fast(&interlaced), dir.path()).await).await;

	assert!(
		(info.streams[0].fps().unwrap() - 60_000.0 / 1001.0).abs() < 0.01,
		"{:?}",
		info.streams[0]
	);
	assert_eq!(
		info.streams[1].channel_layout.as_deref(),
		Some("5.1"),
		"{:?}",
		info.streams[1]
	);
}

#[tokio::test]
async fn an_efficient_transparent_webm_is_kept() {
	let dir = tempfile::tempdir().unwrap();
	let webm = dir.path().join("transparent.webm");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=320x240:r=25:d=2,format=yuva420p,geq=lum='lum(X,Y)':cb='cb(X,Y)':cr='cr(X,Y)':a='if(lt(X,160),255,0)'",
		"-c:v",
		"libvpx-vp9",
		"-pix_fmt",
		"yuva420p",
		"-crf",
		"63",
		"-b:v",
		"0",
		"-deadline",
		"realtime",
		"-cpu-used",
		"8",
		webm.to_str().unwrap(),
	]);

	let outcome = run(&fast(&webm), dir.path()).await;

	assert!(
		matches!(
			&outcome,
			JobOutcome::KeptOriginal {
				reason: KeepReason::AlreadyEfficient(reason)
			} if reason.contains("VP9")
		),
		"{outcome:?}"
	);
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

async fn run_recording(engine: &Engine, job: &Job, out_dir: &Path) -> (JobOutcome, Vec<JobEvent>) {
	let events = Mutex::new(Vec::new());
	let outcome = engine
		.run(
			job,
			&sink(out_dir),
			&|event| events.lock().unwrap().push(event),
			&CancellationToken::new(),
		)
		.await;
	(outcome, events.into_inner().unwrap())
}

fn stages(events: &[JobEvent], stage: Stage) -> usize {
	events.iter().filter(|event| **event == JobEvent::Stage(stage)).count()
}

#[tokio::test]
async fn moderate_video_is_searched_checked_and_learned() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::mpeg4(dir.path(), "clip.mp4", 3);
	let memory = Arc::new(SharedMemory::default());
	let engine = Engine::new(support::toolchain().await).with_quality_memory(memory.clone());

	let (outcome, events) = run_recording(&engine, &job(&clip), dir.path()).await;

	assert!(matches!(outcome, JobOutcome::Encoded { .. }), "{outcome:?}");
	assert_eq!(stages(&events, Stage::Sample), 1, "{events:?}");
	assert_eq!(
		stages(&events, Stage::Verify),
		2,
		"the VMAF check and the final checks: {events:?}"
	);
	assert_eq!(stages(&events, Stage::Encode), 1, "{events:?}");
	let entries = memory.entries();
	assert_eq!(entries.len(), 1, "{entries:?}");
	assert!(
		entries[0].0.starts_with("video/mpeg4/sd/fps25/sdr8/progressive/"),
		"{entries:?}"
	);
	assert!(entries[0].0.contains("|p5|"), "{entries:?}");
	assert!(entries[0].1.bpp.is_some_and(|bpp| bpp > 0.0), "{entries:?}");
}

#[tokio::test]
async fn a_learned_group_skips_the_search_at_moderate() {
	let dir = tempfile::tempdir().unwrap();
	let first = support::mpeg4(dir.path(), "first.mp4", 3);
	let second = support::mpeg4(dir.path(), "second.mp4", 3);
	let memory = Arc::new(SharedMemory::default());
	let engine = Engine::new(support::toolchain().await).with_quality_memory(memory.clone());

	run_recording(&engine, &job(&first), dir.path()).await;
	let (outcome, events) = run_recording(&engine, &job(&second), dir.path()).await;

	assert!(matches!(outcome, JobOutcome::Encoded { .. }), "{outcome:?}");
	assert_eq!(stages(&events, Stage::Sample), 0, "{events:?}");
	assert_eq!(stages(&events, Stage::Verify), 2, "{events:?}");
	assert_eq!(memory.entries()[0].1.observations, 2);
}

#[tokio::test]
async fn fast_video_is_never_measured_or_learned() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::mpeg4(dir.path(), "clip.mp4", 3);
	let memory = Arc::new(SharedMemory::default());
	let engine = Engine::new(support::toolchain().await).with_quality_memory(memory.clone());

	let (outcome, events) = run_recording(&engine, &fast(&clip), dir.path()).await;

	assert!(matches!(outcome, JobOutcome::Encoded { .. }), "{outcome:?}");
	assert_eq!(stages(&events, Stage::Sample), 0, "{events:?}");
	assert_eq!(stages(&events, Stage::Verify), 1, "{events:?}");
	assert!(memory.entries().is_empty());
}

#[tokio::test]
async fn a_missed_check_encodes_the_video_once_more_at_most() {
	let dir = tempfile::tempdir().unwrap();
	let first = support::mpeg4(dir.path(), "first.mp4", 3);
	let second = support::mpeg4(dir.path(), "second.mp4", 3);
	let tools = support::toolchain().await;
	let learned = Arc::new(SharedMemory::default());
	run_recording(
		&Engine::new(tools.clone()).with_quality_memory(learned.clone()),
		&job(&first),
		dir.path(),
	)
	.await;
	let (key, stats) = learned.entries().remove(0);
	let careless = Arc::new(SharedMemory::with_entries([(
		key,
		GroupStats { setting: 55.0, ..stats },
	)]));
	let engine = Engine::new(tools).with_quality_memory(careless);

	let (outcome, events) = run_recording(&engine, &job(&second), dir.path()).await;

	assert_eq!(stages(&events, Stage::Encode), 2, "{events:?}");
	match outcome {
		JobOutcome::Encoded { warnings, .. } => assert!(
			warnings.iter().any(|warning| warning.contains("encoded again at CRF")),
			"{warnings:?}"
		),
		JobOutcome::KeptOriginal {
			reason: KeepReason::QualityUnreachable(_) | KeepReason::NotSmaller { .. },
		} => {}
		other => panic!("{other:?}"),
	}
	assert!(!dir.path().join(CLI_WORK_DIR).exists());
}

#[tokio::test]
async fn interlaced_moderate_video_is_verified_at_double_rate() {
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
	let memory = Arc::new(SharedMemory::default());
	let engine = Engine::new(support::toolchain().await).with_quality_memory(memory.clone());

	let (outcome, events) = run_recording(&engine, &job(&interlaced), dir.path()).await;

	assert!(matches!(outcome, JobOutcome::Encoded { .. }), "{outcome:?}");
	assert_eq!(stages(&events, Stage::Verify), 2, "{events:?}");
	let entries = memory.entries();
	assert!(
		entries[0].0.starts_with("video/mpeg2/sd/fps50/sdr8/interlaced/"),
		"{entries:?}"
	);
}

#[tokio::test]
async fn cancelling_the_search_leaves_nothing_behind() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::mpeg4(dir.path(), "clip.mp4", 20);
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

	let outcome = engine.run(&job(&clip), &sink(dir.path()), &on_event, &cancel).await;

	assert!(
		matches!(&outcome, JobOutcome::Failed(err) if err.kind == ErrorKind::Cancelled),
		"{outcome:?}"
	);
	assert!(!dir.path().join(CLI_WORK_DIR).exists());
	assert!(!dir.path().join("clip.cry.mkv").exists());
}

#[tokio::test]
async fn a_learned_crf_too_low_for_the_size_gate_is_searched_again() {
	let dir = tempfile::tempdir().unwrap();
	let short = support::mpeg4(dir.path(), "short.mp4", 3);
	let easy = support::mpeg4(dir.path(), "easy.mp4", 125);
	let tools = support::toolchain().await;
	let learned = Arc::new(SharedMemory::default());
	run_recording(
		&Engine::new(tools.clone()).with_quality_memory(learned.clone()),
		&job(&short),
		dir.path(),
	)
	.await;
	let (key, stats) = learned.entries().remove(0);
	let hard_won = Arc::new(SharedMemory::with_entries([(
		key,
		GroupStats { setting: 10.0, ..stats },
	)]));
	let engine = Engine::new(tools).with_quality_memory(hard_won.clone());
	let demanding = Job {
		min_savings: 0.6,
		..job(&easy)
	};

	let (outcome, events) = run_recording(&engine, &demanding, dir.path()).await;

	assert!(matches!(outcome, JobOutcome::Encoded { .. }), "{outcome:?}");
	assert_eq!(
		stages(&events, Stage::Sample),
		2,
		"the size gate, then the search: {events:?}"
	);
	let group = hard_won.entries().remove(0).1;
	assert_eq!(group.observations, 2, "{group:?}");
	assert!(group.setting > 10.0, "{group:?}");
}

async fn learned_group(dir: &Path, setting: f64) -> SharedMemory {
	let first = support::mpeg4(dir, "first.mp4", 3);
	let learned = Arc::new(SharedMemory::default());
	let engine = Engine::new(support::toolchain().await).with_quality_memory(learned.clone());
	run_recording(&engine, &job(&first), dir).await;

	let (key, stats) = learned.entries().remove(0);
	SharedMemory::with_entries([(key, GroupStats { setting, ..stats })])
}

#[tokio::test]
async fn a_learned_crf_whose_output_is_not_smaller_is_forgotten() {
	let dir = tempfile::tempdir().unwrap();
	let short = support::mpeg4(dir.path(), "short.mp4", 3);
	let next = support::mpeg4(dir.path(), "next.mp4", 3);
	let memory = Arc::new(learned_group(dir.path(), 10.0).await);
	let engine = Engine::new(support::toolchain().await).with_quality_memory(memory.clone());
	let demanding = |source: &Path| Job {
		min_savings: 0.95,
		..job(source)
	};

	let (outcome, events) = run_recording(&engine, &demanding(&short), dir.path()).await;

	assert!(
		matches!(
			outcome,
			JobOutcome::KeptOriginal {
				reason: KeepReason::NotSmaller { .. }
			}
		),
		"{outcome:?}"
	);
	assert_eq!(
		stages(&events, Stage::Sample),
		0,
		"the learned CRF was used: {events:?}"
	);
	assert!(memory.entries().is_empty(), "{:?}", memory.entries());

	let (_, events) = run_recording(&engine, &demanding(&next), dir.path()).await;

	assert_eq!(stages(&events, Stage::Sample), 1, "the next clip searches: {events:?}");
	assert!(memory.entries().is_empty(), "{:?}", memory.entries());
}

#[tokio::test]
async fn a_learned_crf_that_misses_its_check_and_its_retry_is_forgotten() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::mpeg4(dir.path(), "clip.mp4", 3);
	let next = support::mpeg4(dir.path(), "next.mp4", 3);
	let (key, stats) = learned_group(dir.path(), 55.0).await.entries().remove(0);
	assert!(key.ends_with("|t93.0/89.0"), "{key}");
	let strict_key = key.replace("|t93.0/89.0", "|t99.0/98.0");
	let memory = Arc::new(SharedMemory::with_entries([(strict_key, stats)]));
	let engine = Engine::new(support::toolchain().await).with_quality_memory(memory.clone());
	let strict = |source: &Path| Job {
		overrides: Overrides {
			video: Some(VmafOverride {
				mean: 99.0,
				floor: 98.0,
			}),
			..Overrides::default()
		},
		..job(source)
	};

	let (outcome, events) = run_recording(&engine, &strict(&clip), dir.path()).await;

	assert!(
		matches!(
			outcome,
			JobOutcome::KeptOriginal {
				reason: KeepReason::QualityUnreachable(_)
			}
		),
		"{outcome:?}"
	);
	assert_eq!(
		stages(&events, Stage::Sample),
		0,
		"the learned CRF was used: {events:?}"
	);
	assert_eq!(
		stages(&events, Stage::Encode),
		2,
		"encoded, then encoded again: {events:?}"
	);
	assert!(memory.entries().is_empty(), "{:?}", memory.entries());

	let (_, events) = run_recording(&engine, &strict(&next), dir.path()).await;

	assert_eq!(stages(&events, Stage::Sample), 1, "the next clip searches: {events:?}");
}

#[tokio::test]
async fn a_pass_with_room_to_spare_raises_the_learned_crf() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::mpeg4(dir.path(), "clip.mp4", 3);
	let memory = Arc::new(learned_group(dir.path(), 20.0).await);
	let engine = Engine::new(support::toolchain().await).with_quality_memory(memory.clone());
	let keeping_any_saving = Job {
		min_savings: 0.0,
		..job(&clip)
	};

	let (outcome, events) = run_recording(&engine, &keeping_any_saving, dir.path()).await;

	assert!(matches!(outcome, JobOutcome::Encoded { .. }), "{outcome:?}");
	assert_eq!(
		stages(&events, Stage::Sample),
		0,
		"the learned CRF was used: {events:?}"
	);
	let group = memory.entries().remove(0).1;
	assert_eq!(group.observations, 2, "{group:?}");
	assert!(group.setting > 20.0, "{group:?}");
}

#[tokio::test]
async fn a_job_cancelled_after_a_passing_check_learns_nothing() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::mpeg4(dir.path(), "clip.mp4", 3);
	let memory = Arc::new(SharedMemory::default());
	let engine = Engine::new(support::toolchain().await).with_quality_memory(memory.clone());
	let cancel = CancellationToken::new();
	let trigger = cancel.clone();
	let events = Mutex::new(Vec::new());
	let on_event = |event: JobEvent| {
		if event == JobEvent::Stage(Stage::Mux) {
			trigger.cancel();
		}
		events.lock().unwrap().push(event);
	};

	let outcome = engine.run(&job(&clip), &sink(dir.path()), &on_event, &cancel).await;

	assert!(
		matches!(&outcome, JobOutcome::Failed(err) if err.kind == ErrorKind::Cancelled),
		"{outcome:?}"
	);
	let events = events.into_inner().unwrap();
	assert_eq!(stages(&events, Stage::Verify), 1, "the VMAF check ran: {events:?}");
	assert!(memory.entries().is_empty(), "{:?}", memory.entries());
	assert!(!dir.path().join(CLI_WORK_DIR).exists());
	assert!(!dir.path().join("clip.cry.mkv").exists());
}

#[tokio::test]
async fn a_video_of_unknown_duration_is_encoded_at_the_table_crf_and_checked() {
	let dir = tempfile::tempdir().unwrap();
	let live = dir.path().join("live.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=320x240:r=25:d=3",
		"-c:v",
		"mpeg4",
		"-q:v",
		"2",
		"-live",
		"1",
		live.to_str().unwrap(),
	]);
	let memory = Arc::new(SharedMemory::default());
	let engine = Engine::new(support::toolchain().await).with_quality_memory(memory.clone());

	let (outcome, events) = run_recording(&engine, &job(&live), dir.path()).await;

	let JobOutcome::Encoded { warnings, .. } = outcome else {
		panic!("{outcome:?}")
	};
	assert!(
		warnings.iter().any(|warning| warning.contains("not searched")),
		"{warnings:?}"
	);
	assert_eq!(stages(&events, Stage::Sample), 0, "{events:?}");
	assert_eq!(stages(&events, Stage::Verify), 2, "{events:?}");
	assert_eq!(memory.entries().len(), 1);
}

#[tokio::test]
async fn a_long_searched_video_is_predicted_from_the_search_samples() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::mpeg4(dir.path(), "long.mp4", 125);
	let engine = Engine::new(support::toolchain().await);
	let demanding = Job {
		min_savings: 1.0,
		..job(&clip)
	};

	let (outcome, events) = run_recording(&engine, &demanding, dir.path()).await;

	assert!(
		matches!(
			outcome,
			JobOutcome::KeptOriginal {
				reason: KeepReason::PredictedNotSmaller { .. }
			}
		),
		"{outcome:?}"
	);
	assert_eq!(stages(&events, Stage::Sample), 1, "{events:?}");
	assert_eq!(stages(&events, Stage::Encode), 0, "{events:?}");
	assert!(!dir.path().join(CLI_WORK_DIR).exists());
}

#[tokio::test]
async fn a_search_that_cannot_reach_the_target_keeps_the_original_before_encoding() {
	let dir = tempfile::tempdir().unwrap();
	let faint_noise = dir.path().join("faint-noise.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"color=c=gray:s=320x240:r=25:d=3,noise=alls=10:allf=t",
		"-pix_fmt",
		"yuv420p",
		"-c:v",
		"ffv1",
		faint_noise.to_str().unwrap(),
	]);
	let memory = Arc::new(SharedMemory::default());
	let engine = Engine::new(support::toolchain().await).with_quality_memory(memory.clone());

	let (outcome, events) = run_recording(&engine, &job(&faint_noise), dir.path()).await;

	let JobOutcome::KeptOriginal {
		reason: KeepReason::QualityUnreachable(reason),
	} = outcome
	else {
		panic!("{outcome:?}")
	};
	assert!(
		reason.contains("target 93.0 / 89.0") || reason.contains("target 91.0 / 86.5"),
		"{reason}"
	);
	assert_eq!(stages(&events, Stage::Sample), 1, "{events:?}");
	assert!(events.contains(&JobEvent::Progress(0.75)), "four probes: {events:?}");
	assert_eq!(stages(&events, Stage::Encode), 0, "{events:?}");
	assert!(memory.entries().is_empty());
	assert!(!dir.path().join(CLI_WORK_DIR).exists());
}

fn requested(source: &Path, format: OutputFormat) -> Job {
	Job {
		options: JobOptions {
			format: Some(format),
			..JobOptions::default()
		},
		..fast(source)
	}
}

fn warned(outcome: &JobOutcome, text: &str) -> bool {
	matches!(outcome, JobOutcome::Encoded { warnings, .. } if warnings.iter().any(|warning| warning.contains(text)))
}

#[tokio::test]
async fn a_requested_container_remuxes_an_efficient_opus_source() {
	let dir = tempfile::tempdir().unwrap();
	let opus = support::tone(
		dir.path(),
		"low.opus",
		"mono",
		1,
		3,
		&["-c:a", "libopus", "-b:a", "32k"],
	);

	let outcome = run(&requested(&opus, OutputFormat::Mka), dir.path()).await;

	assert!(
		matches!(&outcome, JobOutcome::Encoded { output, .. } if *output == dir.path().join("low.cry.mka")),
		"{outcome:?}"
	);
	assert!(warned(&outcome, "because it was requested"), "{outcome:?}");
}

#[tokio::test]
async fn a_format_for_another_kind_of_media_fails_that_file() {
	let dir = tempfile::tempdir().unwrap();
	let song = support::tone(dir.path(), "song.flac", "stereo", 2, 2, &["-c:a", "flac"]);
	let photo = support::photo_png(dir.path(), "photo.png", 64, 48);

	let audio = run(&requested(&song, OutputFormat::Mkv), dir.path()).await;
	let image = run(&requested(&photo, OutputFormat::Opus), dir.path()).await;

	assert!(
		matches!(&audio, JobOutcome::Failed(err) if err.to_string().contains("an audio file can't be written as .mkv")),
		"{audio:?}"
	);
	assert!(
		matches!(&image, JobOutcome::Failed(err) if err.to_string().contains("an image can't be written as .opus")),
		"{image:?}"
	);
	assert!(!dir.path().join(CLI_WORK_DIR).exists());
}

#[tokio::test]
async fn a_video_asked_for_as_opus_keeps_only_its_audio() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::mpeg4(dir.path(), "clip.mp4", 2);

	let outcome = run(&requested(&clip, OutputFormat::Opus), dir.path()).await;

	assert!(
		matches!(&outcome, JobOutcome::Encoded { output, .. } if *output == dir.path().join("clip.cry.opus")),
		"{outcome:?}"
	);
	let info = encoded_info(outcome).await;
	assert_eq!(info.audio_streams().count(), 1);
	assert!(!info.streams.iter().any(Stream::is_video));
}

#[tokio::test]
async fn single_track_formats_refuse_several_tracks() {
	let dir = tempfile::tempdir().unwrap();
	let two = dir.path().join("two.mka");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"sine=duration=2",
		"-f",
		"lavfi",
		"-i",
		"sine=frequency=880:duration=2",
		"-map",
		"0",
		"-map",
		"1",
		"-c:a",
		"flac",
		two.to_str().unwrap(),
	]);

	let outcome = run(&requested(&two, OutputFormat::Opus), dir.path()).await;

	assert!(
		matches!(&outcome, JobOutcome::Failed(err) if err.to_string().contains("holds one audio track")),
		"{outcome:?}"
	);
}

#[tokio::test]
async fn an_efficient_av1_source_is_encoded_when_mkv_is_requested() {
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

	let outcome = run(&requested(&av1, OutputFormat::Mkv), dir.path()).await;

	assert!(matches!(outcome, JobOutcome::Encoded { .. }), "{outcome:?}");
}

#[tokio::test]
async fn a_requested_video_that_misses_its_target_is_written_and_learns_nothing() {
	let dir = tempfile::tempdir().unwrap();
	let faint_noise = dir.path().join("faint-noise.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"color=c=gray:s=320x240:r=25:d=3,noise=alls=10:allf=t",
		"-pix_fmt",
		"yuv420p",
		"-c:v",
		"ffv1",
		faint_noise.to_str().unwrap(),
	]);
	let memory = Arc::new(SharedMemory::default());
	let engine = Engine::new(support::toolchain().await).with_quality_memory(memory.clone());
	let moderate = Job {
		speed: Speed::Moderate,
		..requested(&faint_noise, OutputFormat::Mkv)
	};

	let (outcome, _) = run_recording(&engine, &moderate, dir.path()).await;

	assert!(
		matches!(&outcome, JobOutcome::Encoded { output, .. } if *output == dir.path().join("faint-noise.cry.mkv")),
		"{outcome:?}"
	);
	let warning = quality_warning(&outcome);
	assert!(
		warning.starts_with("quality target not reached (VMAF ")
			&& warning.ends_with(" without a full check because it was requested"),
		"{warning}"
	);
	assert_eq!(
		(warning.matches("VMAF").count(), warning.matches("CRF").count()),
		(1, 2),
		"{warning}"
	);
	assert!(memory.entries().is_empty(), "{:?}", memory.entries());
}

fn quality_warning(outcome: &JobOutcome) -> String {
	let JobOutcome::Encoded { warnings, .. } = outcome else {
		panic!("{outcome:?}")
	};
	let found: Vec<&String> = warnings
		.iter()
		.filter(|warning| warning.contains("quality target not reached"))
		.collect();
	assert_eq!(found.len(), 1, "{warnings:?}");
	assert!(!found[0].contains("at a smaller size"), "{warnings:?}");
	found[0].clone()
}

#[tokio::test]
async fn a_learned_crf_whose_requested_output_misses_its_check_and_its_retry_is_forgotten() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::mpeg4(dir.path(), "clip.mp4", 3);
	let (key, stats) = learned_group(dir.path(), 55.0).await.entries().remove(0);
	let strict_key = key.replace("|t93.0/89.0", "|t99.0/98.0");
	let memory = Arc::new(SharedMemory::with_entries([(strict_key, stats)]));
	let engine = Engine::new(support::toolchain().await).with_quality_memory(memory.clone());
	let strict = Job {
		overrides: Overrides {
			video: Some(VmafOverride {
				mean: 99.0,
				floor: 98.0,
			}),
			..Overrides::default()
		},
		options: requested(&clip, OutputFormat::Mkv).options,
		..job(&clip)
	};

	let (outcome, events) = run_recording(&engine, &strict, dir.path()).await;

	let warning = quality_warning(&outcome);
	assert!(
		warning.starts_with("quality target not reached (VMAF ")
			&& warning.ends_with("); it was written because it was requested"),
		"{warning}"
	);
	assert_eq!(
		(warning.matches("VMAF").count(), warning.matches("CRF").count()),
		(1, 1),
		"{warning}"
	);
	assert_eq!(
		stages(&events, Stage::Sample),
		0,
		"the learned CRF was used: {events:?}"
	);
	assert_eq!(
		stages(&events, Stage::Encode),
		2,
		"encoded, then encoded again: {events:?}"
	);
	assert!(memory.entries().is_empty(), "{:?}", memory.entries());
}

fn position(bytes: &[u8], needle: &[u8]) -> Option<usize> {
	bytes.windows(needle.len()).position(|window| window == needle)
}

#[tokio::test]
async fn webm_output_converts_subtitles_to_webvtt_and_drops_attachments() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::clip_with_extras(dir.path());

	let outcome = run(&requested(&clip, OutputFormat::Webm), dir.path()).await;

	let JobOutcome::Encoded { output, warnings, .. } = outcome else {
		panic!("{outcome:?}")
	};
	assert_eq!(output, dir.path().join("extras.cry.webm"));
	assert!(
		warnings.iter().any(|warning| warning.contains("can't hold them")),
		"{warnings:?}"
	);
	let info = probe(&support::toolchain().await, &output, &CancellationToken::new())
		.await
		.unwrap();
	let codecs: Vec<&str> = info.streams.iter().map(|stream| stream.codec_name.as_str()).collect();
	assert_eq!(codecs, ["av1", "opus", "webvtt"]);
	assert_eq!(info.chapters.len(), 3);
}

#[tokio::test]
async fn mp4_output_has_mov_text_chapters_and_its_index_in_front() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::clip_with_extras(dir.path());

	let outcome = run(&requested(&clip, OutputFormat::Mp4), dir.path()).await;

	let JobOutcome::Encoded { output, .. } = outcome else {
		panic!("{outcome:?}")
	};
	let info = probe(&support::toolchain().await, &output, &CancellationToken::new())
		.await
		.unwrap();
	let codecs: Vec<&str> = info
		.streams
		.iter()
		.filter(|stream| stream.codec_type != "data")
		.map(|stream| stream.codec_name.as_str())
		.collect();
	assert_eq!(codecs, ["av1", "opus", "mov_text"]);
	assert_eq!(info.chapters.len(), 3);
	let bytes = fs::read(&output).unwrap();
	assert!(
		position(&bytes, b"moov") < position(&bytes, b"mdat"),
		"faststart puts the index first"
	);
}

#[tokio::test]
async fn transparent_video_can_not_be_written_as_mp4() {
	let dir = tempfile::tempdir().unwrap();
	let alpha = dir.path().join("alpha.webm");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=160x120:d=1,format=yuva420p",
		"-c:v",
		"libvpx-vp9",
		"-pix_fmt",
		"yuva420p",
		alpha.to_str().unwrap(),
	]);

	let outcome = run(&requested(&alpha, OutputFormat::Mp4), dir.path()).await;

	assert!(
		matches!(&outcome, JobOutcome::Failed(err) if err.to_string().contains("can't be stored in MP4")),
		"{outcome:?}"
	);
	assert!(!dir.path().join(CLI_WORK_DIR).exists());
}
