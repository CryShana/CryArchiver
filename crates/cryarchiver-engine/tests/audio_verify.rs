mod support;

use cryarchiver_engine::audio::plan::{EncodePlan, FileOptions, FilePlan, LayoutFix, TrackJob, TrackPlan};
use cryarchiver_engine::audio::verify::{Levels, compare, measure_output};
use cryarchiver_engine::container::OGG_OPUS;
use cryarchiver_engine::outcome::ErrorKind;
use tokio_util::sync::CancellationToken;

const TONE_RMS_DB: f64 = -13.47;

fn opus_plan() -> FilePlan {
	let plan = EncodePlan {
		bitrate_kbps: 128,
		channels: 2,
		layout: LayoutFix::Keep("stereo"),
	};
	FilePlan {
		container: OGG_OPUS,
		tracks: vec![TrackJob {
			stream_index: 0,
			sample_rate: 48_000,
			plan: TrackPlan::Encode(plan),
		}],
		options: FileOptions::default(),
	}
}

fn tone_levels(seconds: f64) -> Levels {
	Levels {
		duration_s: seconds,
		rms_db: vec![TONE_RMS_DB, TONE_RMS_DB],
	}
}

#[tokio::test]
async fn measures_each_output_track() {
	let dir = tempfile::tempdir().unwrap();
	let opus = support::tone(
		dir.path(),
		"a.opus",
		"stereo",
		2,
		3,
		&["-c:a", "libopus", "-b:a", "128k"],
	);
	let tools = support::toolchain().await;

	let levels = measure_output(&tools, &opus, &opus_plan(), &CancellationToken::new())
		.await
		.unwrap();

	assert_eq!(levels.len(), 1);
	assert_eq!(compare(&tone_levels(3.0), &levels[0], true, None), Ok(()));
}

#[tokio::test]
async fn detects_a_muted_channel_and_a_short_output() {
	let dir = tempfile::tempdir().unwrap();
	let muted = support::tone(
		dir.path(),
		"muted.opus",
		"stereo",
		2,
		3,
		&["-af", "pan=stereo|c0=c0|c1=0*c1", "-c:a", "libopus"],
	);
	let short = support::tone(
		dir.path(),
		"short.opus",
		"stereo",
		2,
		3,
		&["-t", "2.5", "-c:a", "libopus"],
	);
	let tools = support::toolchain().await;
	let cancel = CancellationToken::new();

	let muted = measure_output(&tools, &muted, &opus_plan(), &cancel).await.unwrap();
	let short = measure_output(&tools, &short, &opus_plan(), &cancel).await.unwrap();

	assert!(
		compare(&tone_levels(3.0), &muted[0], true, None)
			.unwrap_err()
			.contains("channel 2")
	);
	assert!(
		compare(&tone_levels(3.0), &short[0], true, None)
			.unwrap_err()
			.contains("duration")
	);
}

#[tokio::test]
async fn an_undecodable_output_is_an_encode_failure() {
	let dir = tempfile::tempdir().unwrap();
	let broken = dir.path().join("broken.opus");
	std::fs::write(&broken, b"OggS but not really").unwrap();
	let tools = support::toolchain().await;

	let err = measure_output(&tools, &broken, &opus_plan(), &CancellationToken::new())
		.await
		.unwrap_err();

	assert!(matches!(err.kind, ErrorKind::EncodeFailed(_)));
}
