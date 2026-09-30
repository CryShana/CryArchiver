mod support;

use std::fs;

use std::path::Path;

use cryarchiver_engine::options::{SubtitlePick, TrimWindow};
use cryarchiver_engine::preset::{Overrides, Speed};
use cryarchiver_engine::probe::probe;
use cryarchiver_engine::quality::search::{DEFAULT_SLOPE, Target};
use cryarchiver_engine::toolchain::Toolchain;
use cryarchiver_engine::video::analyze::{Analysis, DEFAULT_SEGMENTS, NoiseClass, ScanType, analyze};
use cryarchiver_engine::video::burn;
use cryarchiver_engine::video::encode::{Sample, encode_sample, encode_video, video_command};
use cryarchiver_engine::video::plan::{VideoCaps, VideoPlan, plan_video, precheck};
use cryarchiver_engine::video::quality::{
	Scorer, Searched, search, search_below, search_samples, target, verify_windows,
};
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
		None,
		&cancel,
	))
	.await
	.unwrap();
	let mut plan = plan_video(&info, main, &[], &analysis, Speed::Moderate, None);
	let workspace = Workspace::create(&dir.path().join("work"), &clip).unwrap();
	let goal = target(Speed::Moderate, plan.encoder.film_grain(), &Overrides::default());
	let span = info
		.format
		.duration
		.map(|length_s| TrimWindow { start_s: 0.0, length_s });
	let samples = search_samples(&analysis, span, Speed::Moderate);

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

#[tokio::test]
async fn a_search_below_a_size_gives_up_once_a_miss_is_already_that_large() {
	let dir = tempfile::tempdir().unwrap();
	let tools = support::toolchain().await;
	let clip = support::mpeg4(dir.path(), "clip.mp4", 3);
	let cancel = CancellationToken::new();
	let info = probe(&tools, &clip, &cancel).await.unwrap();
	let main = precheck(&info, &CAPS).unwrap();
	let analysis = Analysis {
		scan: ScanType::Progressive,
		noise: NoiseClass::Clean,
		noise_sigma: None,
		detail: Vec::new(),
	};
	let plan = plan_video(&info, main, &[], &analysis, Speed::Fast, None);
	let workspace = Workspace::create(&dir.path().join("work"), &clip).unwrap();
	let unreachable = Target {
		mean: 99.9,
		floor: 99.9,
	};
	let samples = [Sample {
		start_s: 0.0,
		length_s: 3.0,
	}];
	let scorer = Scorer {
		tools: &tools,
		source: &clip,
		workspace: &workspace,
		plan: &plan,
		threads: 4,
		cancel: &cancel,
	};

	let mut rounds = 0;
	let searched = search_below(&scorer, &samples, 40.0, unreachable, DEFAULT_SLOPE, Some(1), |_| {
		rounds += 1;
	})
	.await
	.unwrap();

	assert!(
		matches!(searched, Searched::Unreachable { crf, .. } if (crf - 40.0).abs() < 1e-9),
		"{searched:?}"
	);
	assert_eq!(rounds, 1);
}

async fn burning_plan(
	tools: &Toolchain,
	source: &Path,
	workspace: &Workspace,
	cancel: &CancellationToken,
) -> VideoPlan {
	let info = probe(tools, source, cancel).await.unwrap();
	let main = precheck(&info, &CAPS).unwrap();
	let analysis = Analysis {
		scan: ScanType::Progressive,
		noise: NoiseClass::Clean,
		noise_sigma: None,
		detail: Vec::new(),
	};
	let chosen = burn::pick(&info, &SubtitlePick::Default).unwrap();
	burn::prepare(tools, source, workspace, &info, chosen, cancel)
		.await
		.unwrap();

	VideoPlan {
		subtitles: burn::subtitle_filter(chosen.kind, 20),
		..plan_video(&info, main, &[], &analysis, Speed::Moderate, None)
	}
}

#[tokio::test]
async fn a_search_sample_and_its_reference_draw_the_subtitles_on_screen_at_its_own_time() {
	let dir = tempfile::tempdir().unwrap();
	let tools = support::toolchain().await;
	let cancel = CancellationToken::new();
	let cues = dir.path().join("late.srt");
	fs::write(&cues, "1\n00:00:05,000 --> 00:00:06,000\nfive\n").unwrap();
	let black = support::black_clip(dir.path(), "black.mkv", &cues, "srt", false);
	let colour = support::clip_with_extras(dir.path());
	let on_screen = Sample {
		start_s: 5.0,
		length_s: 1.0,
	};
	let off_screen = Sample {
		start_s: 2.5,
		length_s: 1.0,
	};

	let black_work = Workspace::create(&dir.path().join("work"), &black).unwrap();
	let black_plan = burning_plan(&tools, &black, &black_work, &cancel).await;
	let during = black_work.file("during.mkv");
	let before = black_work.file("before.mkv");
	for (sample, output) in [(&on_screen, &during), (&off_screen, &before)] {
		encode_sample(&tools.ffmpeg, &black, output, &black_plan, sample, &cancel)
			.await
			.unwrap();
	}

	let colour_work = Workspace::create(&dir.path().join("work"), &colour).unwrap();
	let colour_plan = burning_plan(&tools, &colour, &colour_work, &cancel).await;
	let scorer = Scorer {
		tools: &tools,
		source: &colour,
		workspace: &colour_work,
		plan: &colour_plan,
		threads: 4,
		cancel: &cancel,
	};
	let (score, _) = scorer.probe(20.0, &[on_screen], 0).await.unwrap();

	let lit = support::lit_span(&during, 0).unwrap();
	assert!(lit.0.abs() < 0.02 && (lit.1 - 0.96).abs() < 0.02, "{lit:?}");
	assert_eq!(support::lit_span(&before, 0), None);
	assert!(
		score.p5 > 90.0,
		"a reference without the sample's cue scores about 80: {score:?}"
	);
}
