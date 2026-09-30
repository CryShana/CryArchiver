mod support;

use std::fs::{self, File};
use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use cryarchiver_engine::engine::{Encode, Engine, Job, JobEvent, MemoryGate, Stage};
use cryarchiver_engine::format::OutputFormat;
use cryarchiver_engine::images::plan::worst_case_memory;
use cryarchiver_engine::options::{Advanced, BurnSubs, JobOptions, SubtitlePick, Trim, VideoEncoderChoice};
use cryarchiver_engine::outcome::{ErrorClass, ErrorKind, JobOutcome, KeepReason};
use cryarchiver_engine::preset::{Overrides, Speed, VmafOverride};
use cryarchiver_engine::probe::{self, Stream, probe};
use cryarchiver_engine::quality::memory::{GroupStats, SharedMemory};
use cryarchiver_engine::sink::{ArchivalSink, CliSink};
use cryarchiver_engine::toolchain::LIBAOM;
use cryarchiver_engine::video::plan::synthesized_grain;
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
		output_file: None,
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
async fn an_eight_bit_source_keeps_its_chroma_location() {
	let dir = tempfile::tempdir().unwrap();

	for location in ["left", "topleft"] {
		let clip = support::video(
			dir.path(),
			&format!("chroma-{location}.mkv"),
			2,
			&[
				"-map",
				"0:v",
				"-map",
				"1:a",
				"-c:v",
				"mpeg4",
				"-pix_fmt",
				"yuv420p",
				"-chroma_sample_location",
				location,
				"-c:a",
				"aac",
			],
		);

		let outcome = run(&fast(&clip), dir.path()).await;

		let JobOutcome::Encoded { output, .. } = outcome else {
			panic!("{location}: {outcome:?}")
		};
		let info = probe(&support::toolchain().await, &output, &CancellationToken::new())
			.await
			.unwrap();
		assert_eq!(info.streams[0].pix_fmt.as_deref(), Some("yuv420p10le"));
		assert_eq!(info.streams[0].chroma_location.as_deref(), Some(location));
	}
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
async fn faintly_interlaced_home_video_of_either_field_order_is_deinterlaced_to_field_rate() {
	let dir = tempfile::tempdir().unwrap();
	let truth = support::still_home_video(dir.path(), "truth.mkv", "50p");

	for (scan, wrong_parity) in [("tff", "bff"), ("bff", "tff")] {
		for container in ["mpg", "mkv"] {
			let name = format!("{scan}.{container}");
			let clip = support::still_home_video(dir.path(), &name, scan);
			let out_dir = dir.path().join(format!("{scan}-{container}"));
			fs::create_dir(&out_dir).unwrap();

			let JobOutcome::Encoded { output, .. } = run(&fast_keeping_any_saving(&clip), &out_dir).await else {
				panic!("{name} was not encoded")
			};

			let info = probe(&support::toolchain().await, &output, &CancellationToken::new())
				.await
				.unwrap();
			let (interlaced, progressive, frames) = support::idet_verdicts(&output, support::HOME_VIDEO_INSET);
			let encoded = support::psnr_in(&output, "", &truth, support::HOME_VIDEO_INSET);
			let swapped = support::psnr_in(
				&clip,
				&format!("bwdif=mode=send_field:parity={wrong_parity}:deint=all,"),
				&truth,
				support::HOME_VIDEO_INSET,
			);

			assert!(
				(info.streams[0].fps().unwrap() - 50.0).abs() < 0.5,
				"{name}: {:?}",
				info.streams[0]
			);
			assert!(
				progressive * 100 >= frames * 95,
				"{name}: {interlaced} interlaced and {progressive} progressive of {frames} frames"
			);
			assert!(
				encoded > swapped + 2.0,
				"{name}: {encoded} dB against the pictures, {swapped} dB with the fields swapped"
			);
		}
	}
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

#[tokio::test]
async fn noise_that_grain_cannot_reach_the_target_on_is_encoded_and_learned_without_grain() {
	let dir = tempfile::tempdir().unwrap();
	let noisy = |name: &str, seed: u32| {
		let path = dir.path().join(name);
		support::ffmpeg(&[
			"-f",
			"lavfi",
			"-i",
			&format!("color=c=gray:s=320x240:r=25:d=3,noise=alls=6:allf=t:all_seed={seed}"),
			"-pix_fmt",
			"yuv420p",
			"-c:v",
			"ffv1",
			path.to_str().unwrap(),
		]);
		path
	};
	let (first, second) = (noisy("first.mkv", 1), noisy("second.mkv", 2));
	let memory = Arc::new(SharedMemory::default());
	let tools = support::toolchain().await;
	let engine = Engine::new(tools.clone()).with_quality_memory(memory.clone());
	let reachable_without_grain = |source: &Path| Job {
		overrides: Overrides {
			video: Some(VmafOverride {
				mean: 85.0,
				floor: 75.0,
			}),
			..Overrides::default()
		},
		..job(source)
	};
	let grain_params = async |output: &Path| {
		let frames = probe::first_frames(&tools, output, 0, 2, &CancellationToken::new())
			.await
			.unwrap();
		synthesized_grain(&frames)
	};

	let (outcome, events) = run_recording(&engine, &reachable_without_grain(&first), dir.path()).await;

	let JobOutcome::Encoded { output, .. } = &outcome else {
		panic!("{outcome:?}")
	};
	assert!(
		warned(&outcome, "was left out: with it quality target not reached"),
		"{outcome:?}"
	);
	assert!(!grain_params(output).await, "{outcome:?}");
	assert_eq!(stages(&events, Stage::Sample), 1, "{events:?}");
	let learned = memory.entries();
	assert!(
		learned.len() == 1 && learned[0].0.contains("/grain10/") && learned[0].1.without_grain,
		"{learned:?}"
	);

	let (outcome, events) = run_recording(&engine, &reachable_without_grain(&second), dir.path()).await;

	let JobOutcome::Encoded { output, .. } = &outcome else {
		panic!("{outcome:?}")
	};
	assert_eq!(
		stages(&events, Stage::Sample),
		0,
		"the learned CRF was used: {events:?}"
	);
	assert!(!grain_params(output).await, "{outcome:?}");
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
async fn an_audio_only_output_rewrites_its_track_titles() {
	let dir = tempfile::tempdir().unwrap();
	let two = dir.path().join("two.mka");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"anoisesrc=d=3:c=pink:a=0.3:r=48000",
		"-f",
		"lavfi",
		"-i",
		"anoisesrc=d=3:c=pink:a=0.3:r=48000:s=2",
		"-map",
		"0",
		"-map",
		"1",
		"-ac",
		"2",
		"-c:a",
		"flac",
		"-metadata:s:a:0",
		"title=Japanese 2.0 FLAC",
		"-metadata:s:a:1",
		"title=Magnum opus",
		two.to_str().unwrap(),
	]);

	let outcome = run(&job(&two), dir.path()).await;

	let JobOutcome::Encoded { output, .. } = outcome else {
		panic!("{outcome:?}")
	};
	let info = probe(&support::toolchain().await, &output, &CancellationToken::new())
		.await
		.unwrap();
	let titles: Vec<Option<&str>> = info.streams.iter().map(|stream| stream.tag("title")).collect();
	assert_eq!(titles, [Some("Japanese 2.0 Opus"), Some("Magnum opus")]);
}

/// A clip whose video is tagged Japanese, with a titled AAC track and two titled ASS tracks.
fn titled_ass_clip(dir: &Path) -> std::path::PathBuf {
	let ass = dir.join("dialogue.ass");
	fs::write(
		&ass,
		"[Script Info]\nScriptType: v4.00+\n\n[V4+ Styles]\n\
		 Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, \
		 Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, \
		 MarginR, MarginV, Encoding\n\
		 Style: Default,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,1,0,0,0,100,100,0,0,1,1,0,2,10,10,10,1\n\n\
		 [Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
		 Dialogue: 0,0:00:00.50,0:00:01.50,Default,,0,0,0,,{\\b1}bold\n",
	)
	.unwrap();
	let clip = dir.join("titled.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=320x240:r=25:d=2",
		"-i",
		ass.to_str().unwrap(),
		"-f",
		"lavfi",
		"-i",
		"sine=duration=2",
		"-map",
		"0",
		"-map",
		"2",
		"-map",
		"1",
		"-map",
		"1",
		"-c:v",
		"mpeg4",
		"-c:a",
		"aac",
		"-c:s",
		"copy",
		"-metadata:s:v:0",
		"language=jpn",
		"-metadata:s:a:0",
		"title=English AAC",
		"-metadata:s:s:0",
		"title=Signs & Songs",
		"-metadata:s:s:1",
		"title=Dialogue",
		clip.to_str().unwrap(),
	]);
	clip
}

#[tokio::test]
async fn converted_ass_warns_and_mp4_keeps_track_names_and_the_video_language() {
	let dir = tempfile::tempdir().unwrap();
	let clip = titled_ass_clip(dir.path());
	let tools = support::toolchain().await;

	for (format, keep_metadata) in [
		(OutputFormat::Mp4, false),
		(OutputFormat::Mp4, true),
		(OutputFormat::Webm, false),
	] {
		let job = Job {
			keep_metadata,
			..requested(&clip, format)
		};

		let outcome = run(&job, dir.path()).await;

		let JobOutcome::Encoded { output, warnings, .. } = outcome else {
			panic!("{format}: {outcome:?}")
		};
		let styling: Vec<&String> = warnings
			.iter()
			.filter(|warning| warning.contains("styling and fonts can't be kept"))
			.collect();
		assert!(
			styling.len() == 1 && styling[0].ends_with(" (×2)"),
			"{format}: {warnings:?}"
		);

		let info = probe(&tools, &output, &CancellationToken::new()).await.unwrap();
		if format == OutputFormat::Mp4 {
			let names: Vec<Option<&str>> = info
				.streams
				.iter()
				.filter(|stream| stream.codec_type == "subtitle")
				.map(|stream| stream.tag("handler_name"))
				.collect();
			assert_eq!(names, [Some("Signs & Songs"), Some("Dialogue")], "keep {keep_metadata}");
			let audio = info.streams.iter().find(|stream| stream.is_audio()).unwrap();
			assert_eq!(audio.tag("handler_name"), Some("English Opus"), "keep {keep_metadata}");
			assert_eq!(info.streams[0].tag("language"), Some("jpn"), "keep {keep_metadata}");
		}
		fs::remove_file(&output).unwrap();
	}
}

#[tokio::test]
async fn keep_metadata_leaves_the_source_metadata_out_of_webm_and_mp4() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::mpeg4(dir.path(), "clip.mp4", 2);
	let tools = support::toolchain().await;

	for format in [OutputFormat::Webm, OutputFormat::Mp4] {
		let keeping = Job {
			keep_metadata: true,
			..requested(&clip, format)
		};

		let outcome = run(&keeping, dir.path()).await;

		let JobOutcome::Encoded { output, warnings, .. } = outcome else {
			panic!("{format}: {outcome:?}")
		};
		assert!(
			warnings
				.iter()
				.any(|warning| warning.contains("the source metadata attachment was left out")),
			"{format}: {warnings:?}"
		);
		let info = probe(&tools, &output, &CancellationToken::new()).await.unwrap();
		assert!(
			info.streams
				.iter()
				.all(|stream| stream.codec_type != "attachment" && !stream.is_attached_pic()),
			"{format}: {:?}",
			info.streams
		);
	}
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

fn with_options(job: Job, options: JobOptions) -> Job {
	Job { options, ..job }
}

#[tokio::test]
async fn a_bitrate_cap_makes_a_smaller_video() {
	let dir = tempfile::tempdir().unwrap();
	let free = support::mpeg4(dir.path(), "free.mp4", 3);
	let capped = support::mpeg4(dir.path(), "capped.mp4", 3);
	let cap = JobOptions {
		max_bitrate_kbps: Some(60),
		..JobOptions::default()
	};

	let free = run(&fast_keeping_any_saving(&free), dir.path()).await;
	let capped = run(&with_options(fast_keeping_any_saving(&capped), cap), dir.path()).await;

	let (
		JobOutcome::Encoded {
			out_bytes: free_bytes, ..
		},
		JobOutcome::Encoded {
			out_bytes: capped_bytes,
			..
		},
	) = (&free, &capped)
	else {
		panic!("{free:?} {capped:?}")
	};
	assert!(capped_bytes < free_bytes, "{capped_bytes} ≥ {free_bytes}");
}

#[tokio::test]
async fn merged_video_audio_is_one_stereo_track() {
	let dir = tempfile::tempdir().unwrap();
	let clip = dir.path().join("two-tracks.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=320x240:r=25:d=3",
		"-f",
		"lavfi",
		"-i",
		"sine=duration=3",
		"-f",
		"lavfi",
		"-i",
		"sine=frequency=880:duration=3",
		"-map",
		"0",
		"-map",
		"1",
		"-map",
		"2",
		"-c:v",
		"mpeg4",
		"-c:a",
		"aac",
		"-ac:a:0",
		"2",
		clip.to_str().unwrap(),
	]);
	let merge = JobOptions {
		merge_audio: true,
		..JobOptions::default()
	};

	let outcome = run(&with_options(fast(&clip), merge), dir.path()).await;

	let JobOutcome::Encoded { output, .. } = outcome else {
		panic!("{outcome:?}")
	};
	let info = probe(&support::toolchain().await, &output, &CancellationToken::new())
		.await
		.unwrap();
	let audio: Vec<&Stream> = info.audio_streams().collect();
	assert_eq!(audio.len(), 1);
	assert_eq!(audio[0].channels, Some(2));
}

#[tokio::test]
async fn merged_mono_tracks_become_one_mono_opus() {
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
	let merge = JobOptions {
		merge_audio: true,
		..JobOptions::default()
	};

	let outcome = run(&with_options(job(&two), merge), dir.path()).await;

	let JobOutcome::Encoded { output, .. } = outcome else {
		panic!("{outcome:?}")
	};
	assert_eq!(output, dir.path().join("two.cry.opus"));
	let info = probe(&support::toolchain().await, &output, &CancellationToken::new())
		.await
		.unwrap();
	assert_eq!(
		info.audio_streams().map(|stream| stream.channels).collect::<Vec<_>>(),
		[Some(1)]
	);
}

#[tokio::test]
async fn merge_audio_on_a_single_track_file_changes_nothing() {
	let dir = tempfile::tempdir().unwrap();
	let opus = support::tone(
		dir.path(),
		"low.opus",
		"mono",
		1,
		3,
		&["-c:a", "libopus", "-b:a", "32k"],
	);
	let merge = JobOptions {
		merge_audio: true,
		..JobOptions::default()
	};

	let outcome = run(&with_options(job(&opus), merge), dir.path()).await;

	assert!(
		matches!(
			&outcome,
			JobOutcome::KeptOriginal {
				reason: KeepReason::AlreadyEfficient(_)
			}
		),
		"{outcome:?}"
	);
}

#[tokio::test]
async fn a_short_first_track_does_not_fail_the_merge() {
	let dir = tempfile::tempdir().unwrap();
	let uneven = dir.path().join("uneven.mka");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"sine=duration=3",
		"-f",
		"lavfi",
		"-i",
		"sine=frequency=880:duration=8",
		"-map",
		"0:a",
		"-map",
		"1:a",
		"-ac:a:1",
		"2",
		"-c:a",
		"flac",
		uneven.to_str().unwrap(),
	]);
	let merge = JobOptions {
		merge_audio: true,
		..JobOptions::default()
	};

	let outcome = run(&with_options(job(&uneven), merge), dir.path()).await;

	let JobOutcome::Encoded { output, .. } = outcome else {
		panic!("{outcome:?}")
	};
	let info = probe(&support::toolchain().await, &output, &CancellationToken::new())
		.await
		.unwrap();
	let duration = info.format.duration.unwrap();
	assert!(
		(duration - 3.0).abs() < 0.1,
		"expected ~3.0 s (the first track), got {duration} s"
	);
}

#[tokio::test]
async fn a_trimmed_merge_lasts_exactly_the_window() {
	let dir = tempfile::tempdir().unwrap();
	let two = dir.path().join("two-long.mka");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"sine=duration=6",
		"-f",
		"lavfi",
		"-i",
		"sine=frequency=880:duration=6",
		"-map",
		"0",
		"-map",
		"1",
		"-c:a",
		"flac",
		two.to_str().unwrap(),
	]);
	let options = JobOptions {
		trim: Some(Trim {
			start_s: 1.0,
			end_s: Some(4.0),
		}),
		merge_audio: true,
		..JobOptions::default()
	};

	let outcome = run(&with_options(job(&two), options), dir.path()).await;

	let JobOutcome::Encoded { output, .. } = outcome else {
		panic!("{outcome:?}")
	};
	let info = probe(&support::toolchain().await, &output, &CancellationToken::new())
		.await
		.unwrap();
	let duration = info.format.duration.unwrap();
	assert!(
		(duration - 3.0).abs() < 0.1,
		"expected the 3 s window, got {duration} s"
	);
}

#[tokio::test]
async fn a_merge_without_a_declared_duration_still_checks_the_trim_window() {
	let dir = tempfile::tempdir().unwrap();
	let two = dir.path().join("piped.mka");
	let piped = std::process::Command::new("ffmpeg")
		.args([
			"-hide_banner",
			"-v",
			"error",
			"-y",
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
			"-f",
			"matroska",
			"pipe:1",
		])
		.output()
		.unwrap();
	assert!(piped.status.success(), "{}", String::from_utf8_lossy(&piped.stderr));
	fs::write(&two, &piped.stdout).unwrap();
	let fixture = probe(&support::toolchain().await, &two, &CancellationToken::new())
		.await
		.unwrap();
	assert!(
		fixture.format.duration.is_none(),
		"the fixture must have no declared duration: {:?}",
		fixture.format
	);
	let options = JobOptions {
		trim: Some(Trim {
			start_s: 0.0,
			end_s: Some(5.0),
		}),
		merge_audio: true,
		..JobOptions::default()
	};

	let outcome = run(&with_options(job(&two), options), dir.path()).await;

	assert!(
		matches!(
			&outcome,
			JobOutcome::Failed(err) if matches!(&err.kind, ErrorKind::SourceDamaged(reason) if reason.contains("should last"))
		),
		"{outcome:?}"
	);
}

#[tokio::test]
async fn merged_extraction_mixes_a_two_track_video_to_one_opus_track() {
	let dir = tempfile::tempdir().unwrap();
	let clip = dir.path().join("two-tracks.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=320x240:r=25:d=3",
		"-f",
		"lavfi",
		"-i",
		"sine=duration=3",
		"-f",
		"lavfi",
		"-i",
		"sine=frequency=880:duration=3",
		"-map",
		"0",
		"-map",
		"1",
		"-map",
		"2",
		"-c:v",
		"mpeg4",
		"-c:a",
		"aac",
		clip.to_str().unwrap(),
	]);
	let options = JobOptions {
		format: Some(OutputFormat::Opus),
		merge_audio: true,
		..JobOptions::default()
	};

	let outcome = run(&with_options(fast(&clip), options), dir.path()).await;

	assert!(
		matches!(&outcome, JobOutcome::Encoded { output, .. } if *output == dir.path().join("two-tracks.cry.opus")),
		"{outcome:?}"
	);
	let info = encoded_info(outcome).await;
	assert_eq!(info.audio_streams().count(), 1);
	assert!(!info.streams.iter().any(Stream::is_video));
}

#[tokio::test]
async fn merging_keeps_every_source_audible() {
	let dir = tempfile::tempdir().unwrap();
	let two = dir.path().join("two-tones.mka");
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
	let merge = JobOptions {
		merge_audio: true,
		..JobOptions::default()
	};

	let outcome = run(&with_options(job(&two), merge), dir.path()).await;

	let JobOutcome::Encoded { output, .. } = outcome else {
		panic!("{outcome:?}")
	};
	let filtered = std::process::Command::new("ffmpeg")
		.args(["-hide_banner", "-i"])
		.arg(&output)
		.args(["-af", "highpass=f=660,volumedetect", "-f", "null", "-"])
		.output()
		.unwrap();
	let log = String::from_utf8_lossy(&filtered.stderr);
	let mean: f64 = log
		.split("mean_volume:")
		.nth(1)
		.and_then(|rest| rest.split_whitespace().next()?.parse().ok())
		.unwrap_or_else(|| panic!("{log}"));
	assert!(
		mean > -30.0,
		"the 880 Hz source should still be audible after a 660 Hz highpass: {mean} dB\n{log}"
	);
}

#[tokio::test]
async fn stripped_video_keeps_only_the_main_video_and_the_audio() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::clip_with_extras(dir.path());
	let strip = JobOptions {
		strip: true,
		..JobOptions::default()
	};

	let outcome = run(&with_options(fast(&clip), strip), dir.path()).await;

	let JobOutcome::Encoded { output, warnings, .. } = outcome else {
		panic!("{outcome:?}")
	};
	assert!(
		warnings.iter().any(|warning| warning.starts_with("--strip left out")),
		"{warnings:?}"
	);
	let info = probe(&support::toolchain().await, &output, &CancellationToken::new())
		.await
		.unwrap();
	let kinds: Vec<&str> = info.streams.iter().map(|stream| stream.codec_type.as_str()).collect();
	assert_eq!(kinds, ["video", "audio"]);
	assert!(info.chapters.is_empty());
}

#[tokio::test]
async fn stripped_audio_has_no_cover_art() {
	let dir = tempfile::tempdir().unwrap();
	let mp3 = support::mp3_with_cover(dir.path(), "song.mp3", "Song");
	let strip = JobOptions {
		strip: true,
		..JobOptions::default()
	};

	let outcome = run(&with_options(job(&mp3), strip), dir.path()).await;

	let JobOutcome::Encoded { output, .. } = outcome else {
		panic!("{outcome:?}")
	};
	let opus = OpusFile::read_from(
		&mut File::open(&output).unwrap(),
		ParseOptions::new().read_properties(false),
	)
	.unwrap();
	assert!(opus.vorbis_comments().pictures().is_empty());
}

#[tokio::test]
async fn strip_drops_a_genuine_second_video_stream() {
	let dir = tempfile::tempdir().unwrap();
	let cues = dir.path().join("cues.srt");
	fs::write(&cues, "1\n00:00:00,000 --> 00:00:01,000\nHello\n").unwrap();
	let clip = dir.path().join("two-angle.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=64x64:r=25:d=1",
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=64x64:r=25:d=1",
		"-f",
		"lavfi",
		"-i",
		"sine=duration=1",
		"-i",
		cues.to_str().unwrap(),
		"-map",
		"0:v",
		"-map",
		"1:v",
		"-map",
		"2:a",
		"-map",
		"3:s",
		"-c:v",
		"mpeg4",
		"-q:v",
		"2",
		"-c:a",
		"aac",
		"-c:s",
		"srt",
		clip.to_str().unwrap(),
	]);
	let strip = JobOptions {
		strip: true,
		..JobOptions::default()
	};

	let outcome = run(&with_options(fast(&clip), strip), dir.path()).await;

	let JobOutcome::Encoded { output, warnings, .. } = outcome else {
		panic!("{outcome:?}")
	};
	assert!(!warnings.is_empty(), "{warnings:?}");
	let info = probe(&support::toolchain().await, &output, &CancellationToken::new())
		.await
		.unwrap();
	let video_streams = info
		.streams
		.iter()
		.filter(|stream| stream.codec_type == "video")
		.count();
	assert_eq!(video_streams, 1, "{:?}", info.streams);
}

#[tokio::test]
async fn stripped_extraction_drops_chapters_and_the_cover() {
	let dir = tempfile::tempdir().unwrap();
	let chapters = dir.path().join("chapters.txt");
	fs::write(
		&chapters,
		";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=1000\ntitle=One\n",
	)
	.unwrap();
	let clip = dir.path().join("clip.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=64x64:r=25:d=1",
		"-f",
		"lavfi",
		"-i",
		"sine=duration=1",
		"-f",
		"lavfi",
		"-i",
		"color=c=red:s=32x32",
		"-i",
		chapters.to_str().unwrap(),
		"-map",
		"0:v",
		"-map",
		"1:a",
		"-map",
		"2:v",
		"-map_chapters",
		"3",
		"-frames:v:1",
		"1",
		"-c:v:0",
		"mpeg4",
		"-q:v",
		"2",
		"-c:a",
		"aac",
		"-c:v:1",
		"png",
		"-disposition:v:1",
		"attached_pic",
		clip.to_str().unwrap(),
	]);
	let extract_opus = JobOptions {
		format: Some(OutputFormat::Opus),
		strip: true,
		..JobOptions::default()
	};

	let outcome = run(&with_options(fast(&clip), extract_opus), dir.path()).await;

	let JobOutcome::Encoded { output, warnings, .. } = outcome else {
		panic!("{outcome:?}")
	};
	assert!(
		!warnings.iter().any(|warning| warning.contains("cover art")),
		"{warnings:?}"
	);
	let opus = OpusFile::read_from(
		&mut File::open(&output).unwrap(),
		ParseOptions::new().read_properties(false),
	)
	.unwrap();
	assert!(opus.vorbis_comments().pictures().is_empty());
	let info = probe(&support::toolchain().await, &output, &CancellationToken::new())
		.await
		.unwrap();
	assert!(info.chapters.is_empty(), "{:?}", info.chapters);
}

fn trimmed(job: Job, start_s: f64, end_s: f64) -> Job {
	with_options(
		job,
		JobOptions {
			trim: Some(Trim {
				start_s,
				end_s: Some(end_s),
			}),
			..JobOptions::default()
		},
	)
}

fn subtitle_times(path: &Path) -> Vec<f64> {
	let out = std::process::Command::new("ffprobe")
		.args([
			"-v",
			"error",
			"-select_streams",
			"s",
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

fn subtitle_spans(path: &Path) -> Vec<(f64, f64)> {
	let out = std::process::Command::new("ffprobe")
		.args([
			"-v",
			"error",
			"-select_streams",
			"s",
			"-show_entries",
			"packet=pts_time,duration_time",
			"-of",
			"csv=p=0",
		])
		.arg(path)
		.output()
		.unwrap();
	String::from_utf8_lossy(&out.stdout)
		.lines()
		.filter_map(|line| {
			let (start, length) = line.trim().trim_end_matches(',').split_once(',')?;
			let start: f64 = start.parse().ok()?;
			Some((start, start + length.parse::<f64>().ok()?))
		})
		.collect()
}

fn chapter_spans(info: &cryarchiver_engine::probe::MediaInfo) -> Vec<(String, f64, f64)> {
	info.chapters
		.iter()
		.map(|chapter| {
			let time = |key: &str| chapter[key].as_str().unwrap().parse::<f64>().unwrap();
			(
				chapter["tags"]["title"].as_str().unwrap_or_default().to_owned(),
				time("start_time"),
				time("end_time"),
			)
		})
		.collect()
}

fn near(actual: (f64, f64), expected: (f64, f64)) -> bool {
	(actual.0 - expected.0).abs() < 0.02 && (actual.1 - expected.1).abs() < 0.02
}

#[tokio::test]
async fn cues_and_chapters_across_the_cuts_are_clipped_to_the_window() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::clip_with_extras(dir.path());

	let outcome = run(&trimmed(fast(&clip), 5.5, 8.5), dir.path()).await;

	let JobOutcome::Encoded { output, .. } = outcome else {
		panic!("{outcome:?}")
	};
	let info = probe(&support::toolchain().await, &output, &CancellationToken::new())
		.await
		.unwrap();
	let cues = subtitle_spans(&output);
	assert_eq!(cues.len(), 2, "{cues:?}");
	assert!(near(cues[0], (0.0, 0.5)), "{cues:?}");
	assert!(near(cues[1], (2.5, 3.0)), "{cues:?}");
	let chapters = chapter_spans(&info);
	assert_eq!(chapters.len(), 2, "{chapters:?}");
	assert!(
		chapters[0].0 == "B" && near((chapters[0].1, chapters[0].2), (0.0, 1.5)),
		"{chapters:?}"
	);
	assert!(
		chapters[1].0 == "C" && near((chapters[1].1, chapters[1].2), (1.5, 3.0)),
		"{chapters:?}"
	);
	assert!((info.format.duration.unwrap() - 3.0).abs() < 0.05, "{:?}", info.format);
}

#[tokio::test]
async fn a_cue_on_screen_at_a_late_cut_is_still_found() {
	let dir = tempfile::tempdir().unwrap();
	let cues = dir.path().join("late.srt");
	fs::write(
		&cues,
		"1\n00:00:05,000 --> 00:00:06,000\nearly\n\n2\n00:00:36,000 --> 00:00:39,500\nacross the start\n\n\
		 3\n00:00:40,000 --> 00:00:44,000\nacross the end\n",
	)
	.unwrap();
	let clip = support::black_clip_of(dir.path(), "long.mkv", 45, &cues, "srt");

	let outcome = run(&trimmed(fast(&clip), 38.0, 41.0), dir.path()).await;

	let JobOutcome::Encoded { output, .. } = outcome else {
		panic!("{outcome:?}")
	};
	let cues = subtitle_spans(&output);
	assert_eq!(cues.len(), 2, "{cues:?}");
	assert!(near(cues[0], (0.0, 1.5)), "{cues:?}");
	assert!(near(cues[1], (2.0, 3.0)), "{cues:?}");
}

#[tokio::test]
async fn cut_ass_subtitles_keep_their_styles() {
	let dir = tempfile::tempdir().unwrap();
	let ass = dir.path().join("signs.ass");
	fs::write(
		&ass,
		"[Script Info]\nScriptType: v4.00+\nPlayResX: 320\nPlayResY: 240\n\n[V4+ Styles]\n\
		 Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, \
		 Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, \
		 MarginR, MarginV, Encoding\n\
		 Style: Sign,Arial,24,&H0000FFFF,&H000000FF,&H00000000,&H00000000,1,0,0,0,100,100,0,0,1,2,0,8,10,10,10,1\n\n\
		 [Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
		 Dialogue: 0,0:00:01.00,0:00:03.00,Sign,,0,0,0,,{\\b1}across the start\n\
		 Dialogue: 0,0:00:03.50,0:00:04.00,Sign,,0,0,0,,inside\n",
	)
	.unwrap();
	let clip = dir.path().join("signs.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=320x240:r=25:d=5",
		"-i",
		ass.to_str().unwrap(),
		"-map",
		"0",
		"-map",
		"1",
		"-c:v",
		"mpeg4",
		"-c:s",
		"copy",
		clip.to_str().unwrap(),
	]);

	let outcome = run(&trimmed(fast(&clip), 2.0, 4.5), dir.path()).await;

	let JobOutcome::Encoded { output, .. } = outcome else {
		panic!("{outcome:?}")
	};
	let script = std::process::Command::new("ffmpeg")
		.args(["-v", "error", "-i"])
		.arg(&output)
		.args(["-map", "0:s", "-c", "copy", "-f", "ass", "-"])
		.output()
		.unwrap();
	let script = String::from_utf8_lossy(&script.stdout);
	assert!(script.contains("Style: Sign,Arial,24,"), "{script}");
	assert!(
		script.contains("Dialogue: 0,0:00:00.00,0:00:01.00,Sign,,0,0,0,,{\\b1}across the start"),
		"{script}"
	);
	assert!(script.contains("0:00:01.50,0:00:02.00,Sign"), "{script}");
}

#[tokio::test]
async fn every_text_track_is_cut_onto_its_own_stream() {
	let dir = tempfile::tempdir().unwrap();
	let srt = dir.path().join("first.srt");
	fs::write(&srt, "1\n00:00:01,000 --> 00:00:03,000\nfrom the srt\n").unwrap();
	let ass = dir.path().join("second.ass");
	fs::write(
		&ass,
		"[Script Info]\nScriptType: v4.00+\n\n[V4+ Styles]\n\
		 Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, \
		 Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, \
		 MarginR, MarginV, Encoding\n\
		 Style: Default,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,1,0,2,10,10,10,1\n\n\
		 [Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
		 Dialogue: 0,0:00:01.50,0:00:03.50,Default,,0,0,0,,from the ass\n",
	)
	.unwrap();
	let clip = dir.path().join("two-tracks.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=320x240:r=25:d=5",
		"-i",
		srt.to_str().unwrap(),
		"-i",
		ass.to_str().unwrap(),
		"-map",
		"0",
		"-map",
		"1",
		"-map",
		"2",
		"-c:v",
		"mpeg4",
		"-c:s",
		"copy",
		clip.to_str().unwrap(),
	]);

	let outcome = run(&trimmed(fast(&clip), 2.0, 4.0), dir.path()).await;

	let JobOutcome::Encoded { output, .. } = outcome else {
		panic!("{outcome:?}")
	};
	let track = |spec: &str| {
		let out = std::process::Command::new("ffmpeg")
			.args(["-v", "error", "-i"])
			.arg(&output)
			.args(["-map", spec, "-c:s", "srt", "-f", "srt", "-"])
			.output()
			.unwrap();
		String::from_utf8_lossy(&out.stdout).into_owned()
	};
	assert_eq!(track("0:s:0"), "1\n00:00:00,000 --> 00:00:01,000\nfrom the srt\n\n");
	let second = track("0:s:1");
	assert!(
		second.starts_with("1\n00:00:00,000 --> 00:00:01,500\n") && second.contains("from the ass"),
		"{second}"
	);
}

#[tokio::test]
async fn cut_cues_become_mov_text_in_mp4() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::clip_with_extras(dir.path());
	let options = JobOptions {
		trim: Some(Trim {
			start_s: 5.5,
			end_s: Some(8.5),
		}),
		format: Some(OutputFormat::Mp4),
		..JobOptions::default()
	};

	let outcome = run(&with_options(fast(&clip), options), dir.path()).await;

	let JobOutcome::Encoded { output, .. } = outcome else {
		panic!("{outcome:?}")
	};
	let info = probe(&support::toolchain().await, &output, &CancellationToken::new())
		.await
		.unwrap();
	assert!(
		info.streams.iter().any(|stream| stream.codec_name == "mov_text"),
		"{:?}",
		info.streams
	);
	let cues = subtitle_spans(&output);
	assert!(cues.first().is_some_and(|cue| near(*cue, (0.0, 0.5))), "{cues:?}");
}

#[tokio::test]
async fn trimmed_audio_starts_at_the_cut() {
	let dir = tempfile::tempdir().unwrap();
	let late_tone = dir.path().join("late-tone.flac");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"aevalsrc='if(lt(t,3),0,0.5*sin(2*PI*440*t))':s=48000:d=6",
		"-c:a",
		"flac",
		late_tone.to_str().unwrap(),
	]);

	let from_the_tone = run(&trimmed(job(&late_tone), 3.0, 5.0), dir.path()).await;

	let JobOutcome::Encoded { output, .. } = from_the_tone else {
		panic!("{from_the_tone:?}")
	};
	let first_quarter = std::process::Command::new("ffmpeg")
		.args(["-hide_banner", "-t", "0.25", "-i"])
		.arg(&output)
		.args(["-af", "volumedetect", "-f", "null", "-"])
		.output()
		.unwrap();
	let log = String::from_utf8_lossy(&first_quarter.stderr);
	let peak: f64 = log
		.split("max_volume:")
		.nth(1)
		.and_then(|rest| rest.split_whitespace().next()?.parse().ok())
		.unwrap_or_else(|| panic!("{log}"));
	assert!(
		peak > -20.0,
		"the output starts silent, so it didn't start at --ss: {peak} dB"
	);
}

#[tokio::test]
async fn a_trimmed_output_is_compared_with_the_same_part_of_the_source() {
	let dir = tempfile::tempdir().unwrap();
	let thin = support::tone(
		dir.path(),
		"thin.opus",
		"stereo",
		2,
		20,
		&["-c:a", "libopus", "-b:a", "32k"],
	);

	let outcome = run(&trimmed(job(&thin), 2.0, 5.0), dir.path()).await;

	let JobOutcome::Encoded { warnings, .. } = &outcome else {
		panic!("{outcome:?}")
	};
	assert!(
		warnings
			.iter()
			.any(|warning| warning.starts_with("the output is not smaller than the same part of the source (")),
		"{warnings:?}"
	);
}

#[tokio::test]
#[allow(clippy::cast_precision_loss)]
async fn a_trimmed_outcome_reports_sizes_against_the_same_part_of_the_source() {
	let dir = tempfile::tempdir().unwrap();
	let flac = support::tone(dir.path(), "long.flac", "stereo", 2, 20, &["-c:a", "flac"]);
	let clip = support::clip_with_extras(dir.path());

	for (source, end_s, duration_s) in [(&flac, 5.0, 20.0), (&clip, 6.0, 10.0)] {
		let outcome = run(&trimmed(fast(source), 2.0, end_s), dir.path()).await;

		let JobOutcome::Encoded { src_bytes, .. } = outcome else {
			panic!("{outcome:?}")
		};
		let expected = fs::metadata(source).unwrap().len() as f64 * (end_s - 2.0) / duration_s;
		assert!(
			(src_bytes as f64 - expected).abs() < expected * 0.02,
			"{}: {src_bytes} bytes, expected about {expected}",
			source.display()
		);
	}
}

#[tokio::test]
async fn trimmed_video_keeps_subtitles_and_chapters_in_the_window() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::clip_with_extras(dir.path());

	let outcome = run(&trimmed(fast(&clip), 4.0, 8.0), dir.path()).await;

	let JobOutcome::Encoded { output, .. } = outcome else {
		panic!("{outcome:?}")
	};
	let info = probe(&support::toolchain().await, &output, &CancellationToken::new())
		.await
		.unwrap();
	assert!((info.format.duration.unwrap() - 4.0).abs() < 0.25, "{:?}", info.format);
	let titles: Vec<&str> = info
		.chapters
		.iter()
		.filter_map(|chapter| chapter["tags"]["title"].as_str())
		.collect();
	assert_eq!(titles, ["B", "C"]);
	let cues = subtitle_times(&output);
	assert!((cues[0] - 1.0).abs() < 0.05, "{cues:?}");
	assert!(cues.iter().all(|cue| *cue < 4.05), "{cues:?}");
}

#[tokio::test]
async fn a_trimmed_transparent_video_checks_its_alpha_against_the_window() {
	let dir = tempfile::tempdir().unwrap();
	let alpha = dir.path().join("moving-alpha.mov");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=320x240:r=25:d=4,format=rgba,geq=r='r(X,Y)':g='g(X,Y)':b='b(X,Y)':a='if(lt(X,mod(N*8,320)),255,0)'",
		"-c:v",
		"qtrle",
		alpha.to_str().unwrap(),
	]);

	let outcome = run(&trimmed(fast(&alpha), 2.0, 3.0), dir.path()).await;

	assert!(matches!(outcome, JobOutcome::Encoded { .. }), "{outcome:?}");
}

#[tokio::test]
async fn a_trimmed_video_is_searched_and_checked_on_its_window_and_learns_nothing() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::mpeg4(dir.path(), "clip.mp4", 6);
	let memory = Arc::new(SharedMemory::default());
	let engine = Engine::new(support::toolchain().await).with_quality_memory(memory.clone());

	let (outcome, events) = run_recording(&engine, &trimmed(job(&clip), 2.0, 5.0), dir.path()).await;

	let JobOutcome::Encoded { warnings, .. } = &outcome else {
		panic!("{outcome:?}")
	};
	assert!(warnings.is_empty(), "{warnings:?}");
	assert_eq!(stages(&events, Stage::Sample), 1, "{events:?}");
	assert_eq!(stages(&events, Stage::Verify), 2, "{events:?}");
	assert!(memory.entries().is_empty(), "{:?}", memory.entries());
}

#[tokio::test]
async fn trimmed_audio_is_cut_to_the_window() {
	let dir = tempfile::tempdir().unwrap();
	let flac = support::tone(dir.path(), "a.flac", "stereo", 2, 10, &["-c:a", "flac"]);

	let outcome = run(&trimmed(job(&flac), 2.0, 5.0), dir.path()).await;

	let JobOutcome::Encoded { output, .. } = outcome else {
		panic!("{outcome:?}")
	};
	let info = probe(&support::toolchain().await, &output, &CancellationToken::new())
		.await
		.unwrap();
	assert!((info.format.duration.unwrap() - 3.0).abs() < 0.1, "{:?}", info.format);
}

#[tokio::test]
async fn trimmed_audio_clips_its_chapters_to_the_window() {
	let dir = tempfile::tempdir().unwrap();
	let chapters = dir.path().join("chapters.txt");
	fs::write(
		&chapters,
		";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=2000\ntitle=A\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=2000\nEND=6000\ntitle=B\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=6000\nEND=10000\ntitle=C\n",
	)
	.unwrap();
	let album = dir.path().join("album.mka");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"sine=duration=10",
		"-i",
		chapters.to_str().unwrap(),
		"-map",
		"0",
		"-map_chapters",
		"1",
		"-c:a",
		"flac",
		album.to_str().unwrap(),
	]);
	let options = JobOptions {
		trim: Some(Trim {
			start_s: 1.0,
			end_s: Some(4.0),
		}),
		format: Some(OutputFormat::Mka),
		..JobOptions::default()
	};

	let outcome = run(&with_options(job(&album), options), dir.path()).await;

	let JobOutcome::Encoded { output, .. } = outcome else {
		panic!("{outcome:?}")
	};
	let info = probe(&support::toolchain().await, &output, &CancellationToken::new())
		.await
		.unwrap();
	let chapters: Vec<(&str, f64, f64)> = info
		.chapters
		.iter()
		.map(|chapter| {
			let time = |key: &str| chapter[key].as_str().unwrap().parse::<f64>().unwrap();
			(
				chapter["tags"]["title"].as_str().unwrap(),
				time("start_time"),
				time("end_time"),
			)
		})
		.collect();
	assert_eq!(chapters.len(), 2, "{chapters:?}");
	assert_eq!((chapters[0].0, chapters[1].0), ("A", "B"), "{chapters:?}");
	assert!(
		chapters[0].1.abs() < 0.01 && (chapters[0].2 - 1.0).abs() < 0.01,
		"{chapters:?}"
	);
	assert!(
		(chapters[1].1 - 1.0).abs() < 0.01 && (chapters[1].2 - 3.0).abs() < 0.01,
		"{chapters:?}"
	);
}

#[tokio::test]
async fn a_trim_shorter_than_a_frame_fails_with_the_frame_length() {
	let dir = tempfile::tempdir().unwrap();
	let slow = support::video(
		dir.path(),
		"slideshow.mkv",
		3,
		&["-r", "5", "-c:v", "mpeg4", "-q:v", "2"],
	);

	let outcome = run(&trimmed(fast(&slow), 1.05, 1.2), dir.path()).await;

	assert!(
		matches!(&outcome, JobOutcome::Failed(err) if err.to_string().contains("it must last at least 0.2 s")),
		"{outcome:?}"
	);
}

#[tokio::test]
async fn trims_outside_the_file_and_trimmed_images_fail() {
	let dir = tempfile::tempdir().unwrap();
	let flac = support::tone(dir.path(), "a.flac", "stereo", 2, 3, &["-c:a", "flac"]);
	let photo = support::photo_png(dir.path(), "photo.png", 64, 48);

	let late = run(&trimmed(job(&flac), 20.0, 25.0), dir.path()).await;
	let backwards = run(&trimmed(job(&flac), 2.0, 1.0), dir.path()).await;
	let instant = run(&trimmed(job(&flac), 1.0, 1.05), dir.path()).await;
	let image = run(&trimmed(job(&photo), 1.0, 2.0), dir.path()).await;

	assert!(
		matches!(&late, JobOutcome::Failed(err) if err.to_string().contains("past the end")),
		"{late:?}"
	);
	assert!(
		matches!(&backwards, JobOutcome::Failed(err) if err.to_string().contains("must be later than --ss")),
		"{backwards:?}"
	);
	assert!(
		matches!(&instant, JobOutcome::Failed(err) if err.to_string().contains("at least 0.1 s")),
		"{instant:?}"
	);
	assert!(
		matches!(&image, JobOutcome::Failed(err) if err.to_string().contains("not images")),
		"{image:?}"
	);
}

fn burning(job: Job, pick: SubtitlePick) -> Job {
	with_options(
		job,
		JobOptions {
			burn_subs: Some(BurnSubs { pick, size_percent: 5 }),
			..JobOptions::default()
		},
	)
}

fn with_burn(job: Job) -> Job {
	let options = JobOptions {
		burn_subs: Some(BurnSubs {
			pick: SubtitlePick::Default,
			size_percent: 5,
		}),
		..job.options.clone()
	};
	with_options(job, options)
}

async fn subtitle_count(output: &Path) -> usize {
	probe(&support::toolchain().await, output, &CancellationToken::new())
		.await
		.unwrap()
		.streams
		.iter()
		.filter(|stream| stream.codec_type == "subtitle")
		.count()
}

#[tokio::test]
async fn burned_text_subtitles_leave_no_subtitle_track() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::clip_with_extras(dir.path());

	let outcome = run(&burning(fast(&clip), SubtitlePick::Default), dir.path()).await;

	let JobOutcome::Encoded { output, .. } = outcome else {
		panic!("{outcome:?}")
	};
	assert_eq!(subtitle_count(&output).await, 0);
	assert!(!dir.path().join(CLI_WORK_DIR).exists());
}

/// H.264 with B-frames, as Blu-ray video is, or MPEG-4 with B-frames when libx264 is missing.
async fn b_frame_codec() -> &'static [&'static str] {
	if support::toolchain().await.encoders.contains("libx264") {
		&["-c:v", "libx264", "-bf", "3", "-g", "12", "-pix_fmt", "yuv420p"]
	} else {
		&["-c:v", "mpeg4", "-bf", "2", "-g", "12"]
	}
}

async fn b_frame_clip(dir: &Path, subtitles: &Path, marked_frame: Option<u32>) -> std::path::PathBuf {
	support::subtitled_clip(
		dir,
		"b-frames.mkv",
		&support::Picture {
			source: "color=c=black",
			seconds: 10,
			codec: b_frame_codec().await,
			second_angle: false,
			marked_frame,
		},
		subtitles,
		"copy",
	)
}

/// A B-frame clip whose frame 125 (5.00 s into the video) is marked and a PGS box shown from that frame for 1 s.
async fn marked_box_clip(dir: &Path) -> std::path::PathBuf {
	let pgs = support::pgs_box(dir, 5_000, 6_000);
	b_frame_clip(dir, &pgs, Some(125)).await
}

/// Runs the untrimmed burn of `source` and one burn per trim in `trims`, and checks that the box is drawn on exactly
/// the 25 frames that start at the marked one, wherever the cut and the source's timestamps put them.
async fn drawn_from_the_marker(source: &Path, out_dir: &Path, trims: &[(f64, f64)]) {
	let cuts = trims
		.iter()
		.map(|&(start_s, end_s)| with_burn(trimmed(fast(source), start_s, end_s)));
	for job in std::iter::once(burning(fast(source), SubtitlePick::Default)).chain(cuts) {
		let outcome = run(&job, out_dir).await;

		let JobOutcome::Encoded { output, .. } = outcome else {
			panic!("{outcome:?}")
		};
		let (marked, _) = support::lit_span_in(&output, 0, support::MARKER).unwrap();
		let drawn = support::lit_span_in(&output, 0, support::BELOW_MARKER).unwrap();
		assert!(
			near(drawn, (marked, marked + 0.96)),
			"{}: box {drawn:?}, marked frame {marked}",
			output.display()
		);
		fs::remove_file(&output).unwrap();
	}
}

#[tokio::test]
async fn bitmap_cues_in_an_m2ts_whose_audio_starts_first_are_drawn_on_their_frames() {
	let dir = tempfile::tempdir().unwrap();
	let mkv = marked_box_clip(dir.path()).await;
	let m2ts = dir.path().join("blu-ray.m2ts");
	support::ffmpeg(&[
		"-i",
		mkv.to_str().unwrap(),
		"-itsoffset",
		"0.3",
		"-i",
		mkv.to_str().unwrap(),
		"-map",
		"1:v",
		"-map",
		"0:a",
		"-map",
		"1:s",
		"-c",
		"copy",
		"-mpegts_m2ts_mode",
		"1",
		m2ts.to_str().unwrap(),
	]);

	drawn_from_the_marker(&m2ts, dir.path(), &[(4.0, 8.0), (0.1, 8.0), (0.2, 7.0)]).await;
}

#[tokio::test]
async fn bitmap_cues_on_a_video_time_base_of_one_tick_per_frame_are_drawn_on_their_frames() {
	let dir = tempfile::tempdir().unwrap();
	let mkv = marked_box_clip(dir.path()).await;
	let dvd = support::converted(
		dir.path(),
		"dvd.mkv",
		&["-fix_sub_duration"],
		&mkv,
		&["-map", "0", "-c:v", "copy", "-c:a", "copy", "-c:s", "dvdsub"],
	);
	let ticks = support::converted(
		dir.path(),
		"ticks.mp4",
		&[],
		&dvd,
		&["-map", "0", "-c", "copy", "-video_track_timescale", "25"],
	);

	drawn_from_the_marker(&ticks, dir.path(), &[(4.0, 8.0)]).await;
}

/// A clip of B-frame video whose frame 125 (5.00 s into the video) is marked, with a click in the audio at the same
/// moment. The video starts `video_delay_s` and the audio `audio_delay_s` into the file. A `.m2ts` name gets the
/// Blu-ray layout.
async fn marked_click_clip(dir: &Path, name: &str, video_delay_s: f64, audio_delay_s: f64) -> std::path::PathBuf {
	let path = dir.join(name);
	let video = "color=c=black:s=320x240:r=25:d=10,drawbox=x=0:y=0:w=40:h=40:c=white:t=fill:enable='eq(n\\,125)'";
	let click_s = 5.0 + video_delay_s - audio_delay_s;
	let audio = format!(
		"aevalsrc='if(between(t\\,{click_s}\\,{})\\,0.9\\,0)':s=48000:d=10",
		click_s + 0.005
	);
	let (video_delay, audio_delay) = (video_delay_s.to_string(), audio_delay_s.to_string());

	let mut args = vec![
		"-itsoffset",
		video_delay.as_str(),
		"-f",
		"lavfi",
		"-i",
		video,
		"-itsoffset",
		audio_delay.as_str(),
		"-f",
		"lavfi",
		"-i",
		audio.as_str(),
		"-map",
		"0",
		"-map",
		"1",
	];
	args.extend(b_frame_codec().await);
	args.extend(["-c:a", "aac"]);
	if path.extension().is_some_and(|extension| extension == "m2ts") {
		args.extend(["-mpegts_m2ts_mode", "1"]);
	}
	args.push(path.to_str().unwrap());
	support::ffmpeg(&args);
	path
}

/// The first start of each stream kind in `path`.
async fn start_of(path: &Path, kind: &str) -> f64 {
	let info = probe(&support::toolchain().await, path, &CancellationToken::new())
		.await
		.unwrap();
	info.streams
		.iter()
		.filter(|stream| stream.codec_type == kind)
		.find_map(|stream| stream.start_time)
		.unwrap()
}

/// Runs `source` without a burn, whole or cut to `trim`, and checks it against the file's own clock, which is what a
/// player shows: the marked frame lands where that clock has it, less the cut's start, and the click stays as far
/// from it as in the source. Uncut, the audio also starts as far from the video as in the source; cut, the video
/// starts within a frame of where that clock puts the source's.
async fn on_the_file_clock(source: &Path, out_dir: &Path, trim: Option<(f64, f64)>) {
	let file_start = probe(&support::toolchain().await, source, &CancellationToken::new())
		.await
		.unwrap()
		.format
		.start_time
		.unwrap_or(0.0);
	let (marker, click) = support::marker_and_click(source);
	let start_s = trim.map_or(0.0, |(start_s, _)| start_s);
	let job = match trim {
		Some((start_s, end_s)) => trimmed(fast(source), start_s, end_s),
		None => with_options(
			fast(source),
			JobOptions {
				strip: true,
				..JobOptions::default()
			},
		),
	};

	let outcome = run(&job, out_dir).await;

	let JobOutcome::Encoded { output, .. } = outcome else {
		panic!("{outcome:?}")
	};
	let (cut_marker, cut_click) = support::marker_and_click(&output);
	let name = source.file_name().unwrap().display();
	let expected = marker - file_start - start_s;
	assert!(
		(cut_marker - expected).abs() < 0.021,
		"{name} {trim:?}: marked frame at {cut_marker}, {expected:.3} on the file's clock"
	);
	assert!(
		((cut_click - cut_marker) - (click - marker)).abs() < 0.03,
		"{name} {trim:?}: click {:.3} s after the marked frame, {:.3} s in the source",
		cut_click - cut_marker,
		click - marker
	);
	if trim.is_none() {
		let source_lead = start_of(source, "video").await - start_of(source, "audio").await;
		let output_lead = start_of(&output, "video").await - start_of(&output, "audio").await;
		assert!(
			(output_lead - source_lead).abs() < 0.021,
			"{name}: the video starts {output_lead:.3} s after the audio, {source_lead:.3} s in the source"
		);
	} else {
		let wanted = (start_of(source, "video").await - file_start - start_s).max(0.0);
		let video_start = start_of(&output, "video").await;
		assert!(
			(-0.021..0.041).contains(&(video_start - wanted)),
			"{name} {trim:?}: the video starts at {video_start}, {wanted:.3} on the file's clock"
		);
	}
	fs::remove_file(&output).unwrap();
}

#[tokio::test]
async fn an_m2ts_keeps_every_stream_on_the_file_clock_whole_and_cut() {
	let dir = tempfile::tempdir().unwrap();
	let audio_first = marked_click_clip(dir.path(), "audio-first.m2ts", 0.3, 0.0).await;
	let video_first = marked_click_clip(dir.path(), "video-first.m2ts", 0.0, 0.3).await;

	for (source, trim) in [
		(&audio_first, None),
		(&audio_first, Some((4.0, 8.0))),
		(&audio_first, Some((0.1, 8.0))),
		(&video_first, None),
		(&video_first, Some((0.1, 8.0))),
	] {
		on_the_file_clock(source, dir.path(), trim).await;
	}
}

#[tokio::test]
async fn an_mkv_whose_video_starts_after_its_audio_keeps_its_offsets() {
	let dir = tempfile::tempdir().unwrap();
	let late_video = marked_click_clip(dir.path(), "late-video.mkv", 0.3, 0.0).await;

	for trim in [None, Some((4.0, 8.0))] {
		on_the_file_clock(&late_video, dir.path(), trim).await;
	}
}

/// A broadcast-style recording: a `.ts` whose video starts 0.3 s after its audio, cut on a packet boundary a fifth of
/// the way in, so that it starts mid-GOP with packets that don't decode.
async fn broadcast_cut(dir: &Path) -> std::path::PathBuf {
	let full = dir.join("recording.ts");
	let mut args = vec![
		"-itsoffset",
		"0.3",
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=320x240:r=25:d=8",
		"-f",
		"lavfi",
		"-i",
		"sine=duration=8.3",
		"-map",
		"0",
		"-map",
		"1",
	];
	args.extend(b_frame_codec().await);
	args.extend(["-c:a", "aac", full.to_str().unwrap()]);
	support::ffmpeg(&args);

	let bytes = fs::read(&full).unwrap();
	let cut = dir.join("cut.ts");
	fs::write(&cut, &bytes[bytes.len() / 5 / 188 * 188..]).unwrap();
	cut
}

/// An open-GOP H.264 clip copied out from its second GOP, so that it starts with B-frames that don't decode.
fn open_gop_cut(dir: &Path) -> std::path::PathBuf {
	let full = dir.join("open-gop.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=320x240:r=25:d=8",
		"-f",
		"lavfi",
		"-i",
		"sine=duration=8",
		"-c:v",
		"libx264",
		"-bf",
		"3",
		"-g",
		"12",
		"-x264-params",
		"open-gop=1",
		"-pix_fmt",
		"yuv420p",
		"-c:a",
		"aac",
		full.to_str().unwrap(),
	]);
	support::converted(
		dir,
		"open-gop-cut.mp4",
		&["-ss", "1.1"],
		&full,
		&["-map", "0", "-c", "copy"],
	)
}

#[tokio::test]
async fn sources_whose_first_packets_do_not_decode_encode_whole() {
	let dir = tempfile::tempdir().unwrap();
	if !support::toolchain().await.encoders.contains("libx264") {
		return;
	}
	let broadcast = broadcast_cut(dir.path()).await;
	assert!(start_of(&broadcast, "audio").await + 0.2 < start_of(&broadcast, "video").await);
	let open_gop = open_gop_cut(dir.path());

	for (source, format) in [
		(&broadcast, OutputFormat::Mkv),
		(&broadcast, OutputFormat::Mp4),
		(&open_gop, OutputFormat::Mkv),
		(&open_gop, OutputFormat::Mp4),
	] {
		let job = with_options(
			fast(source),
			JobOptions {
				format: Some(format),
				..JobOptions::default()
			},
		);

		let outcome = run(&job, dir.path()).await;

		let JobOutcome::Encoded { output, .. } = outcome else {
			panic!("{} to {format:?}: {outcome:?}", source.display())
		};
		fs::remove_file(&output).unwrap();
	}
}

#[tokio::test]
async fn a_ts_trim_decodes_through_the_cut_and_its_samples_and_checks_stay_aligned() {
	let dir = tempfile::tempdir().unwrap();
	let m2ts = dir.path().join("moving.m2ts");
	let mut args = vec![
		"-itsoffset",
		"0.3",
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=320x240:r=25:d=10",
		"-f",
		"lavfi",
		"-i",
		"sine=duration=10.3",
		"-map",
		"0",
		"-map",
		"1",
	];
	args.extend(b_frame_codec().await);
	args.extend(["-c:a", "aac", "-mpegts_m2ts_mode", "1", m2ts.to_str().unwrap()]);
	support::ffmpeg(&args);
	let engine = Engine::new(support::toolchain().await);

	let (outcome, events) = run_recording(&engine, &trimmed(job(&m2ts), 4.0, 8.0), dir.path()).await;

	let JobOutcome::Encoded { output, warnings, .. } = &outcome else {
		panic!("{outcome:?}")
	};
	assert!(
		!warnings.iter().any(|warning| warning.contains("quality target")),
		"{warnings:?}"
	);
	assert_eq!(stages(&events, Stage::Sample), 1, "{events:?}");
	assert_eq!(stages(&events, Stage::Verify), 2, "{events:?}");
	let video_start = start_of(output, "video").await;
	assert!(video_start <= 0.041, "the video starts {video_start} s after the cut");
}

const ORANGE: [u8; 3] = [152, 50, 188];
const GREY: [u8; 3] = [196, 128, 128];
/// Holds orange (ff8c00) and grey (d2d2d2), which ffmpeg's default DVD palette doesn't.
const DVD_PALETTE: &str =
	"000000,ff8c00,d2d2d2,808080,0000ff,00ff00,ff0000,ffff00,ff00ff,00ffff,808000,8080ff,800080,80ff80,008080,ffffff";

#[tokio::test]
async fn dvd_subtitles_are_drawn_in_their_own_palette_and_place_from_mkv_and_mp4() {
	let dir = tempfile::tempdir().unwrap();
	let bands = support::pgs_bands(dir.path(), 5_000, 6_000, &[ORANGE, GREY]);
	let clip = support::black_clip(dir.path(), "bands.mkv", &bands, "copy", false);
	let mkv = support::converted(
		dir.path(),
		"vobsub.mkv",
		&["-fix_sub_duration"],
		&clip,
		&[
			"-map",
			"0",
			"-c:v",
			"copy",
			"-c:a",
			"copy",
			"-c:s",
			"dvdsub",
			"-palette",
			DVD_PALETTE,
			"-s:s",
			"320x240",
		],
	);
	let mp4 = support::converted(dir.path(), "vobsub.mp4", &[], &mkv, &["-map", "0", "-c", "copy"]);

	for source in [&mkv, &mp4] {
		let outcome = run(&burning(fast(source), SubtitlePick::Default), dir.path()).await;

		let JobOutcome::Encoded { output, .. } = outcome else {
			panic!("{outcome:?}")
		};
		let name = source.file_name().unwrap().display();
		for (region, expected) in [(support::BOX_TOP, ORANGE), (support::BOX_BOTTOM, GREY)] {
			let colour = support::colour_in(&output, region);
			assert!(
				colour.is_some_and(|colour| colour
					.iter()
					.zip(expected)
					.all(|(actual, expected)| (actual - f64::from(expected)).abs() < 3.0)),
				"{name}: {region} is {colour:?}, {expected:?} expected"
			);
		}
		assert_eq!(
			support::lit_span_in(&output, 0, "crop=320:178:0:0"),
			None,
			"{name}: drawn above the box"
		);
		fs::remove_file(&output).unwrap();
	}
}

#[tokio::test]
async fn a_trimmed_text_burn_on_a_video_time_base_of_one_tick_per_frame_keeps_every_frame_time() {
	let dir = tempfile::tempdir().unwrap();
	let cues = dir.path().join("late.srt");
	fs::write(&cues, "1\n00:00:05,000 --> 00:00:06,000\nfive\n").unwrap();
	let codec = [b_frame_codec().await, &["-video_track_timescale", "25"]].concat();
	let ticks = support::subtitled_clip(
		dir.path(),
		"ticks.mp4",
		&support::Picture {
			source: "color=c=black",
			seconds: 10,
			codec: &codec,
			second_angle: false,
			marked_frame: Some(125),
		},
		&cues,
		"mov_text",
	);

	let plain = run(&trimmed(fast(&ticks), 4.01, 6.0), dir.path()).await;
	let JobOutcome::Encoded { output: plain, .. } = plain else {
		panic!("{plain:?}")
	};
	let plain_times = support::frame_times(&plain, 0);
	fs::remove_file(&plain).unwrap();
	let burned = run(&with_burn(trimmed(fast(&ticks), 4.01, 6.0)), dir.path()).await;
	let JobOutcome::Encoded { output: burned, .. } = burned else {
		panic!("{burned:?}")
	};

	let burned_times = support::frame_times(&burned, 0);
	assert!(
		burned_times.windows(2).all(|pair| pair[1] > pair[0]),
		"{burned_times:?}"
	);
	assert_eq!(burned_times.len(), plain_times.len());
	assert!(
		burned_times
			.iter()
			.zip(&plain_times)
			.all(|(burned, plain)| (burned - plain).abs() < 0.001),
		"{burned_times:?} vs {plain_times:?}"
	);
	let (marked, _) = support::lit_span_in(&burned, 0, support::MARKER).unwrap();
	let drawn = support::lit_span_in(&burned, 0, support::BELOW_MARKER).unwrap();
	assert!(
		near(drawn, (marked, marked + 0.96)),
		"text {drawn:?}, marked frame {marked}"
	);
}

#[tokio::test]
async fn burned_bitmap_subtitles_are_overlaid_on_exactly_their_frames() {
	let dir = tempfile::tempdir().unwrap();
	let pgs = support::pgs_box(dir.path(), 5_000, 6_000);
	let bitmap = b_frame_clip(dir.path(), &pgs, None).await;

	let whole = burning(fast(&bitmap), SubtitlePick::Track(1));
	let cut = with_burn(trimmed(fast(&bitmap), 4.0, 8.0));

	for (job, expected) in [(whole, (5.0, 5.96)), (cut, (1.0, 1.96))] {
		let outcome = run(&job, dir.path()).await;

		let JobOutcome::Encoded { output, .. } = outcome else {
			panic!("{outcome:?}")
		};
		assert_eq!(subtitle_count(&output).await, 0);
		let lit = support::lit_span(&output, 0).unwrap();
		assert!(near(lit, expected), "{}: {lit:?}", output.display());
		fs::remove_file(&output).unwrap();
	}
}

#[tokio::test]
async fn a_bitmap_cue_on_screen_at_the_trim_start_shows_from_the_first_frame() {
	let dir = tempfile::tempdir().unwrap();
	let pgs = support::pgs_box(dir.path(), 1_000, 7_000);
	let bitmap = support::black_clip(dir.path(), "gop12.mkv", &pgs, "copy", false);

	let outcome = run(&with_burn(trimmed(fast(&bitmap), 4.0, 8.0)), dir.path()).await;

	let JobOutcome::Encoded { output, .. } = outcome else {
		panic!("{outcome:?}")
	};
	let lit = support::lit_span(&output, 0).unwrap();
	assert!(near(lit, (0.0, 2.96)), "{lit:?}");
}

/// A PGS clip whose box is shown for each of `spans` (in ms), and the same clip with the track as DVD subtitles,
/// whose packets last until the next one and carry that stop time in their payload too. The video has one keyframe,
/// so a seek anywhere reads the clip from its start, as a seek into a real file reads back to a subtitle still on
/// screen.
fn bitmap_clips(dir: &Path, spans: &[(u32, u32)]) -> [std::path::PathBuf; 2] {
	let first = spans[0].0;
	let pgs = dir.join(format!("cues-{first}.sup"));
	let cues: Vec<u8> = spans
		.iter()
		.flat_map(|&(from_ms, to_ms)| fs::read(support::pgs_box(dir, from_ms, to_ms)).unwrap())
		.collect();
	fs::write(&pgs, cues).unwrap();
	let pgs_clip = support::subtitled_clip(
		dir,
		&format!("pgs-{first}.mkv"),
		&support::Picture {
			source: "color=c=black",
			seconds: 10,
			codec: &["-c:v", "mpeg4", "-g", "250"],
			second_angle: false,
			marked_frame: None,
		},
		&pgs,
		"copy",
	);
	let dvd_clip = support::converted(
		dir,
		&format!("dvd-{first}.mkv"),
		&["-fix_sub_duration"],
		&pgs_clip,
		&["-map", "0", "-c:v", "copy", "-c:a", "copy", "-c:s", "dvdsub"],
	);
	[pgs_clip, dvd_clip]
}

#[tokio::test]
async fn a_kept_bitmap_cue_on_screen_at_the_trim_start_does_not_move_the_start() {
	let dir = tempfile::tempdir().unwrap();

	for source in bitmap_clips(dir.path(), &[(1_000, 3_500), (4_000, 4_500)]) {
		let outcome = run(&trimmed(fast(&source), 3.0, 5.0), dir.path()).await;

		let JobOutcome::Encoded { output, .. } = outcome else {
			panic!("{outcome:?}")
		};
		let starts = stream_starts(&output).await;
		let started = |codec: &str, start: Option<f64>| {
			let earliest = if codec.contains("sub") { 1.05 } else { 0.05 };
			start.is_some_and(|start| (0.0..earliest).contains(&start))
		};
		assert!(
			starts.iter().all(|(codec, start)| started(codec, *start)),
			"{}: {starts:?}",
			source.display()
		);
		let cues = subtitle_times(&output);
		assert!(
			cues.iter().all(|time| *time >= 0.0) && cues.iter().any(|time| (time - 1.0).abs() < 0.02),
			"{}: {cues:?}",
			source.display()
		);
		fs::remove_file(&output).unwrap();
	}
}

#[tokio::test]
async fn copied_audio_in_a_trim_starts_no_earlier_than_the_window() {
	let dir = tempfile::tempdir().unwrap();
	let expr = (0..10).map(|_| "0.3*sin(2*PI*440*t)").collect::<Vec<_>>().join("|");
	let wide = dir.path().join("wide.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=320x240:r=25:d=6",
		"-f",
		"lavfi",
		"-i",
		&format!("aevalsrc={expr}:s=48000:d=6"),
		"-c:v",
		"mpeg4",
		"-c:a",
		"pcm_s16le",
		wide.to_str().unwrap(),
	]);

	let outcome = run(&trimmed(fast(&wide), 2.01, 4.0), dir.path()).await;

	let JobOutcome::Encoded { output, warnings, .. } = outcome else {
		panic!("{outcome:?}")
	};
	assert!(
		warnings.iter().any(|warning| warning.contains("copied as-is")),
		"{warnings:?}"
	);
	let starts = stream_starts(&output).await;
	assert!(
		starts
			.iter()
			.all(|(_, start)| start.is_some_and(|start| (0.0..0.05).contains(&start))),
		"{starts:?}"
	);
}

#[tokio::test]
async fn a_kept_bitmap_cue_starting_exactly_at_the_trim_start_is_kept_at_zero() {
	let dir = tempfile::tempdir().unwrap();

	for source in bitmap_clips(dir.path(), &[(3_000, 4_000)]) {
		let outcome = run(&trimmed(fast(&source), 3.0, 5.0), dir.path()).await;

		let JobOutcome::Encoded { output, .. } = outcome else {
			panic!("{outcome:?}")
		};
		let cues = subtitle_times(&output);
		assert!(
			cues.first().is_some_and(|start| start.abs() < 0.001),
			"{}: {cues:?}",
			source.display()
		);
		fs::remove_file(&output).unwrap();
	}
}

#[tokio::test]
async fn a_kept_bitmap_cue_running_past_the_trim_end_ends_with_the_window() {
	let dir = tempfile::tempdir().unwrap();
	let [pgs, dvd] = bitmap_clips(dir.path(), &[(4_000, 7_000)]);

	for source in [&pgs, &dvd] {
		let outcome = run(&trimmed(fast(source), 3.0, 5.0), dir.path()).await;

		let JobOutcome::Encoded { output, .. } = outcome else {
			panic!("{outcome:?}")
		};
		let info = probe(&support::toolchain().await, &output, &CancellationToken::new())
			.await
			.unwrap();
		let duration = info.format.duration.unwrap();
		assert!(
			(duration - 2.0).abs() < 0.05,
			"{}: {duration} s, cues {:?}",
			source.display(),
			subtitle_spans(&output)
		);
		let shown = decoded_cue_spans(&output);
		assert!(
			shown.iter().all(|(_, end)| *end < 2.02),
			"{}: decoded cues {shown:?}",
			source.display()
		);
		if source == &pgs {
			assert!(
				subtitle_times(&output)
					.first()
					.is_some_and(|start| (start - 1.0).abs() < 0.02),
				"{:?}",
				subtitle_times(&output)
			);
		}
		fs::remove_file(&output).unwrap();
	}
}

async fn stream_starts(path: &Path) -> Vec<(String, Option<f64>)> {
	probe(&support::toolchain().await, path, &CancellationToken::new())
		.await
		.unwrap()
		.streams
		.iter()
		.map(|stream| (stream.codec_name.clone(), stream.start_time))
		.collect()
}

/// When each decoded subtitle is shown and hidden, as a decoder that honours a DVD packet's own stop time sees it.
/// A PGS cue has no stop time of its own, so it ends at its start here.
fn decoded_cue_spans(path: &Path) -> Vec<(f64, f64)> {
	let out = std::process::Command::new("ffprobe")
		.args([
			"-v",
			"error",
			"-select_streams",
			"s",
			"-show_frames",
			"-show_entries",
			"subtitle=pts_time,end_display_time",
			"-of",
			"csv=p=0",
		])
		.arg(path)
		.output()
		.unwrap();
	let spans: Vec<(f64, f64)> = String::from_utf8_lossy(&out.stdout)
		.lines()
		.filter_map(|line| {
			let (start, end_ms) = line.trim().trim_end_matches(',').split_once(',')?;
			let start: f64 = start.parse().ok()?;
			let end_ms: f64 = end_ms.parse().ok()?;
			Some((start, start + if end_ms > 1e6 { 0.0 } else { end_ms / 1000.0 }))
		})
		.collect();
	assert_eq!(
		spans.len(),
		subtitle_times(path).len(),
		"every subtitle packet decodes: {spans:?}"
	);
	spans
}

#[tokio::test]
async fn a_burned_trim_starts_on_the_same_frame_time_as_an_unburned_one() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::clip_with_extras(dir.path());
	let pgs = support::pgs_box(dir.path(), 5_000, 6_000);
	let bitmap = support::black_clip(dir.path(), "bitmap.mkv", &pgs, "copy", false);

	for source in [&clip, &bitmap] {
		let plain = run(&trimmed(fast(source), 4.01, 6.0), dir.path()).await;
		let JobOutcome::Encoded { output: plain, .. } = plain else {
			panic!("{plain:?}")
		};
		let plain_start = support::frame_times(&plain, 0)[0];
		fs::remove_file(&plain).unwrap();
		let burned = run(&with_burn(trimmed(fast(source), 4.01, 6.0)), dir.path()).await;
		let JobOutcome::Encoded { output: burned, .. } = burned else {
			panic!("{burned:?}")
		};

		let burned_start = support::frame_times(&burned, 0)[0];
		assert!(plain_start > 0.02, "{plain_start}");
		assert!(
			(burned_start - plain_start).abs() < 0.001,
			"{burned_start} vs {plain_start}"
		);
		fs::remove_file(&burned).unwrap();
	}
}

#[tokio::test]
async fn subtitles_are_burned_into_the_main_video_only() {
	let dir = tempfile::tempdir().unwrap();
	let cues = dir.path().join("late.srt");
	fs::write(&cues, "1\n00:00:05,000 --> 00:00:06,000\nfive\n").unwrap();
	let text = support::black_clip(dir.path(), "text-angles.mkv", &cues, "srt", true);
	let pgs = support::pgs_box(dir.path(), 5_000, 6_000);
	let bitmap = support::black_clip(dir.path(), "bitmap-angles.mkv", &pgs, "copy", true);

	let from_text = run(&burning(fast(&text), SubtitlePick::Default), dir.path()).await;
	let from_bitmap = run(&burning(fast(&bitmap), SubtitlePick::Default), dir.path()).await;

	for outcome in [from_text, from_bitmap] {
		let JobOutcome::Encoded { output, .. } = outcome else {
			panic!("{outcome:?}")
		};
		let info = probe(&support::toolchain().await, &output, &CancellationToken::new())
			.await
			.unwrap();
		assert_eq!(
			info.streams
				.iter()
				.filter(|stream| stream.codec_type == "video")
				.count(),
			2
		);
		let main = support::lit_span(&output, 0).unwrap();
		assert!(near(main, (5.0, 5.96)), "{}: {main:?}", output.display());
		assert_eq!(support::lit_span(&output, 1), None, "{}", output.display());
	}
}

#[tokio::test]
async fn a_burn_without_its_filter_fails_before_any_encoding() {
	let dir = tempfile::tempdir().unwrap();
	let text = support::clip_with_extras(dir.path());
	let pgs = support::pgs_box(dir.path(), 5_000, 6_000);
	let bitmap = support::black_clip(dir.path(), "bitmap.mkv", &pgs, "copy", false);
	let mut without_libass = support::toolchain().await;
	without_libass.filters.remove("subtitles");
	let mut without_overlay = support::toolchain().await;
	without_overlay.filters.remove("overlay");
	let mut without_subrip = support::toolchain().await;
	without_subrip.decoders.remove("subrip");
	let mut without_pgs = support::toolchain().await;
	without_pgs.decoders.remove("pgssub");

	let cases = [
		(without_libass, &text, "subtitles filter"),
		(without_overlay, &bitmap, "overlay filter"),
		(without_subrip, &text, "subrip decoder"),
		(without_pgs, &bitmap, "pgssub decoder"),
	];
	for (tools, source, missing) in cases {
		let engine = Engine::new(tools);
		let (outcome, events) = run_recording(&engine, &burning(fast(source), SubtitlePick::Default), dir.path()).await;

		let JobOutcome::Failed(err) = &outcome else {
			panic!("{outcome:?}")
		};
		assert!(matches!(err.kind, ErrorKind::Toolchain(_)), "{err:?}");
		assert!(err.to_string().contains(missing), "{err}");
		assert_eq!(stages(&events, Stage::Analyze), 0, "{events:?}");
	}
}

#[tokio::test]
async fn a_missing_subtitle_language_fails_before_encoding() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::clip_with_extras(dir.path());

	let outcome = run(&burning(fast(&clip), SubtitlePick::Language("jpn".into())), dir.path()).await;

	assert!(
		matches!(&outcome, JobOutcome::Failed(err) if err.to_string().contains("jpn")),
		"{outcome:?}"
	);
}

#[tokio::test]
async fn a_trimmed_and_stripped_burn_draws_each_cue_at_its_time_in_the_window() {
	let dir = tempfile::tempdir().unwrap();
	let cues = dir.path().join("late.srt");
	fs::write(&cues, "1\n00:00:05,000 --> 00:00:06,000\nfive\n").unwrap();
	let text = support::black_clip(dir.path(), "text.mkv", &cues, "srt", false);
	let pgs = support::pgs_box(dir.path(), 5_000, 6_000);
	let bitmap = support::black_clip(dir.path(), "bitmap.mkv", &pgs, "copy", false);
	let options = JobOptions {
		trim: Some(Trim {
			start_s: 4.0,
			end_s: Some(8.0),
		}),
		strip: true,
		burn_subs: Some(BurnSubs {
			pick: SubtitlePick::Default,
			size_percent: 5,
		}),
		..JobOptions::default()
	};

	let from_text = run(&with_options(fast(&text), options.clone()), dir.path()).await;
	let from_bitmap = run(&with_options(fast(&bitmap), options), dir.path()).await;

	for outcome in [from_text, from_bitmap] {
		let JobOutcome::Encoded { output, .. } = outcome else {
			panic!("{outcome:?}")
		};
		assert_eq!(subtitle_count(&output).await, 0);
		let lit = support::lit_span(&output, 0).unwrap();
		assert!(near(lit, (1.0, 1.96)), "{}: {lit:?}", output.display());
	}
}

#[tokio::test]
async fn burned_subtitles_are_searched_and_checked_against_the_same_burn() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::clip_with_extras(dir.path());
	let engine = Engine::new(support::toolchain().await);
	let options = JobOptions {
		trim: Some(Trim {
			start_s: 2.0,
			end_s: Some(6.0),
		}),
		burn_subs: Some(BurnSubs {
			pick: SubtitlePick::Track(1),
			size_percent: 20,
		}),
		..JobOptions::default()
	};

	let (outcome, events) = run_recording(&engine, &with_options(job(&clip), options), dir.path()).await;

	let JobOutcome::Encoded { output, warnings, .. } = &outcome else {
		panic!("{outcome:?}")
	};
	assert!(
		!warnings.iter().any(|warning| warning.contains("quality target")),
		"{warnings:?}"
	);
	assert_eq!(stages(&events, Stage::Sample), 1, "{events:?}");
	assert_eq!(stages(&events, Stage::Verify), 2, "{events:?}");
	assert_eq!(subtitle_count(output).await, 0);
}

#[tokio::test]
async fn burned_bitmap_subtitles_are_searched_and_checked_against_the_same_burn() {
	let dir = tempfile::tempdir().unwrap();
	let pgs = support::pgs_box(dir.path(), 1_000, 2_000);
	let clip = support::subtitled_clip(
		dir.path(),
		"boxed.mkv",
		&support::Picture {
			source: "testsrc2",
			seconds: 3,
			codec: &["-c:v", "mpeg4", "-q:v", "2"],
			second_angle: false,
			marked_frame: None,
		},
		&pgs,
		"copy",
	);
	let engine = Engine::new(support::toolchain().await);

	let (outcome, events) = run_recording(&engine, &with_burn(job(&clip)), dir.path()).await;

	let JobOutcome::Encoded { output, warnings, .. } = &outcome else {
		panic!("{outcome:?}")
	};
	assert!(
		!warnings.iter().any(|warning| warning.contains("quality target")),
		"{warnings:?}"
	);
	assert_eq!(stages(&events, Stage::Sample), 1, "{events:?}");
	assert_eq!(stages(&events, Stage::Verify), 2, "{events:?}");
	assert_eq!(subtitle_count(output).await, 0);
}

fn advanced(job: Job, advanced: Advanced) -> Job {
	with_options(
		job,
		JobOptions {
			advanced,
			..JobOptions::default()
		},
	)
}

#[tokio::test]
async fn a_crf_override_skips_the_search_the_check_and_the_learning() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::mpeg4(dir.path(), "clip.mp4", 3);
	let memory = Arc::new(SharedMemory::default());
	let engine = Engine::new(support::toolchain().await).with_quality_memory(memory.clone());
	let fixed = advanced(
		job(&clip),
		Advanced {
			crf: Some(40.0),
			..Advanced::default()
		},
	);

	let (outcome, events) = run_recording(&engine, &fixed, dir.path()).await;

	assert!(
		matches!(&outcome, JobOutcome::Encoded { warnings, .. }
			if warnings.iter().any(|warning| warning.contains("--crf fixed the CRF at 40"))),
		"{outcome:?}"
	);
	assert_eq!(stages(&events, Stage::Sample), 0, "{events:?}");
	assert_eq!(stages(&events, Stage::Verify), 1, "only the final checks: {events:?}");
	assert!(memory.entries().is_empty());
}

#[tokio::test]
async fn a_fixed_crf_output_that_is_not_smaller_leaves_the_learned_group_alone() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::mpeg4(dir.path(), "clip.mp4", 3);
	let memory = Arc::new(learned_group(dir.path(), 30.0).await);
	let before = memory.entries();
	let engine = Engine::new(support::toolchain().await).with_quality_memory(memory.clone());
	let wasteful = Job {
		min_savings: 0.95,
		..advanced(
			job(&clip),
			Advanced {
				crf: Some(1.0),
				..Advanced::default()
			},
		)
	};

	let (outcome, events) = run_recording(&engine, &wasteful, dir.path()).await;

	assert!(
		matches!(
			outcome,
			JobOutcome::KeptOriginal {
				reason: KeepReason::NotSmaller { .. }
			}
		),
		"{outcome:?}"
	);
	assert_eq!(stages(&events, Stage::Sample), 0, "{events:?}");
	assert_eq!(memory.entries(), before);
}

#[tokio::test]
async fn a_fixed_crf_ignores_a_learned_efficiency() {
	let dir = tempfile::tempdir().unwrap();
	let first = support::video(
		dir.path(),
		"first.mp4",
		3,
		&[
			"-map",
			"0:v",
			"-map",
			"1:a",
			"-c:v",
			"libsvtav1",
			"-preset",
			"12",
			"-crf",
			"5",
			"-c:a",
			"aac",
		],
	);
	let second = dir.path().join("second.mp4");
	let third = dir.path().join("third.mp4");
	fs::copy(&first, &second).unwrap();
	fs::copy(&first, &third).unwrap();
	let learning = Arc::new(SharedMemory::default());
	let tools = support::toolchain().await;
	run_recording(
		&Engine::new(tools.clone()).with_quality_memory(learning.clone()),
		&job(&first),
		dir.path(),
	)
	.await;
	let (key, stats) = learning.entries().remove(0);
	let memory = Arc::new(SharedMemory::with_entries([(
		key,
		GroupStats {
			bpp: Some(1000.0),
			..stats
		},
	)]));
	let engine = Engine::new(tools).with_quality_memory(memory.clone());
	let fixed = advanced(
		job(&third),
		Advanced {
			crf: Some(35.0),
			..Advanced::default()
		},
	);

	let (learned, _) = run_recording(&engine, &job(&second), dir.path()).await;
	let (outcome, _) = run_recording(&engine, &fixed, dir.path()).await;

	assert!(
		matches!(
			learned,
			JobOutcome::KeptOriginal {
				reason: KeepReason::AlreadyEfficient(_)
			}
		),
		"{learned:?}"
	);
	assert!(matches!(outcome, JobOutcome::Encoded { .. }), "{outcome:?}");
}

#[tokio::test]
async fn an_aom_override_still_encodes_the_video() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::mpeg4(dir.path(), "clip.mp4", 2);
	let aom = advanced(
		fast(&clip),
		Advanced {
			video_encoder: Some(VideoEncoderChoice::Aom),
			..Advanced::default()
		},
	);

	let outcome = run(&aom, dir.path()).await;

	assert!(matches!(outcome, JobOutcome::Encoded { .. }), "{outcome:?}");
}

#[tokio::test]
async fn an_aom_override_without_libaom_fails_the_file() {
	let dir = tempfile::tempdir().unwrap();
	let clip = support::mpeg4(dir.path(), "clip.mp4", 2);
	let mut tools = support::toolchain().await;
	tools.encoders.remove(LIBAOM);
	let aom = advanced(
		fast(&clip),
		Advanced {
			video_encoder: Some(VideoEncoderChoice::Aom),
			..Advanced::default()
		},
	);

	let outcome = Engine::new(tools)
		.run(&aom, &sink(dir.path()), &|_| {}, &CancellationToken::new())
		.await;

	assert!(
		matches!(&outcome, JobOutcome::Failed(err)
			if err.to_string().contains("--video-encoder aom needs ffmpeg with libaom-av1")),
		"{outcome:?}"
	);
	assert!(!dir.path().join("clip.cry.mkv").exists());
}
