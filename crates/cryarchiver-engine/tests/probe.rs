mod support;

use cryarchiver_engine::probe::{ProbeError, first_frames, probe, probe_video};
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

#[tokio::test]
async fn video_probe_counts_packets_and_reads_rotation_and_chapters() {
	let dir = tempfile::tempdir().unwrap();
	let plain = support::video(
		dir.path(),
		"plain.mp4",
		2,
		&["-map", "0:v", "-map", "1:a", "-c:v", "mpeg4", "-c:a", "aac"],
	);
	let rotated = dir.path().join("rotated.mp4");
	support::ffmpeg(&[
		"-display_rotation:v:0",
		"90",
		"-i",
		plain.to_str().unwrap(),
		"-c",
		"copy",
		rotated.to_str().unwrap(),
	]);
	let tools = support::toolchain().await;

	let (info, json) = probe_video(&tools, &rotated, &CancellationToken::new()).await.unwrap();

	let video = info.streams.iter().find(|stream| stream.is_real_video()).unwrap();
	assert_eq!(video.nb_read_packets, Some(50));
	assert_eq!(video.rotation(), 90);
	assert!((video.fps().unwrap() - 25.0).abs() < 0.01);
	assert!(String::from_utf8_lossy(&json).contains("\"nb_read_packets\""));
}

#[tokio::test]
async fn first_frames_report_bitstream_colour() {
	let dir = tempfile::tempdir().unwrap();
	let tagged = support::video(
		dir.path(),
		"tagged.mkv",
		1,
		&[
			"-map",
			"0:v",
			"-vf",
			"setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709:range=tv",
			"-c:v",
			"ffv1",
		],
	);
	let tools = support::toolchain().await;

	let frames = first_frames(&tools, &tagged, 0, 1, &CancellationToken::new())
		.await
		.unwrap();

	assert_eq!(frames.len(), 1);
	assert_eq!(frames[0].color_primaries.as_deref(), Some("bt709"));
	assert_eq!(frames[0].color_range.as_deref(), Some("tv"));
}
