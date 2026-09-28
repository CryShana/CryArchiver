mod support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cryarchiver_engine::container::isobmff::parse_meta;
use cryarchiver_engine::container::jxl::read_boxes;
use cryarchiver_engine::engine::{Engine, Job, JobEvent, Stage};
use cryarchiver_engine::images::animated;
use cryarchiver_engine::images::decode::oriented;
use cryarchiver_engine::images::ssim::score;
use cryarchiver_engine::metadata::exif::orientation;
use cryarchiver_engine::outcome::{ErrorKind, JobOutcome, KeepReason};
use cryarchiver_engine::preset::{Overrides, Speed};
use cryarchiver_engine::quality::memory::{GroupStats, SharedMemory};
use cryarchiver_engine::sink::CliSink;
use cryarchiver_engine::workspace::CLI_WORK_DIR;
use image::codecs::png::PngEncoder;
use image::{DynamicImage, GenericImageView, ImageEncoder};
use jxl_oxide::{JxlImage, PixelFormat};
use moxcms::ColorProfile;
use support::heif::{Coding, avif_grid, heic_grid_coded, heic_still_coded, imir, irot};
use tokio_util::sync::CancellationToken;

fn job(source: &Path) -> Job {
	Job {
		source: source.to_path_buf(),
		speed: Speed::Fast,
		keep_metadata: false,
		drop_aux: false,
		min_savings: 0.10,
		overrides: Overrides::default(),
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

const PHOTO_GROUP_AT_FAST: &str = "image/png/photo/opaque/8bit/mp1|ravif-0.13|p9|t80.0";

#[tokio::test]
async fn a_photo_teaches_its_group_the_quality_it_needs() {
	let dir = tempfile::tempdir().unwrap();
	let photo = support::photo_png(dir.path(), "photo.png", 320, 240);
	let memory = Arc::new(SharedMemory::default());
	let engine = Engine::new(support::toolchain().await)
		.with_image_threads(4)
		.with_quality_memory(memory.clone());

	let outcome = engine
		.run(&job(&photo), &sink(dir.path()), &|_| {}, &CancellationToken::new())
		.await;

	assert!(matches!(outcome, JobOutcome::Encoded { .. }), "{outcome:?}");
	let entries = memory.entries();
	assert_eq!(entries.len(), 1, "{entries:?}");
	assert_eq!(entries[0].0, PHOTO_GROUP_AT_FAST);
}

#[tokio::test]
async fn the_next_photo_of_a_group_starts_from_the_learned_quality() {
	let dir = tempfile::tempdir().unwrap();
	let photo = support::photo_png(dir.path(), "photo.png", 320, 240);
	let tools = support::toolchain().await;
	let generous = Arc::new(SharedMemory::with_entries([(
		PHOTO_GROUP_AT_FAST.to_owned(),
		GroupStats {
			setting: 92.0,
			slope: None,
			bpp: None,
			observations: 5,
		},
	)]));

	let plain = encoded(
		Engine::new(tools.clone())
			.with_image_threads(4)
			.run(
				&job(&photo),
				&sink(&dir.path().join("plain")),
				&|_| {},
				&CancellationToken::new(),
			)
			.await,
	);
	let learned = encoded(
		Engine::new(tools)
			.with_image_threads(4)
			.with_quality_memory(generous)
			.run(
				&job(&photo),
				&sink(&dir.path().join("learned")),
				&|_| {},
				&CancellationToken::new(),
			)
			.await,
	);

	assert!(
		std::fs::metadata(&learned).unwrap().len() > std::fs::metadata(&plain).unwrap().len(),
		"quality 92 must produce a larger AVIF than FAST's 88"
	);
}

#[tokio::test]
async fn a_group_that_learned_too_high_a_quality_falls_back_to_the_table_and_learns_lower() {
	let dir = tempfile::tempdir().unwrap();
	let photo = support::photo_png(dir.path(), "photo.png", 320, 240);
	let memory = Arc::new(SharedMemory::with_entries([(
		PHOTO_GROUP_AT_FAST.to_owned(),
		GroupStats {
			setting: 100.0,
			slope: None,
			bpp: None,
			observations: 5,
		},
	)]));
	let engine = Engine::new(support::toolchain().await)
		.with_image_threads(4)
		.with_quality_memory(memory.clone());

	let outcome = engine
		.run(&job(&photo), &sink(dir.path()), &|_| {}, &CancellationToken::new())
		.await;

	assert!(matches!(outcome, JobOutcome::Encoded { .. }), "{outcome:?}");
	let entries = memory.entries();
	assert_eq!(entries.len(), 1, "{entries:?}");
	assert!(entries[0].1.setting < 100.0, "{entries:?}");
	assert_eq!(entries[0].1.observations, 6, "{entries:?}");
}

const GRAPHIC_GROUP_AT_FAST: &str = "image/png/graphic/opaque/8bit/mp1|ravif-0.13|p9|t80.0";

fn palette_png(dir: &Path, name: &str) -> PathBuf {
	let path = dir.join(name);
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"gradients=s=320x240:c0=0x3060a0:c1=0xe0b070:nb_colors=2:x0=0:y0=0:x1=319:y1=239:speed=0,\
		 noise=alls=3:allf=u:all_seed=1",
		"-vf",
		"trim=end_frame=1,split[a][b];[a]palettegen=max_colors=256[p];[b][p]paletteuse=dither=none,format=rgb24",
		"-frames:v",
		"1",
		path.to_str().unwrap(),
	]);
	path
}

#[tokio::test]
async fn a_job_cancelled_after_a_candidate_passed_learns_nothing() {
	let dir = tempfile::tempdir().unwrap();
	let graphic = palette_png(dir.path(), "graphic.png");
	let tools = support::toolchain().await;
	let finished = Arc::new(SharedMemory::default());
	let cancelled = Arc::new(SharedMemory::default());
	let cancel = CancellationToken::new();
	let encodes = AtomicUsize::new(0);
	let cancel_the_second_encode = |event: JobEvent| {
		if event == JobEvent::Stage(Stage::Encode) && encodes.fetch_add(1, Ordering::SeqCst) == 1 {
			cancel.cancel();
		}
	};

	let completed = Engine::new(tools.clone())
		.with_image_threads(4)
		.with_quality_memory(finished.clone())
		.run(
			&job(&graphic),
			&sink(&dir.path().join("finished")),
			&|_| {},
			&CancellationToken::new(),
		)
		.await;
	let stopped = Engine::new(tools)
		.with_image_threads(4)
		.with_quality_memory(cancelled.clone())
		.run(
			&job(&graphic),
			&sink(&dir.path().join("cancelled")),
			&cancel_the_second_encode,
			&cancel,
		)
		.await;

	assert!(matches!(completed, JobOutcome::Encoded { .. }), "{completed:?}");
	assert_eq!(
		finished.entries().first().map(|(key, _)| key.as_str()),
		Some(GRAPHIC_GROUP_AT_FAST)
	);
	assert!(
		matches!(&stopped, JobOutcome::Failed(err) if err.kind == ErrorKind::Cancelled),
		"{stopped:?}"
	);
	assert!(cancelled.entries().is_empty(), "{:?}", cancelled.entries());
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
async fn a_sixteen_bit_png_holding_eight_bit_data_becomes_avif() {
	let dir = tempfile::tempdir().unwrap();
	let photo = image::open(support::photo_png(dir.path(), "photo.png", 320, 240))
		.unwrap()
		.to_rgb8();
	let source = dir.path().join("padded.png");
	image::ImageBuffer::from_fn(320, 240, |x, y| {
		image::Rgb(photo.get_pixel(x, y).0.map(|sample| u16::from(sample) * 257))
	})
	.save(&source)
	.unwrap();

	let output = encoded(run(&job(&source), dir.path()).await);

	assert_eq!(output, dir.path().join("padded.cry.avif"));
}

#[tokio::test]
async fn an_image_too_tall_for_avif_becomes_jxl() {
	let dir = tempfile::tempdir().unwrap();
	let source = support::smooth_png(dir.path(), "tall.png", 256, 16_500);

	let output = encoded(run(&job(&source), dir.path()).await);

	assert_eq!(output, dir.path().join("tall.cry.jxl"));
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
async fn an_animation_is_checked_against_the_image_override() {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("anim.apng");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=160x120:d=1:r=10",
		"-f",
		"apng",
		source.to_str().unwrap(),
	]);
	let with_target = |target: f64| Job {
		min_savings: 0.0,
		overrides: Overrides {
			image: Some(target),
			..Overrides::default()
		},
		..job(&source)
	};

	let strict = run(&with_target(95.0), &dir.path().join("strict")).await;
	let lenient = run(&with_target(50.0), &dir.path().join("lenient")).await;

	assert!(
		matches!(&strict, JobOutcome::KeptOriginal { reason: KeepReason::QualityUnreachable(reason) } if reason.contains("target 95)")),
		"{strict:?}"
	);
	assert!(matches!(lenient, JobOutcome::Encoded { .. }), "{lenient:?}");
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

fn decoded_png(dir: &Path, source: &Path, name: &str) -> DynamicImage {
	let png = dir.join(name);
	support::ffmpeg(&["-i", source.to_str().unwrap(), "-frames:v", "1", png.to_str().unwrap()]);
	image::open(png).unwrap()
}

async fn oriented_output(
	dir: &Path,
	name: &str,
	source: &[u8],
	upright: &[u8],
) -> (PathBuf, DynamicImage, DynamicImage) {
	let source_path = dir.join(name);
	std::fs::write(&source_path, source).unwrap();
	let upright_path = dir.join(format!("upright-{name}"));
	std::fs::write(&upright_path, upright).unwrap();

	let output = encoded(
		run(
			&Job {
				min_savings: 0.0,
				..job(&source_path)
			},
			dir,
		)
		.await,
	);
	let expected = decoded_png(dir, &upright_path, &format!("{name}.expected.png"));
	let written = decoded_png(dir, &output, &format!("{name}.written.png"));
	(output, expected, written)
}

fn assert_same_picture(name: &str, expected: &DynamicImage, written: &DynamicImage) {
	assert_eq!(written.dimensions(), expected.dimensions(), "{name}");
	let similarity = score(expected, written).unwrap();
	assert!(similarity > 70.0, "{name}: {similarity}");
}

fn heic_case(dir: &Path, grid: bool, coding: Coding, transforms: Vec<Vec<u8>>) -> Vec<u8> {
	if grid {
		heic_grid_coded(dir, 128, 96, coding, transforms)
	} else {
		heic_still_coded(dir, 256, 192, coding, transforms, None)
	}
}

async fn heic_orientation_cases(ten_bit: bool, extension: &str) {
	let dir = tempfile::tempdir().unwrap();
	let coding = Coding {
		ten_bit,
		lossless: true,
		smooth: true,
	};
	let cases = [
		("grid-rot.heic", true, irot(1), 8),
		("grid-mirror.heic", true, imir(0), 4),
		("single-rot.heic", false, irot(3), 6),
	];

	for (name, grid, transform, orientation) in cases {
		let source = heic_case(dir.path(), grid, coding, vec![transform]);
		let upright = heic_case(dir.path(), grid, coding, Vec::new());

		let (output, expected, written) = oriented_output(dir.path(), name, &source, &upright).await;

		assert_eq!(output.extension().unwrap(), extension, "{name}");
		assert_same_picture(name, &oriented(expected, orientation), &written);
	}
}

#[tokio::test]
async fn tiled_and_single_heic_are_oriented_once_on_the_avif_route() {
	heic_orientation_cases(false, "avif").await;
}

#[tokio::test]
async fn tiled_and_single_heic_are_oriented_once_on_the_jxl_route() {
	heic_orientation_cases(true, "jxl").await;
}

#[tokio::test]
async fn a_tiled_avif_whose_tiles_repeat_the_rotation_is_rotated_once() {
	let dir = tempfile::tempdir().unwrap();
	let source = avif_grid(dir.path(), 128, 96, &[irot(1)]);
	let upright = avif_grid(dir.path(), 128, 96, &[]);

	let (output, expected, written) = oriented_output(dir.path(), "tiles.avif", &source, &upright).await;

	assert_eq!(output.extension().unwrap(), "avif");
	assert_same_picture("tiles.avif", &oriented(expected, 8), &written);
}

#[tokio::test]
async fn a_ten_bit_avif_with_alpha_keeps_its_transparency_as_jxl() {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("alpha10.avif");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=256x192,gblur=sigma=1.5,format=rgba,geq=r='r(X,Y)':g='g(X,Y)':b='b(X,Y)':a='if(lt(X,128),255,0)'",
		"-filter_complex",
		"[0:v]split[c][a];[c]format=yuv444p10le[co];[a]alphaextract,format=gray10le[ao]",
		"-map",
		"[co]",
		"-map",
		"[ao]",
		"-c:v",
		"libaom-av1",
		"-still-picture",
		"1",
		"-crf",
		"0",
		"-cpu-used",
		"8",
		"-frames:v",
		"1",
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

	assert_eq!(output.extension().unwrap(), "jxl");
	let image = JxlImage::builder()
		.read(std::fs::read(&output).unwrap().as_slice())
		.unwrap();
	assert!(image.pixel_format().has_alpha());
	let render = image.render_frame(0).unwrap();
	let mut stream = render.stream();
	let channels = stream.channels() as usize;
	let mut samples = vec![0f32; 256 * 192 * channels];
	stream.write_to_buffer(&mut samples);
	let alpha = |x: usize, y: usize| samples[(y * 256 + x) * channels + channels - 1];
	assert!(alpha(10, 80) > 0.99, "{}", alpha(10, 80));
	assert!(alpha(220, 80) < 0.01, "{}", alpha(220, 80));
}

#[tokio::test]
async fn animated_avif_is_kept_including_our_own_output() {
	let dir = tempfile::tempdir().unwrap();
	let sequence = dir.path().join("sequence.avif");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=160x120:d=3:r=10",
		"-c:v",
		"libaom-av1",
		"-crf",
		"30",
		"-cpu-used",
		"8",
		sequence.to_str().unwrap(),
	]);
	let apng = dir.path().join("anim.apng");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=160x120:d=0.5:r=10",
		"-f",
		"apng",
		apng.to_str().unwrap(),
	]);
	let ours = encoded(
		run(
			&Job {
				min_savings: 0.0,
				..job(&apng)
			},
			dir.path(),
		)
		.await,
	);
	let fed_back = dir.path().join("fed-back.avif");
	std::fs::copy(&ours, &fed_back).unwrap();

	for source in [sequence, fed_back] {
		let outcome = run(
			&Job {
				min_savings: 0.0,
				..job(&source)
			},
			dir.path(),
		)
		.await;

		assert!(
			matches!(&outcome, JobOutcome::KeptOriginal { reason: KeepReason::Unsupported(reason) } if reason.contains("animated AVIF")),
			"{source:?}: {outcome:?}"
		);
	}
}

#[tokio::test]
async fn a_gif_without_a_loop_extension_plays_once() {
	let dir = tempfile::tempdir().unwrap();
	let once = dir.path().join("once.gif");
	let forever = dir.path().join("forever.gif");
	for (path, loops) in [(&once, "-1"), (&forever, "0")] {
		support::ffmpeg(&[
			"-f",
			"lavfi",
			"-i",
			"testsrc2=s=320x240:d=1:r=10",
			"-loop",
			loops,
			path.to_str().unwrap(),
		]);
	}
	let fast = |source: &Path| Job {
		min_savings: 0.0,
		..job(source)
	};

	let once_output = encoded(run(&fast(&once), dir.path()).await);
	let forever_output = encoded(run(&fast(&forever), dir.path()).await);

	assert_eq!(animated::plays(&std::fs::read(once_output).unwrap()), Some(Some(1)));
	assert_eq!(animated::plays(&std::fs::read(forever_output).unwrap()), Some(None));
}

#[tokio::test]
async fn an_animation_with_a_colour_profile_warns_that_it_is_dropped() {
	let dir = tempfile::tempdir().unwrap();
	let apng = dir.path().join("profiled.apng");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=96x64:d=0.5:r=10",
		"-f",
		"apng",
		apng.to_str().unwrap(),
	]);
	let mut compressed = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
	compressed
		.write_all(&ColorProfile::new_srgb().encode().unwrap())
		.unwrap();
	let body = [b"sRGB\0\0".as_slice(), &compressed.finish().unwrap()].concat();
	let chunk = support::png_chunk(*b"iCCP", &body);
	std::fs::write(&apng, support::with_png_chunk(&std::fs::read(&apng).unwrap(), &chunk)).unwrap();

	let outcome = run(
		&Job {
			min_savings: 0.0,
			..job(&apng)
		},
		dir.path(),
	)
	.await;

	assert!(
		matches!(&outcome, JobOutcome::Encoded { warnings, .. } if warnings.iter().any(|warning| warning.contains("colour profile"))),
		"{outcome:?}"
	);
}

fn motion_photo_jpeg(dir: &Path, name: &str, xmp: Option<&str>, trailer: &[u8]) -> PathBuf {
	let jpeg = support::jpeg_with_exif(dir, name, 320, 240, 1);
	let mut bytes = std::fs::read(&jpeg).unwrap();
	if let Some(packet) = xmp {
		let payload = [b"http://ns.adobe.com/xap/1.0/\0".as_slice(), packet.as_bytes()].concat();
		bytes = support::with_jpeg_segment(&bytes, 0xE1, &payload);
	}
	bytes.extend_from_slice(trailer);
	std::fs::write(&jpeg, bytes).unwrap();
	jpeg
}

#[tokio::test]
async fn motion_photos_keep_the_original_unless_auxiliary_content_is_dropped() {
	let dir = tempfile::tempdir().unwrap();
	let mut samsung_trailer = b"\0\0\0\x18ftypisom\0\0\0\0isommp41".to_vec();
	samsung_trailer.extend_from_slice(&[0x42; 4000]);
	samsung_trailer.extend_from_slice(b"MotionPhoto_Data\0\0\0\0SEFT");
	let samsung = motion_photo_jpeg(dir.path(), "samsung.jpg", None, &samsung_trailer);
	let pixel = motion_photo_jpeg(
		dir.path(),
		"pixel.jpg",
		Some(
			"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><rdf:Description rdf:about='' xmlns:GCamera='http://ns.google.com/photos/1.0/camera/' GCamera:MicroVideo='1'/></rdf:RDF></x:xmpmeta>",
		),
		&[],
	);

	for source in [&samsung, &pixel] {
		let kept = run(&job(source), dir.path()).await;
		let dropped = run(
			&Job {
				drop_aux: true,
				min_savings: 0.0,
				..job(source)
			},
			dir.path(),
		)
		.await;

		assert!(
			matches!(&kept, JobOutcome::KeptOriginal { reason: KeepReason::Unsupported(reason) } if reason.contains("motion photo video")),
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
}
