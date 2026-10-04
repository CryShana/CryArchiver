mod support;

use std::path::Path;

use cryarchiver_engine::audio::encode;
use cryarchiver_engine::audio::plan::{AudioPlan, merge_file, plan_file};
use cryarchiver_engine::audio::verify::Levels;
use cryarchiver_engine::preset::{Overrides, Speed};
use cryarchiver_engine::probe::{MediaInfo, probe};
use tokio_util::sync::CancellationToken;

async fn encode_file(source: &Path, output: &Path) -> (Vec<Levels>, MediaInfo) {
	let tools = support::toolchain().await;
	let cancel = CancellationToken::new();
	let info = probe(&tools, source, &cancel).await.unwrap();
	let AudioPlan::Encode(plan) = plan_file(&info, Speed::Moderate, &Overrides::default()) else {
		panic!("expected an encode plan")
	};

	let cmd = encode::command(&tools.ffmpeg, source, output, &plan);
	let levels = encode::run(&cmd, &plan, info.format.duration, |_| {}, &cancel)
		.await
		.unwrap();
	(levels, probe(&tools, output, &cancel).await.unwrap())
}

async fn encode_merged(source: &Path, output: &Path) -> (Vec<Levels>, MediaInfo) {
	let tools = support::toolchain().await;
	let cancel = CancellationToken::new();
	let info = probe(&tools, source, &cancel).await.unwrap();
	let AudioPlan::Encode(plan) = plan_file(&info, Speed::Moderate, &Overrides::default()) else {
		panic!("expected an encode plan")
	};
	let plan = merge_file(*plan, &info, Speed::Moderate, &Overrides::default());

	let cmd = encode::command(&tools.ffmpeg, source, output, &plan);
	let levels = encode::run(&cmd, &plan, info.format.duration, |_| {}, &cancel)
		.await
		.unwrap();
	(levels, probe(&tools, output, &cancel).await.unwrap())
}

fn volume_stat(output: &Path, extra_args: &[&str], stat: &str) -> f64 {
	let mut args = vec!["-hide_banner"];
	args.extend_from_slice(extra_args);
	args.push("-i");
	let scan = std::process::Command::new("ffmpeg")
		.args(args)
		.arg(output)
		.args(["-af", "volumedetect", "-f", "null", "-"])
		.output()
		.unwrap();
	let log = String::from_utf8_lossy(&scan.stderr).into_owned();
	log.split(stat)
		.nth(1)
		.and_then(|rest| rest.split_whitespace().next()?.parse().ok())
		.unwrap_or_else(|| panic!("{stat} not found: {log}"))
}

#[tokio::test]
async fn flac_becomes_48k_stereo_opus_and_reports_source_levels() {
	let dir = tempfile::tempdir().unwrap();
	let flac = support::tone(dir.path(), "a.flac", "stereo", 2, 3, &["-ar", "44100", "-c:a", "flac"]);

	let (levels, output) = encode_file(&flac, &dir.path().join("out.opus")).await;

	assert!((levels[0].duration_s - 3.0).abs() < 0.01, "{levels:?}");
	assert_eq!(levels[0].rms_db.len(), 2);
	let audio = output.audio_streams().next().unwrap();
	assert_eq!(
		(audio.codec_name.as_str(), audio.channels, audio.sample_rate),
		("opus", Some(2), Some(48_000))
	);
}

#[tokio::test]
async fn side_surround_is_relabelled_to_standard_5_1() {
	let dir = tempfile::tempdir().unwrap();
	let ac3 = support::tone(dir.path(), "a.ac3", "5.1(side)", 6, 3, &["-c:a", "ac3", "-b:a", "448k"]);

	let (_, output) = encode_file(&ac3, &dir.path().join("out.opus")).await;

	let audio = output.audio_streams().next().unwrap();
	assert_eq!(audio.channels, Some(6));
	assert_eq!(audio.channel_layout.as_deref(), Some("5.1"));
}

#[tokio::test]
async fn two_tracks_go_to_matroska_with_levels_for_each() {
	let dir = tempfile::tempdir().unwrap();
	let mka = dir.path().join("two.mka");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"sine=f=440:d=3",
		"-f",
		"lavfi",
		"-i",
		"sine=f=660:d=3",
		"-map",
		"0:a",
		"-map",
		"1:a",
		"-c:a",
		"flac",
		mka.to_str().unwrap(),
	]);

	let (levels, output) = encode_file(&mka, &dir.path().join("out.mka")).await;

	assert_eq!(levels.len(), 2);
	assert_eq!(output.audio_streams().count(), 2);
	assert!(output.format.format_name.contains("matroska"));
}

#[tokio::test]
async fn merging_loud_sources_leaves_headroom_and_no_leading_silence() {
	let dir = tempfile::tempdir().unwrap();
	let mka = dir.path().join("loud.mka");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"aevalsrc=0.95*sin(2*PI*300*t):s=48000:d=2",
		"-f",
		"lavfi",
		"-i",
		"aevalsrc=0.95*sin(2*PI*900*t):s=48000:d=2",
		"-map",
		"0:a",
		"-map",
		"1:a",
		"-c:a",
		"flac",
		mka.to_str().unwrap(),
	]);
	let output = dir.path().join("out.opus");

	let (levels, info) = encode_merged(&mka, &output).await;

	assert_eq!(levels.len(), 1);
	assert!((levels[0].duration_s - 2.0).abs() < 0.005, "{levels:?}");
	assert_eq!(info.audio_streams().next().unwrap().channels, Some(1));

	let peak = volume_stat(&output, &[], "max_volume:");
	assert!(
		peak <= -0.1,
		"the mix should keep headroom under the limiter instead of hitting full scale: {peak} dB"
	);

	let onset = volume_stat(&output, &["-t", "0.003"], "max_volume:");
	assert!(
		onset > -20.0,
		"the output should not start with leading silence from the limiter's look-ahead delay: {onset} dB"
	);
}
