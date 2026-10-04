mod support;

use std::fs;
use std::path::{Path, PathBuf};

use cryarchiver_engine::engine::{Engine, Job};
use cryarchiver_engine::options::JobOptions;
use cryarchiver_engine::outcome::{ErrorKind, JobOutcome, KeepReason};
use cryarchiver_engine::preset::{Overrides, Speed};
use cryarchiver_engine::probe::{MediaInfo, probe};
use cryarchiver_engine::sink::CliSink;
use cryarchiver_engine::workspace::CLI_WORK_DIR;
use tokio_util::sync::CancellationToken;

fn job(source: &Path, speed: Speed) -> Job {
	Job {
		source: source.to_path_buf(),
		speed,
		keep_metadata: false,
		drop_aux: false,
		min_savings: 0.0,
		overrides: Overrides::default(),
		options: JobOptions::default(),
	}
}

async fn run(source: &Path, speed: Speed, out: &Path) -> JobOutcome {
	let engine = Engine::new(support::toolchain().await).with_image_threads(4);
	let sink = CliSink {
		output_dir: Some(out.to_path_buf()),
		output_file: None,
		overwrite: false,
	};

	engine
		.run(&job(source, speed), &sink, &|_| {}, &CancellationToken::new())
		.await
}

async fn encoded_info(outcome: &JobOutcome) -> MediaInfo {
	let JobOutcome::Encoded { output, .. } = outcome else {
		panic!("{outcome:?}")
	};

	probe(&support::toolchain().await, output, &CancellationToken::new())
		.await
		.unwrap()
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
	let path = dir.join(name);
	fs::write(&path, bytes).unwrap();
	path
}

fn noise(length: usize, seed: u32) -> Vec<u8> {
	let mut state = seed.max(1);
	(0..length)
		.map(|_| {
			state ^= state << 13;
			state ^= state >> 17;
			state ^= state << 5;
			state.to_le_bytes()[0]
		})
		.collect()
}

fn truncated(path: &Path, tenths: usize) -> PathBuf {
	let bytes = fs::read(path).unwrap();
	let name = format!("cut-{}", path.file_name().unwrap().to_str().unwrap());
	write(path.parent().unwrap(), &name, &bytes[..bytes.len() * tenths / 10])
}

fn damaged(outcome: &JobOutcome) -> bool {
	matches!(outcome, JobOutcome::Failed(err) if matches!(err.kind, ErrorKind::SourceDamaged(_)))
}

fn converted_or_kept(outcome: &JobOutcome) -> bool {
	matches!(outcome, JobOutcome::Encoded { .. } | JobOutcome::KeptOriginal { .. })
}

fn nothing_left(out: &Path) -> bool {
	!out.exists() || fs::read_dir(out).unwrap().next().is_none()
}

/// The first IDAT with its compressed data cut in half (length and CRC fixed), or with only its CRC flipped.
fn with_broken_idat(png: &[u8], cut_data: bool) -> Vec<u8> {
	let mut at = 8;
	while at + 12 <= png.len() {
		let length = usize::try_from(u32::from_be_bytes(png[at..at + 4].try_into().unwrap())).unwrap();
		let end = at + 12 + length;
		if &png[at + 4..at + 8] != b"IDAT" {
			at = end;
			continue;
		}

		let chunk = if cut_data {
			let kept = length / 2;
			let mut body = b"IDAT".to_vec();
			body.extend_from_slice(&png[at + 8..at + 8 + kept]);
			[
				u32::try_from(kept).unwrap().to_be_bytes().as_slice(),
				&body,
				&crc32fast::hash(&body).to_be_bytes(),
			]
			.concat()
		} else {
			let mut chunk = png[at..end].to_vec();
			let crc = chunk.len() - 4;
			chunk[crc] ^= 0xFF;
			chunk
		};
		return [&png[..at], chunk.as_slice(), &png[end..]].concat();
	}
	panic!("the PNG has no IDAT chunk")
}

#[tokio::test]
async fn files_that_only_look_like_video_are_never_encoded() {
	let dir = tempfile::tempdir().unwrap();
	let out = dir.path().join("out");
	let clip = support::video(
		dir.path(),
		"clip.mkv",
		2,
		&["-map", "0:v", "-map", "1:a", "-c:v", "mpeg4", "-c:a", "flac"],
	);
	let mut blanked = fs::read(&clip).unwrap();
	blanked[..4096].fill(0);
	let blank_header = write(dir.path(), "blank-header.mkv", &blanked);
	let static_noise = write(dir.path(), "static.mkv", &noise(1 << 20, 7));
	let text = write(dir.path(), "notes.mp4", b"shopping list: milk, bread, film\n");
	let empty = write(dir.path(), "empty.mp4", b"");

	for path in [&blank_header, &static_noise] {
		let outcome = run(path, Speed::Fast, &out).await;
		assert!(
			matches!(outcome, JobOutcome::Skipped { .. }) || damaged(&outcome),
			"{}: {outcome:?}",
			path.display()
		);
	}

	for path in [&text, &empty] {
		let outcome = run(path, Speed::Fast, &out).await;
		assert!(
			matches!(outcome, JobOutcome::Skipped { .. }),
			"{}: {outcome:?}",
			path.display()
		);
	}

	assert!(nothing_left(&out));
}

#[tokio::test]
async fn truncated_mp3_gif_jpeg_and_avif_fail_as_damaged() {
	let dir = tempfile::tempdir().unwrap();
	let out = dir.path().join("out");
	let mp3 = support::tone(
		dir.path(),
		"song.mp3",
		"stereo",
		2,
		10,
		&["-c:a", "libmp3lame", "-b:a", "192k"],
	);
	let gif = dir.path().join("loop.gif");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=160x120:r=10:d=2",
		gif.to_str().unwrap(),
	]);
	let jpeg = dir.path().join("photo.jpg");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=640x480",
		"-frames:v",
		"1",
		jpeg.to_str().unwrap(),
	]);
	let png = support::photo_png(dir.path(), "picture.png", 320, 240);
	let JobOutcome::Encoded { output: avif, .. } = run(&png, Speed::Fast, dir.path()).await else {
		panic!("the PNG was not encoded to AVIF");
	};

	for (path, tenths) in [(&mp3, 4), (&gif, 5), (&jpeg, 5), (&avif, 6)] {
		let cut = truncated(path, tenths);
		let outcome = run(&cut, Speed::Fast, &out).await;
		assert!(damaged(&outcome), "{}: {outcome:?}", cut.display());
	}

	assert!(nothing_left(&out));
}

#[tokio::test]
async fn broken_pixel_data_fails_but_broken_checksums_scans_and_profiles_do_not() {
	let dir = tempfile::tempdir().unwrap();
	let out = dir.path().join("out");
	let png = fs::read(support::photo_png(dir.path(), "source.png", 320, 240)).unwrap();
	let cut_data = write(dir.path(), "cut-data.png", &with_broken_idat(&png, true));
	let bad_crc = write(dir.path(), "bad-crc.png", &with_broken_idat(&png, false));
	let jpeg_path = dir.path().join("source.jpg");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=320x240",
		"-frames:v",
		"1",
		jpeg_path.to_str().unwrap(),
	]);
	let jpeg = fs::read(&jpeg_path).unwrap();
	let mut scrambled = jpeg.clone();
	let middle = scrambled.len() / 2;
	scrambled[middle..middle + 64].fill(0x55);
	let scrambled = write(dir.path(), "scrambled.jpg", &scrambled);
	let mut icc = b"ICC_PROFILE\0\x01\x01".to_vec();
	icc.extend_from_slice(&u32::MAX.to_be_bytes());
	icc.extend_from_slice(&[0; 124]);
	let broken_icc = write(
		dir.path(),
		"broken-profile.jpg",
		&support::with_jpeg_segment(&jpeg, 0xE2, &icc),
	);

	let cut = run(&cut_data, Speed::Fast, &out).await;
	let crc = run(&bad_crc, Speed::Fast, &out).await;
	let scan = run(&scrambled, Speed::Fast, &out).await;
	let profile = run(&broken_icc, Speed::Fast, &out).await;

	assert!(damaged(&cut), "{cut:?}");
	assert!(converted_or_kept(&crc), "{crc:?}");
	assert!(converted_or_kept(&scan), "{scan:?}");
	assert!(
		matches!(
			&profile,
			JobOutcome::KeptOriginal {
				reason: KeepReason::InvalidIcc(_)
			}
		),
		"{profile:?}"
	);
	assert!(!out.join(CLI_WORK_DIR).exists());
}

#[tokio::test]
async fn bmp_tga_ppm_qoi_and_a_jpeg_without_an_extension_are_converted_or_kept() {
	let dir = tempfile::tempdir().unwrap();
	let out = dir.path().join("out");
	let mut sources = Vec::new();
	for (name, pix_fmt) in [
		("a.bmp", "bgr24"),
		("b.tga", "bgra"),
		("c.ppm", "rgb24"),
		("d.qoi", "rgba"),
	] {
		let path = dir.path().join(name);
		support::ffmpeg(&[
			"-f",
			"lavfi",
			"-i",
			"testsrc2=s=256x192",
			"-frames:v",
			"1",
			"-pix_fmt",
			pix_fmt,
			path.to_str().unwrap(),
		]);
		sources.push(path);
	}

	let jpeg = dir.path().join("e.jpg");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=256x192",
		"-frames:v",
		"1",
		jpeg.to_str().unwrap(),
	]);
	let bare = dir.path().join("scan");
	fs::rename(&jpeg, &bare).unwrap();
	sources.push(bare);

	for source in &sources {
		let outcome = run(source, Speed::Fast, &out).await;
		assert!(converted_or_kept(&outcome), "{}: {outcome:?}", source.display());
	}
}

#[tokio::test]
async fn two_angles_odd_4_4_4_frames_and_frames_wider_than_svt_are_encoded() {
	let dir = tempfile::tempdir().unwrap();
	let out = dir.path().join("out");
	let tools = support::toolchain().await;
	let angles = dir.path().join("angles.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=320x240:r=25:d=2",
		"-f",
		"lavfi",
		"-i",
		"smptebars=s=320x240:r=25:d=2",
		"-map",
		"0",
		"-map",
		"1",
		"-c:v",
		"mpeg4",
		angles.to_str().unwrap(),
	]);
	let odd = dir.path().join("odd.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=321x241:r=25:d=2",
		"-c:v",
		"ffv1",
		"-pix_fmt",
		"yuv444p",
		odd.to_str().unwrap(),
	]);
	let wide = dir.path().join("wide.avi");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=16400x64:r=5:d=1",
		"-c:v",
		"mjpeg",
		"-q:v",
		"3",
		wide.to_str().unwrap(),
	]);

	let two = run(&angles, Speed::Fast, &out).await;
	let info = encoded_info(&two).await;
	assert_eq!(info.streams.iter().filter(|stream| stream.is_real_video()).count(), 2);

	let odd = run(&odd, Speed::Fast, &out).await;
	assert!(converted_or_kept(&odd), "{odd:?}");

	let wide = run(&wide, Speed::Fast, &out).await;
	if tools.has_libaom() {
		assert!(matches!(wide, JobOutcome::Encoded { .. }), "{wide:?}");
	} else {
		assert!(
			matches!(&wide, JobOutcome::Failed(err) if matches!(err.kind, ErrorKind::Toolchain(_))),
			"{wide:?}"
		);
	}
}

#[tokio::test]
async fn a_black_opening_still_leaves_a_searchable_video() {
	let dir = tempfile::tempdir().unwrap();
	let clip = dir.path().join("late-start.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"color=c=black:s=320x240:r=25:d=8",
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=320x240:r=25:d=10",
		"-filter_complex",
		"[0][1]concat=n=2:v=1:a=0",
		"-c:v",
		"mpeg4",
		"-q:v",
		"2",
		clip.to_str().unwrap(),
	]);

	let outcome = run(&clip, Speed::Moderate, &dir.path().join("out")).await;

	assert!(matches!(outcome, JobOutcome::Encoded { .. }), "{outcome:?}");
}
