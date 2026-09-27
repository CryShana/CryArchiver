mod support;

use cryarchiver_engine::detect::{MediaClass, detect};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn raw_camera_files_are_kept_without_probing() {
	let dir = tempfile::tempdir().unwrap();
	let path = dir.path().join("DSC_0001.nef");
	std::fs::write(&path, b"II*\0\x08\0\0\0\0\0\0\0\0\0").unwrap();
	let tools = support::toolchain().await;

	let detection = detect(&tools, &path, &CancellationToken::new()).await.unwrap();

	assert!(matches!(detection.class, MediaClass::Kept(reason) if reason.contains("RAW")));
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
		matches!(detection.class, MediaClass::AnimatedImage(_)),
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

	assert!(matches!(detection.class, MediaClass::Other(_)));
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

	assert!(matches!(detection.class, MediaClass::Damaged(_)));
	assert!(!detection.probe_log.is_empty());
}

#[tokio::test]
async fn generated_flac_tone_is_audio() {
	let dir = tempfile::tempdir().unwrap();
	let path = support::tone(dir.path(), "a.flac", "stereo", 2, 1, &["-c:a", "flac"]);
	let tools = support::toolchain().await;

	let detection = detect(&tools, &path, &CancellationToken::new()).await.unwrap();

	assert!(matches!(detection.class, MediaClass::Audio(_)));
	assert_eq!(detection.meta.len(), std::fs::metadata(&path).unwrap().len());
}
