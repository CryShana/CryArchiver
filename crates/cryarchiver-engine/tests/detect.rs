mod support;

use cryarchiver_engine::detect::{Detected, MediaClass, detect};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn raw_camera_files_are_kept_without_probing() {
	let dir = tempfile::tempdir().unwrap();
	let path = dir.path().join("DSC_0001.nef");
	std::fs::write(&path, b"II*\0\x08\0\0\0\0\0\0\0\0\0").unwrap();
	let tools = support::toolchain().await;

	let detection = detect(&tools, &path, &CancellationToken::new()).await.unwrap();

	assert!(matches!(detection.class, Detected::Media(MediaClass::Kept(reason)) if reason.contains("RAW")));
}

#[tokio::test]
async fn animated_png_is_an_animated_image() {
	let dir = tempfile::tempdir().unwrap();
	let path = dir.path().join("anim.apng");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=32x32:d=0.5:r=10",
		"-f",
		"apng",
		path.to_str().unwrap(),
	]);
	let tools = support::toolchain().await;

	let detection = detect(&tools, &path, &CancellationToken::new()).await.unwrap();

	assert!(
		matches!(detection.class, Detected::Media(MediaClass::AnimatedImage(_))),
		"{:?}",
		detection.class
	);
}

#[tokio::test]
async fn plain_text_named_mp3_is_other() {
	let dir = tempfile::tempdir().unwrap();
	let path = dir.path().join("notes.mp3");
	std::fs::write(&path, b"just some plain text, not audio at all").unwrap();
	let tools = support::toolchain().await;

	let detection = detect(&tools, &path, &CancellationToken::new()).await.unwrap();

	assert!(matches!(detection.class, Detected::Media(MediaClass::Other(_))));
}

#[tokio::test]
async fn files_named_as_images_are_judged_by_their_content() {
	let dir = tempfile::tempdir().unwrap();
	let jpeg = dir.path().join("a.jpg");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=64x48",
		"-frames:v",
		"1",
		jpeg.to_str().unwrap(),
	]);
	let without_soi = dir.path().join("no-soi.jpg");
	std::fs::write(&without_soi, &std::fs::read(&jpeg).unwrap()[2..]).unwrap();
	let empty = dir.path().join("empty.jpg");
	std::fs::write(&empty, b"").unwrap();
	let html = dir.path().join("error.gif");
	std::fs::write(
		&html,
		b"<!DOCTYPE html><html><head><title>503 Service Unavailable</title></head><body>try again</body></html>",
	)
	.unwrap();
	let tools = support::toolchain().await;

	for path in [&without_soi, &empty, &html] {
		let detection = detect(&tools, path, &CancellationToken::new()).await.unwrap();
		assert!(
			matches!(detection.class, Detected::Media(MediaClass::Other(_))),
			"{}: {:?}",
			path.display(),
			detection.class
		);
	}
}

#[tokio::test]
async fn a_video_named_mp3_and_an_mp3_behind_junk_are_classified_by_their_streams() {
	let dir = tempfile::tempdir().unwrap();
	let video = support::mpeg4(dir.path(), "clip.mp4", 1);
	let named_mp3 = dir.path().join("actually-video.mp3");
	std::fs::rename(&video, &named_mp3).unwrap();
	let mp3 = dir.path().join("song.mp3");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"sine=frequency=440:duration=3",
		"-c:a",
		"libmp3lame",
		mp3.to_str().unwrap(),
	]);
	let junk: Vec<u8> = (0..4096u32).map(|i| u8::try_from(i * 7919 % 251).unwrap()).collect();
	let prefixed = dir.path().join("prefixed.mp3");
	std::fs::write(&prefixed, [junk, std::fs::read(&mp3).unwrap()].concat()).unwrap();
	let tools = support::toolchain().await;

	let video = detect(&tools, &named_mp3, &CancellationToken::new()).await.unwrap();
	let audio = detect(&tools, &prefixed, &CancellationToken::new()).await.unwrap();

	assert!(
		matches!(video.class, Detected::Media(MediaClass::Video(_))),
		"{:?}",
		video.class
	);
	assert!(
		matches!(audio.class, Detected::Media(MediaClass::Audio(_))),
		"{:?}",
		audio.class
	);
}

#[tokio::test]
async fn flac_signature_with_junk_is_damaged() {
	let dir = tempfile::tempdir().unwrap();
	let path = dir.path().join("broken.flac");
	let mut content = b"fLaC".to_vec();
	content.extend_from_slice(&[0u8; 64]);
	std::fs::write(&path, &content).unwrap();
	let tools = support::toolchain().await;

	let detection = detect(&tools, &path, &CancellationToken::new()).await.unwrap();

	assert!(matches!(detection.class, Detected::Media(MediaClass::Damaged(_))));
	assert!(!detection.probe_log.is_empty());
}

#[tokio::test]
async fn generated_flac_tone_is_audio() {
	let dir = tempfile::tempdir().unwrap();
	let path = support::tone(dir.path(), "a.flac", "stereo", 2, 1, &["-c:a", "flac"]);
	let tools = support::toolchain().await;

	let detection = detect(&tools, &path, &CancellationToken::new()).await.unwrap();

	assert!(matches!(detection.class, Detected::Media(MediaClass::Audio(_))));
	assert_eq!(detection.meta.len(), std::fs::metadata(&path).unwrap().len());
}
