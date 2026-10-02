mod support;

use std::fs;

use std::path::Path;

use cryarchiver_engine::calibration::tables;
use cryarchiver_engine::options::{SubtitlePick, TrimWindow};
use cryarchiver_engine::preset::{Overrides, Speed};
use cryarchiver_engine::probe::probe;
use cryarchiver_engine::quality::search::Target;
use cryarchiver_engine::quality::vmaf::{Reference, Score, Scoring, frames, model_for, pool, window_command};
use cryarchiver_engine::toolchain::Toolchain;
use cryarchiver_engine::video::analyze::{Analysis, DEFAULT_SEGMENTS, NoiseClass, ScanType, analyze};
use cryarchiver_engine::video::burn;
use cryarchiver_engine::video::encode::{Sample, encode_sample, encode_video, video_command};
use cryarchiver_engine::video::plan::{VideoCaps, VideoPlan, plan_video, precheck};
use cryarchiver_engine::video::quality::{
	Aim, Scorer, SearchStep, Searched, search, search_below, search_samples, target, verify_windows,
};
use cryarchiver_engine::video::verify::Retiming;
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
		search(
			&scorer,
			&samples,
			plan.encoder.crf(),
			Aim::exact(goal),
			tables().video.default_slope,
			|_| {},
		)
		.await
		.unwrap()
	};

	let Searched::Found(found) = searched.searched else {
		panic!("{searched:?}")
	};
	assert!(goal.met(found.score), "{found:?}");
	assert!(found.sample_bytes > 0);
	assert!(searched.rate.is_finite() && searched.rate > 0.0, "{searched:?}");
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
	let windows = verify_windows(encoded.frames, encoded.decoded_s, Speed::Moderate, &[]);
	let scorer = Scorer {
		tools: &tools,
		source: &clip,
		workspace: &workspace,
		plan: &plan,
		threads: 4,
		cancel: &cancel,
	};

	let checked = scorer
		.verify(&temp, &windows, encoded.frames, Retiming::Kept, |_| {})
		.await
		.unwrap();

	assert_eq!(checked.frames, 75, "{checked:?}");
	assert!(goal.met(checked), "{checked:?} at CRF {}", found.crf);
}

#[tokio::test]
async fn a_long_encode_aims_the_search_above_the_target_from_its_first_probe() {
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
	let plan = plan_video(&info, main, &[], &analysis, Speed::Moderate, None);
	let workspace = Workspace::create(&dir.path().join("work"), &clip).unwrap();
	let goal = target(Speed::Moderate, false, &Overrides::default());
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
	let searched = |aim: Aim| {
		search(
			&scorer,
			&samples,
			plan.encoder.crf(),
			aim,
			tables().video.default_slope,
			|_| {},
		)
	};

	let exact = searched(Aim::exact(goal)).await.unwrap();
	let days = searched(Aim {
		length_s: Some(1e7),
		..Aim::exact(goal)
	})
	.await
	.unwrap();

	let (Searched::Found(exact), Searched::Found(margined)) = (exact.searched, days.searched) else {
		panic!("{exact:?} {days:?}")
	};
	assert!(
		margined.score.mean >= goal.mean + 1.0 && margined.score.p5 >= goal.floor + 1.5,
		"{margined:?}"
	);
	assert!(margined.crf < exact.crf, "{margined:?} {exact:?}");
}

/// The full check of `clip` encoded by `plan`, through the seeking path and through the old one that decodes both files
/// to the windows.
async fn both_checks(
	tools: &Toolchain,
	clip: &Path,
	workspace: &Workspace,
	plan: &VideoPlan,
	retiming: Retiming,
	hardest_s: &[f64],
) -> (Score, Score) {
	let cancel = CancellationToken::new();
	let declared = probe(tools, clip, &cancel).await.unwrap().format.duration;
	let temp = workspace.file("video.a1.mkv");
	let cmd = video_command(&tools.ffmpeg, clip, &temp, plan);
	let duration_s = plan.window.map(|window| window.length_s).or(declared);
	let encoded = encode_video(&cmd, plan, duration_s, |_| {}, &cancel).await.unwrap();
	let windows = verify_windows(encoded.frames, encoded.decoded_s, Speed::Moderate, hardest_s);
	let scorer = Scorer {
		tools,
		source: clip,
		workspace,
		plan,
		threads: 4,
		cancel: &cancel,
	};

	let seeked = scorer
		.verify_by_seeking(&temp, &windows, encoded.frames, retiming)
		.await
		.unwrap()
		.expect("the check seeks to its windows");

	let reference = Reference {
		source: clip,
		stream: plan.streams[0],
		decoder: plan.decoder,
		shape: &plan.shape,
		filters: &plan.filters,
		window: plan.window,
		start: plan.starts.main,
		overlay: plan.overlay,
		subtitles: plan.subtitles.as_deref(),
	};
	let scoring = Scoring {
		model: model_for(plan.width, plan.height),
		threads: 4,
		dir: workspace.dir(),
		log: "vmaf-through.json",
	};
	let through_cmd = window_command(&tools.ffmpeg, &temp, &reference, &windows, &scoring);
	let through = pool(&frames(&through_cmd, &scoring, |_| {}, &cancel).await.unwrap()).unwrap();
	(seeked, through)
}

fn same_frames(seeked: Score, through: Score) -> bool {
	seeked.frames == through.frames && (seeked.mean - through.mean).abs() < 0.3 && (seeked.p5 - through.p5).abs() < 1.0
}

async fn plan_for(tools: &Toolchain, clip: &Path) -> (VideoPlan, Retiming) {
	let cancel = CancellationToken::new();
	let info = probe(tools, clip, &cancel).await.unwrap();
	let main = precheck(&info, &CAPS).unwrap();
	let analysis = Box::pin(analyze(
		tools,
		clip,
		main,
		info.format.duration,
		DEFAULT_SEGMENTS,
		None,
		&cancel,
	))
	.await
	.unwrap();
	let mut plan = plan_video(&info, main, &[], &analysis, Speed::Moderate, None);
	plan.encoder = plan.encoder.with_crf(40.0);
	(plan, Retiming::of(analysis.scan))
}

#[tokio::test]
async fn a_seeked_check_scores_the_same_frames_as_one_decoded_through_to_its_windows() {
	let dir = tempfile::tempdir().unwrap();
	let tools = support::toolchain().await;
	let interlaced = dir.path().join("tff.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=720x576:r=50:d=20,interlace=scan=tff,setfield=tff",
		"-c:v",
		"mpeg2video",
		"-b:v",
		"12M",
		"-flags",
		"+ilme+ildct",
		interlaced.to_str().unwrap(),
	]);
	let late = dir.path().join("late-video.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"sine=frequency=440:duration=22",
		"-itsoffset",
		"1.5",
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=320x240:r=25:d=20",
		"-map",
		"1:v",
		"-map",
		"0:a",
		"-c:v",
		"mpeg4",
		"-q:v",
		"2",
		"-c:a",
		"flac",
		late.to_str().unwrap(),
	]);
	let clips = [
		support::mpeg4(dir.path(), "plain.mp4", 30),
		support::variable_rate(dir.path(), "variable.mkv", 30),
		interlaced,
		late,
	];

	for (index, clip) in clips.into_iter().enumerate() {
		let workspace = Workspace::create(&dir.path().join(format!("work{index}")), &clip).unwrap();
		let (plan, retiming) = plan_for(&tools, &clip).await;
		let (seeked, through) = both_checks(&tools, &clip, &workspace, &plan, retiming, &[7.3, 21.9]).await;

		assert!(same_frames(seeked, through), "{clip:?}: {seeked:?} {through:?}");

		let trimmed = VideoPlan {
			window: Some(TrimWindow {
				start_s: 4.3,
				length_s: 14.0,
			}),
			expected_frames: None,
			..plan
		};
		let (seeked, through) = both_checks(&tools, &clip, &workspace, &trimmed, retiming, &[6.1]).await;

		assert!(same_frames(seeked, through), "{clip:?} trimmed: {seeked:?} {through:?}");
	}
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
	let searched = search_below(
		&scorer,
		&samples,
		40.0,
		Aim::exact(unreachable),
		tables().video.default_slope,
		Some(1),
		|step| {
			if matches!(step, SearchStep::Probe { .. }) {
				rounds += 1;
			}
		},
	)
	.await
	.unwrap()
	.searched;

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
	let probed = scorer.probe(20.0, &[on_screen], 0, |_| {}).await.unwrap();
	let score = probed.score;
	assert!(probed.encode_s > 0.0 && probed.bytes > 0, "{probed:?}");

	let lit = support::lit_span(&during, 0).unwrap();
	assert!(lit.0.abs() < 0.02 && (lit.1 - 0.96).abs() < 0.02, "{lit:?}");
	assert_eq!(support::lit_span(&before, 0), None);
	assert!(
		score.p5 > 90.0,
		"a reference without the sample's cue scores about 80: {score:?}"
	);
}
