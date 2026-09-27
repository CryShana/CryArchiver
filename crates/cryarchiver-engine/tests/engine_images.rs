mod support;

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use cryarchiver_engine::container::isobmff::parse_meta;
use cryarchiver_engine::container::jxl::read_boxes;
use cryarchiver_engine::engine::{Engine, Job, JobEvent, Stage};
use cryarchiver_engine::images::animated;
use cryarchiver_engine::metadata::exif::orientation;
use cryarchiver_engine::outcome::{ErrorKind, JobOutcome, KeepReason};
use cryarchiver_engine::preset::Speed;
use cryarchiver_engine::sink::CliSink;
use cryarchiver_engine::workspace::CLI_WORK_DIR;
use image::ImageEncoder;
use image::codecs::png::PngEncoder;
use jxl_oxide::{JxlImage, PixelFormat};
use moxcms::ColorProfile;
use tokio_util::sync::CancellationToken;

fn job(source: &Path) -> Job {
	Job {
		source: source.to_path_buf(),
		speed: Speed::Fast,
		keep_metadata: false,
		drop_aux: false,
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
	let engine = Engine::new(support::toolchain().await).with_image_threads(4);
	engine
		.run(job, &sink(out_dir), &|_| {}, &CancellationToken::new())
		.await
}

fn encoded(outcome: JobOutcome) -> PathBuf {
	match outcome {
		JobOutcome::Encoded { output, .. } => output,
		other => panic!("{other:?}"),
	}
}

fn rotated_png(dir: &Path, name: &str) -> PathBuf {
	let path = support::photo_png(dir, name, 320, 240);
	let exif = support::png_chunk(*b"eXIf", &support::exif_block(6, 320, 240));
	let png = support::with_png_chunk(&std::fs::read(&path).unwrap(), &exif);
	std::fs::write(&path, png).unwrap();
	path
}

#[tokio::test]
async fn a_rotated_png_becomes_an_upright_avif_without_personal_metadata() {
	let dir = tempfile::tempdir().unwrap();
	let source = rotated_png(dir.path(), "photo.png");

	let output = encoded(run(&job(&source), dir.path()).await);

	assert_eq!(output, dir.path().join("photo.cry.avif"));
	let bytes = std::fs::read(&output).unwrap();
	let meta = parse_meta(&bytes).unwrap();
	assert_eq!(meta.primary_size(), Some((240, 320)));
	assert!(meta.exif_tiff(&bytes).is_none());
	assert!(!dir.path().join(CLI_WORK_DIR).exists());
}

#[tokio::test]
async fn keep_metadata_writes_upright_exif_into_the_avif() {
	let dir = tempfile::tempdir().unwrap();
	let source = rotated_png(dir.path(), "photo.png");

	let output = encoded(
		run(
			&Job {
				keep_metadata: true,
				..job(&source)
			},
			dir.path(),
		)
		.await,
	);

	let bytes = std::fs::read(&output).unwrap();
	let exif = parse_meta(&bytes).unwrap().exif_tiff(&bytes).unwrap().unwrap();
	assert_eq!(orientation(&exif), Some(1));
	assert!(exif.windows(4).any(|w| w == 240u32.to_le_bytes()));
}

#[tokio::test]
async fn sixteen_bit_images_become_jxl() {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("deep.png");
	image::ImageBuffer::from_fn(256, 192, |x, y| {
		image::Rgb([
			u16::try_from(x * 251 + y).unwrap(),
			u16::try_from(y * 331).unwrap(),
			12_345,
		])
	})
	.save(&source)
	.unwrap();

	let output = encoded(run(&job(&source), dir.path()).await);

	assert_eq!(output, dir.path().join("deep.cry.jxl"));
	assert!(read_boxes(&std::fs::read(output).unwrap()).unwrap().container);
}

#[tokio::test]
async fn grey_profiled_photos_become_grey_jxl() {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("scan.png");
	let mut encoder = PngEncoder::new(std::fs::File::create(&source).unwrap());
	encoder
		.set_icc_profile(ColorProfile::new_gray_with_gamma(2.2).encode().unwrap())
		.unwrap();
	image::ImageBuffer::from_fn(320, 240, |x, y| {
		image::Luma([u8::try_from((x * 7 + y * 13 + (x * y) % 17) % 256).unwrap()])
	})
	.write_with_encoder(encoder)
	.unwrap();

	let output = encoded(
		run(
			&Job {
				min_savings: 0.0,
				..job(&source)
			},
			dir.path(),
		)
		.await,
	);

	assert_eq!(output, dir.path().join("scan.cry.jxl"));
	let image = JxlImage::builder()
		.read(std::fs::read(&output).unwrap().as_slice())
		.unwrap();
	assert!(matches!(image.pixel_format(), PixelFormat::Gray));
}

#[tokio::test]
async fn auxiliary_images_keep_the_original_unless_dropped() {
	let dir = tempfile::tempdir().unwrap();
	let jpeg = support::jpeg_with_exif(dir.path(), "hdr.jpg", 320, 240, 1);
	let xmp = [
		b"http://ns.adobe.com/xap/1.0/\0".as_slice(),
		b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><rdf:Description rdf:about='' xmlns:hdrgm='http://ns.adobe.com/hdr-gain-map/1.0/' hdrgm:Version='1.0'/></rdf:RDF></x:xmpmeta>",
	]
	.concat();
	std::fs::write(
		&jpeg,
		support::with_jpeg_segment(&std::fs::read(&jpeg).unwrap(), 0xE1, &xmp),
	)
	.unwrap();

	let kept = run(&job(&jpeg), dir.path()).await;
	let dropped = run(
		&Job {
			drop_aux: true,
			min_savings: 0.0,
			..job(&jpeg)
		},
		dir.path(),
	)
	.await;

	assert!(
		matches!(&kept, JobOutcome::KeptOriginal { reason: KeepReason::Unsupported(reason) } if reason.contains("gain map")),
		"{kept:?}"
	);
	assert!(
		!matches!(
			dropped,
			JobOutcome::KeptOriginal {
				reason: KeepReason::Unsupported(_)
			} | JobOutcome::Failed(_)
		),
		"{dropped:?}"
	);
}

#[tokio::test]
async fn truncated_images_fail_before_encoding_and_bombs_are_kept() {
	let dir = tempfile::tempdir().unwrap();
	let source = support::smooth_png(dir.path(), "cut.png", 320, 240);
	let bytes = std::fs::read(&source).unwrap();
	std::fs::write(&source, &bytes[..bytes.len() / 2]).unwrap();
	let mut bomb = b"\x89PNG\r\n\x1a\n".to_vec();
	let header = [
		100_000u32.to_be_bytes().as_slice(),
		&100_000u32.to_be_bytes(),
		&[8, 2, 0, 0, 0],
	]
	.concat();
	bomb.extend(support::png_chunk(*b"IHDR", &header));
	bomb.extend(support::png_chunk(*b"IDAT", &[0; 8]));
	bomb.extend(support::png_chunk(*b"IEND", &[]));
	let bomb_path = dir.path().join("bomb.png");
	std::fs::write(&bomb_path, bomb).unwrap();
	let stages = Mutex::new(Vec::new());

	let damaged = Engine::new(support::toolchain().await)
		.run(
			&job(&source),
			&sink(dir.path()),
			&|event| stages.lock().unwrap().push(event),
			&CancellationToken::new(),
		)
		.await;
	let kept = run(&job(&bomb_path), dir.path()).await;

	assert!(
		matches!(&damaged, JobOutcome::Failed(err) if matches!(err.kind, ErrorKind::SourceDamaged(_))),
		"{damaged:?}"
	);
	assert!(!stages.lock().unwrap().contains(&JobEvent::Stage(Stage::Encode)));
	assert!(
		matches!(
			kept,
			JobOutcome::KeptOriginal {
				reason: KeepReason::DecodeBomb { megapixels: 10_000 }
			}
		),
		"{kept:?}"
	);
	assert!(!dir.path().join(CLI_WORK_DIR).exists());
}

#[tokio::test]
async fn tiny_images_are_kept_or_encoded_but_never_fail() {
	let dir = tempfile::tempdir().unwrap();
	let tiny = support::smooth_png(dir.path(), "tiny.png", 3, 3);

	let outcome = run(
		&Job {
			min_savings: 0.0,
			..job(&tiny)
		},
		dir.path(),
	)
	.await;

	assert!(!matches!(outcome, JobOutcome::Failed(_)), "{outcome:?}");
}

#[tokio::test]
async fn an_animated_png_keeps_its_frames_timing_and_loop_count() {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("anim.apng");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=160x120:d=1:r=10",
		"-plays",
		"3",
		"-f",
		"apng",
		source.to_str().unwrap(),
	]);

	let output = encoded(
		run(
			&Job {
				min_savings: 0.0,
				..job(&source)
			},
			dir.path(),
		)
		.await,
	);

	assert_eq!(animated::plays(&std::fs::read(output).unwrap()), Some(Some(3)));
}

#[tokio::test]
async fn a_single_frame_gif_becomes_a_still_avif() {
	let dir = tempfile::tempdir().unwrap();
	let png = support::smooth_png(dir.path(), "frame.png", 160, 120);
	let gif = dir.path().join("one.gif");
	support::ffmpeg(&["-i", png.to_str().unwrap(), "-frames:v", "1", gif.to_str().unwrap()]);

	let outcome = run(
		&Job {
			min_savings: 0.0,
			..job(&gif)
		},
		dir.path(),
	)
	.await;

	if let JobOutcome::Encoded { output, .. } = &outcome {
		assert!(
			animated::plays(&std::fs::read(output).unwrap()).is_none(),
			"an animated AVIF was written"
		);
	} else {
		assert!(matches!(outcome, JobOutcome::KeptOriginal { .. }), "{outcome:?}");
	}
}

#[test]
fn cancelling_a_long_still_encode_returns_promptly_and_leaves_nothing() {
	let dir = tempfile::tempdir().unwrap();
	let source = support::smooth_png(dir.path(), "large.png", 4000, 3000);
	let slow = Job {
		speed: Speed::Slow,
		..job(&source)
	};
	let runtime = tokio::runtime::Runtime::new().unwrap();

	let (outcome, elapsed) = runtime.block_on(async {
		let engine = Engine::new(support::toolchain().await).with_image_threads(1);
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
		let started = Instant::now();
		let outcome = engine.run(&slow, &sink(dir.path()), &on_event, &cancel).await;
		(outcome, started.elapsed())
	});
	runtime.shutdown_background();

	assert!(
		matches!(&outcome, JobOutcome::Failed(err) if err.kind == ErrorKind::Cancelled),
		"{outcome:?}"
	);
	assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
	assert!(!dir.path().join(CLI_WORK_DIR).exists());
}
