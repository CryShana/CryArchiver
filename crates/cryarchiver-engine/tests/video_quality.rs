mod support;

use std::fs;

use cryarchiver_engine::preset::{Overrides, Speed};
use cryarchiver_engine::probe::probe;
use cryarchiver_engine::quality::search::DEFAULT_SLOPE;
use cryarchiver_engine::video::analyze::{DEFAULT_SEGMENTS, analyze};
use cryarchiver_engine::video::encode::{encode_video, video_command};
use cryarchiver_engine::video::plan::{VideoCaps, plan_video, precheck};
use cryarchiver_engine::video::quality::{Scorer, Searched, search, search_samples, target, verify_windows};
use cryarchiver_engine::workspace::Workspace;
use tokio_util::sync::CancellationToken;

const CAPS: VideoCaps = VideoCaps {
	vp9_alpha: false,
	libaom: false,
};

#[tokio::test]
async fn a_search_finds_a_passing_crf_and_the_check_confirms_the_full_encode() {
	let dir = tempfile::tempdir().unwrap();
	let tools = support::toolchain().await;
	let clip = support::mpeg4(dir.path(), "clip.mp4", 3);
	let cancel = CancellationToken::new();
	let info = probe(&tools, &clip, &cancel).await.unwrap();
	let main = precheck(&info, &CAPS).unwrap();
	let analysis = Box::pin(analyze(
		&tools,
		&clip,
		main,
		info.format.duration,
		DEFAULT_SEGMENTS,
		&cancel,
	))
	.await
	.unwrap();
	let mut plan = plan_video(&info, main, &[], &analysis, Speed::Moderate);
	let workspace = Workspace::create(&dir.path().join("work"), &clip).unwrap();
	let goal = target(Speed::Moderate, plan.encoder.film_grain(), &Overrides::default());
	let samples = search_samples(&analysis, info.format.duration, Speed::Moderate);

	let searched = {
		let scorer = Scorer {
			tools: &tools,
			source: &clip,
			workspace: &workspace,
			plan: &plan,
			threads: 4,
			cancel: &cancel,
		};
		search(&scorer, &samples, plan.encoder.crf(), goal, DEFAULT_SLOPE, |_| {})
			.await
			.unwrap()
	};

	let Searched::Found(found) = searched else {
		panic!("{searched:?}")
	};
	assert!(goal.met(found.score), "{found:?}");
	assert!(found.sample_bytes > 0);
	assert!(
		fs::read_dir(workspace.dir()).unwrap().all(|entry| !entry
			.unwrap()
			.file_name()
			.to_string_lossy()
			.starts_with("probe")),
		"probe samples are deleted once scored"
	);

	plan.encoder = plan.encoder.with_crf(found.crf);
	let temp = workspace.file("video.a1.mkv");
	let cmd = video_command(&tools.ffmpeg, &clip, &temp, &plan);
	let encoded = encode_video(&cmd, &plan, info.format.duration, |_| {}, &cancel)
		.await
		.unwrap();
	let windows = verify_windows(encoded.frames, encoded.decoded_s, Speed::Moderate);
	let scorer = Scorer {
		tools: &tools,
		source: &clip,
		workspace: &workspace,
		plan: &plan,
		threads: 4,
		cancel: &cancel,
	};

	let checked = scorer.verify(&temp, &windows, encoded.frames, |_| {}).await.unwrap();

	assert_eq!(checked.frames, 75, "{checked:?}");
	assert!(goal.met(checked), "{checked:?} at CRF {}", found.crf);
}
