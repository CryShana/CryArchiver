mod support;

use std::fmt::Write;
use std::fs;

use std::path::Path;

use cryarchiver_engine::budget::{Lanes, ThreadBudget, Width};
use cryarchiver_engine::calibration::tables;
use cryarchiver_engine::options::{SubtitlePick, TrimWindow};
use cryarchiver_engine::preset::{Overrides, Speed};
use cryarchiver_engine::probe::probe;
use cryarchiver_engine::quality::search::Target;
use cryarchiver_engine::quality::vmaf::{Score, pool};
use cryarchiver_engine::toolchain::Toolchain;
use cryarchiver_engine::video::analyze::{
	Analysis, DEFAULT_SEGMENTS, NoiseClass, ScanType, analyze, analyze_reporting,
};
use cryarchiver_engine::video::burn;
use cryarchiver_engine::video::encode::{Sample, encode_sample, encode_video, video_command};
use cryarchiver_engine::video::plan::{VideoCaps, VideoPlan, plan_video, precheck};
use cryarchiver_engine::video::quality::{
	Aim, Scorer, SearchStep, Searched, search, search_below, search_samples, subsample, target,
};
use cryarchiver_engine::video::verify::undecoded_lead;
use cryarchiver_engine::workspace::Workspace;
use tokio_util::sync::CancellationToken;

const CAPS: VideoCaps = VideoCaps { vp9_alpha: false };
/// Against a reference without the cues, the burned clip's p5 is about 79.
const SUBTITLES_ALIGNED_P5: f64 = 90.0;
/// testsrc2 scored against itself one frame later gives a VMAF mean of about 44.
const LEAD_ALIGNED_MEAN: f64 = 90.0;
/// Every clip the full check is tested on, aligned with its source, scores well above a one-frame shift's 44.
const ALIGNED_MEAN: f64 = 80.0;

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
			lanes: Lanes::SERIAL,
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
	let scorer = Scorer {
		tools: &tools,
		source: &clip,
		workspace: &workspace,
		plan: &plan,
		threads: 4,
		lanes: Lanes::SERIAL,
		cancel: &cancel,
	};

	let checked = scorer
		.verify(&temp, encoded.frames, encoded.decoded_s, |_| {})
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
		lanes: Lanes::SERIAL,
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

#[tokio::test]
async fn samples_searched_side_by_side_give_the_same_crf_and_scores_as_one_at_a_time() {
	let dir = tempfile::tempdir().unwrap();
	let tools = support::toolchain().await;
	let clip = support::video(
		dir.path(),
		"grainy.mkv",
		8,
		&[
			"-map",
			"0:v",
			"-vf",
			"noise=alls=10:allf=t",
			"-c:v",
			"libx264",
			"-crf",
			"14",
		],
	);
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
	let plan = plan_video(&info, main, &[], &analysis, Speed::Moderate, None);
	let workspace = Workspace::create(&dir.path().join("work"), &clip).unwrap();
	let goal = target(Speed::Moderate, false, &Overrides::default());
	let samples = [0.5, 2.5, 4.5, 6.5].map(|start_s| Sample { start_s, length_s: 1.0 });
	let searched = async |lanes: Lanes<'_>, aim: Aim| {
		let scorer = Scorer {
			tools: &tools,
			source: &clip,
			workspace: &workspace,
			plan: &plan,
			threads: 4,
			lanes,
			cancel: &cancel,
		};
		search(&scorer, &samples, 20.0, aim, tables().video.default_slope, |_| {})
			.await
			.unwrap()
	};
	let four_lanes = Lanes {
		width: Width::Fixed(8),
		..Lanes::SERIAL
	};

	let one_at_a_time = searched(Lanes::SERIAL, Aim::exact(goal)).await;
	let side_by_side = searched(four_lanes, Aim::exact(goal)).await;

	assert!(one_at_a_time.probes.len() >= 2, "{one_at_a_time:?}");
	assert_eq!(side_by_side.searched, one_at_a_time.searched);
	assert_eq!(side_by_side.probes, one_at_a_time.probes);
	assert_eq!(side_by_side.sample_bytes, one_at_a_time.sample_bytes);
	assert!(one_at_a_time.rate > 0.0, "{one_at_a_time:?}");
	assert!(
		side_by_side.rate.is_nan(),
		"times of samples run side by side: {side_by_side:?}"
	);

	let timed = searched(
		four_lanes,
		Aim {
			length_s: Some(8.0),
			..Aim::exact(goal)
		},
	)
	.await;
	assert_eq!(timed.probes, one_at_a_time.probes);
	assert!(
		timed.rate > 0.0,
		"the first probe times its samples one at a time: {timed:?}"
	);
}

#[tokio::test]
async fn segments_analysed_side_by_side_measure_the_same() {
	let dir = tempfile::tempdir().unwrap();
	let tools = support::toolchain().await;
	let clip = support::video(
		dir.path(),
		"grainy.mkv",
		16,
		&[
			"-map",
			"0:v",
			"-vf",
			"noise=alls=10:allf=t",
			"-c:v",
			"libx264",
			"-crf",
			"14",
		],
	);
	let cancel = CancellationToken::new();
	let info = probe(&tools, &clip, &cancel).await.unwrap();
	let main = precheck(&info, &CAPS).unwrap();
	let analysed = async |lanes: Lanes<'_>| {
		Box::pin(analyze_reporting(
			&tools,
			&clip,
			main,
			(info.format.duration, None),
			(6, lanes),
			&|_, _| {},
			&cancel,
		))
		.await
		.unwrap()
	};

	let serial = analysed(Lanes::SERIAL).await;
	let budget = ThreadBudget::new(8);
	let hold = budget.hold(2);
	let shared = analysed(Lanes {
		width: Width::Shared(&hold),
		..Lanes::SERIAL
	})
	.await;
	let one_thread = analysed(Lanes {
		width: Width::Fixed(1),
		..Lanes::SERIAL
	})
	.await;

	assert_eq!(serial.detail.len(), 6, "{serial:?}");
	assert_eq!(shared, serial);
	assert_eq!(one_thread, serial);
	assert_eq!(budget.free(), 6, "the lease ends with the analysis");
}

/// The full check of `clip` encoded by `plan`, the same frames picked from a decode-through measure that scores every
/// frame, and every how many frames the check scored one.
async fn both_checks(tools: &Toolchain, clip: &Path, workspace: &Workspace, plan: &VideoPlan) -> (Score, Score, u64) {
	let cancel = CancellationToken::new();
	let declared = probe(tools, clip, &cancel).await.unwrap().format.duration;
	let temp = workspace.file("video.a1.mkv");
	let cmd = video_command(&tools.ffmpeg, clip, &temp, plan);
	let duration_s = plan.window.map(|window| window.length_s).or(declared);
	let encoded = encode_video(&cmd, plan, duration_s, |_| {}, &cancel).await.unwrap();
	let scorer = Scorer {
		tools,
		source: clip,
		workspace,
		plan,
		threads: 4,
		lanes: Lanes::SERIAL,
		cancel: &cancel,
	};

	let mut progress = 0.0_f64;
	let checked = scorer
		.verify(&temp, encoded.frames, encoded.decoded_s, |fraction| {
			progress = progress.max(fraction);
		})
		.await
		.unwrap();
	let every = subsample(encoded.frames, encoded.decoded_s);
	let all = scorer.whole_scores(&temp, 1, encoded.frames, |_| {}).await.unwrap();

	assert_eq!(
		u64::try_from(all.len()).unwrap(),
		encoded.frames,
		"{}: every frame of the output is paired with one of the source",
		clip.display()
	);
	assert!(progress > 0.9, "{}: {progress}", clip.display());
	let picked: Vec<f64> = all.into_iter().step_by(usize::try_from(every).unwrap()).collect();
	(checked, pool(&picked).unwrap(), every)
}

fn same_frames(checked: Score, through: Score) -> bool {
	checked.frames == through.frames
		&& (checked.mean - through.mean).abs() < 0.01
		&& (checked.p5 - through.p5).abs() < 0.01
}

async fn plan_for(tools: &Toolchain, clip: &Path) -> VideoPlan {
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
	plan
}

#[tokio::test]
async fn the_sampled_check_of_a_24_fps_encode_scores_like_every_frame() {
	let dir = tempfile::tempdir().unwrap();
	let tools = support::toolchain().await;
	let clip = dir.path().join("film.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=640x360:r=24:d=40,noise=alls=8:allf=t",
		"-c:v",
		"libx264",
		"-crf",
		"16",
		clip.to_str().unwrap(),
	]);
	let cancel = CancellationToken::new();
	let workspace = Workspace::create(&dir.path().join("work"), &clip).unwrap();
	let mut plan = plan_for(&tools, &clip).await;
	plan.encoder = plan.encoder.without_grain().with_crf(38.0);
	let temp = workspace.file("video.a1.mkv");
	let cmd = video_command(&tools.ffmpeg, &clip, &temp, &plan);
	let encoded = encode_video(&cmd, &plan, Some(40.0), |_| {}, &cancel).await.unwrap();
	let scorer = Scorer {
		tools: &tools,
		source: &clip,
		workspace: &workspace,
		plan: &plan,
		threads: 4,
		lanes: Lanes::SERIAL,
		cancel: &cancel,
	};

	let sampled = scorer
		.verify(&temp, encoded.frames, encoded.decoded_s, |_| {})
		.await
		.unwrap();
	let all = pool(&scorer.whole_scores(&temp, 1, encoded.frames, |_| {}).await.unwrap()).unwrap();

	assert_eq!(subsample(encoded.frames, encoded.decoded_s), 3);
	assert!(
		(sampled.p5 - all.p5).abs() < 0.3 && (sampled.mean - all.mean).abs() < 0.2,
		"every third frame {sampled:?} against every frame {all:?}"
	);
}

#[tokio::test]
async fn the_whole_check_scores_the_same_frames_as_a_measure_of_every_frame() {
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
		support::mpeg4(dir.path(), "plain.mp4", 40),
		support::variable_rate(dir.path(), "variable.mkv", 30),
		interlaced,
		late,
	];

	let mut subsampled = 0;
	for clip in clips {
		let workspace = Workspace::create(&dir.path().join("work"), &clip).unwrap();
		let plan = plan_for(&tools, &clip).await;

		let (_, _, every) = checks_agree(&tools, &clip, &workspace, &plan).await;
		if every > 1 {
			subsampled += 1;
		}
	}
	assert!(subsampled >= 2, "the plain and the deinterlaced clips: {subsampled}");
}

/// The full check of `clip`, whole and trimmed, scores the same frames as a measure of every frame; returns the whole
/// check's and the trimmed check's scores, and every how many frames the whole check scored one.
async fn checks_agree(tools: &Toolchain, clip: &Path, workspace: &Workspace, plan: &VideoPlan) -> (Score, Score, u64) {
	let (whole, through, every) = both_checks(tools, clip, workspace, plan).await;

	assert!(same_frames(whole, through), "{}: {whole:?} {through:?}", clip.display());
	assert!(whole.mean > ALIGNED_MEAN, "{}: {whole:?}", clip.display());

	let trimmed = VideoPlan {
		window: Some(TrimWindow {
			start_s: 4.3,
			length_s: 14.0,
		}),
		expected_frames: None,
		..plan.clone()
	};
	let (cut, through, _) = both_checks(tools, clip, workspace, &trimmed).await;

	assert!(
		same_frames(cut, through),
		"{} trimmed: {cut:?} {through:?}",
		clip.display()
	);
	assert!(cut.mean > ALIGNED_MEAN, "{} trimmed: {cut:?}", clip.display());
	(whole, cut, every)
}

#[tokio::test]
async fn the_whole_check_of_a_source_whose_first_frames_never_decode_stays_aligned() {
	let dir = tempfile::tempdir().unwrap();
	let tools = support::toolchain().await;
	let lead = dir.path().join("lost-keyframe.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=320x240:r=25:d=40",
		"-c:v",
		"libx264",
		"-crf",
		"18",
		"-g",
		"25",
		"-bf",
		"2",
		"-bsf:v",
		r"noise=drop=lt(n\,1)",
		lead.to_str().unwrap(),
	]);
	let cancel = CancellationToken::new();
	assert_eq!(
		undecoded_lead(&tools, &lead, 0, &cancel).await.unwrap(),
		24,
		"the first GOP lost its keyframe"
	);
	let workspace = Workspace::create(&dir.path().join("work"), &lead).unwrap();
	let plan = plan_for(&tools, &lead).await;

	let (whole, cut, every) = checks_agree(&tools, &lead, &workspace, &plan).await;

	assert_eq!(every, 3);
	assert!(
		whole.mean > LEAD_ALIGNED_MEAN && cut.mean > LEAD_ALIGNED_MEAN,
		"the encode starts at the first decodable frame, as the reference does: {whole:?} {cut:?}"
	);
}

#[tokio::test]
async fn the_whole_check_draws_burned_subtitles_at_the_times_the_encode_shows_them() {
	let dir = tempfile::tempdir().unwrap();
	let tools = support::toolchain().await;
	let cancel = CancellationToken::new();
	let cues = dir.path().join("cues.srt");
	let clock = |ms: u32| format!("00:00:{:02},{:03}", ms / 1000, ms % 1000);
	let mut srt = String::new();
	for cue in 0..12 {
		let start = cue * 2500 + 700;
		writeln!(
			srt,
			"{}\n{} --> {}\nline {cue}\n",
			cue + 1,
			clock(start),
			clock(start + 1000)
		)
		.unwrap();
	}
	fs::write(&cues, srt).unwrap();
	let clip = support::subtitled_clip(
		dir.path(),
		"subtitled.mkv",
		&support::Picture {
			source: "testsrc2",
			seconds: 40,
			codec: &["-c:v", "mpeg4", "-q:v", "2"],
			second_angle: false,
			marked_frame: None,
		},
		&cues,
		"srt",
	);
	let workspace = Workspace::create(&dir.path().join("work"), &clip).unwrap();
	let plan = plan_for(&tools, &clip).await;
	let info = probe(&tools, &clip, &cancel).await.unwrap();
	let chosen = burn::pick(&info, &SubtitlePick::Default).unwrap();
	burn::prepare(&tools, &clip, &workspace, &info, chosen, &cancel)
		.await
		.unwrap();
	let burning = VideoPlan {
		subtitles: burn::subtitle_filter(chosen.kind, 20),
		..plan
	};

	let (whole, cut, every) = checks_agree(&tools, &clip, &workspace, &burning).await;

	assert_eq!(every, 3);
	assert!(
		whole.p5 > SUBTITLES_ALIGNED_P5 && cut.p5 > SUBTITLES_ALIGNED_P5,
		"the reference draws each cue when the encode shows it: {whole:?} {cut:?}"
	);
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
		lanes: Lanes::SERIAL,
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
		lanes: Lanes::SERIAL,
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
