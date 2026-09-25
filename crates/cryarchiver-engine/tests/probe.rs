mod support;

use cryarchiver_engine::probe::{ProbeError, probe};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn probes_a_generated_flac() {
	let dir = tempfile::tempdir().unwrap();
	let flac = support::tone(dir.path(), "tone.flac", "stereo", 2, 2, &["-c:a", "flac"]);
	let tools = support::toolchain().await;

	let info = probe(&tools, &flac, &CancellationToken::new()).await.unwrap();

	let audio = info.audio_streams().next().unwrap();
	assert_eq!(audio.codec_name, "flac");
	assert_eq!(audio.channels, Some(2));
	assert_eq!(audio.sample_rate, Some(48_000));
	assert!((info.format.duration.unwrap() - 2.0).abs() < 0.05);
	assert!(!info.format.duration_estimated);
}

#[tokio::test]
async fn adts_aac_duration_is_an_estimate() {
	let dir = tempfile::tempdir().unwrap();
	let aac = support::tone(dir.path(), "a.aac", "stereo", 2, 3, &["-c:a", "aac", "-f", "adts"]);
	let tools = support::toolchain().await;

	let info = probe(&tools, &aac, &CancellationToken::new()).await.unwrap();

	assert!(info.format.duration_estimated);
}

#[tokio::test]
async fn failing_probe_keeps_stderr() {
	let dir = tempfile::tempdir().unwrap();
	let bogus = dir.path().join("not-media.mp3");
	std::fs::write(&bogus, b"plain text, not audio").unwrap();
	let tools = support::toolchain().await;

	let result = probe(&tools, &bogus, &CancellationToken::new()).await;

	assert!(matches!(result, Err(ProbeError::Failed(_))));
}
