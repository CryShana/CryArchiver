mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use cryarchiver_engine::container::isobmff::parse_meta;
use cryarchiver_engine::engine::{Engine, Job, JobEvent, Stage};
use cryarchiver_engine::format::OutputFormat;
use cryarchiver_engine::images::animated;
use cryarchiver_engine::options::{BurnSubs, JobOptions, Resize, SubtitlePick, Trim};
use cryarchiver_engine::outcome::JobOutcome;
use cryarchiver_engine::preset::{Overrides, Speed};
use cryarchiver_engine::probe::{Stream, probe};
use cryarchiver_engine::sink::CliSink;
use tokio_util::sync::CancellationToken;

const AUDIO_DROPPED: &str = "an audio track was dropped: animated AVIF has no sound";
const SUBTITLES_DROPPED: &str = "a subtitle track was dropped";

fn as_avif(source: &Path, options: JobOptions) -> Job {
	Job {
		source: source.to_path_buf(),
		speed: Speed::Fast,
		keep_metadata: false,
		drop_aux: false,
		min_savings: 0.10,
		overrides: Overrides::default(),
		options: JobOptions {
			format: Some(OutputFormat::Avif),
			..options
		},
	}
}

async fn run(job: &Job, out_dir: &Path) -> (JobOutcome, Vec<JobEvent>) {
	fs::create_dir_all(out_dir).unwrap();
	let engine = Engine::new(support::toolchain().await).with_image_threads(4);
	let sink = CliSink {
		output_dir: Some(out_dir.to_path_buf()),
		output_file: None,
		overwrite: false,
	};
	let events = Mutex::new(Vec::new());

	let outcome = engine
		.run(
			job,
			&sink,
			&|event| events.lock().unwrap().push(event),
			&CancellationToken::new(),
		)
		.await;
	(outcome, events.into_inner().unwrap())
}

fn written(outcome: &JobOutcome) -> (PathBuf, Vec<String>) {
	match outcome {
		JobOutcome::Encoded { output, warnings, .. } => (output.clone(), warnings.clone()),
		other => panic!("{other:?}"),
	}
}

fn failure(outcome: &JobOutcome) -> String {
	match outcome {
		JobOutcome::Failed(err) => err.to_string(),
		other => panic!("{other:?}"),
	}
}

/// The animated tracks: the colour sequence, and the alpha sequence after it, if any.
async fn sequences(avif: &Path) -> Vec<Stream> {
	probe(&support::toolchain().await, avif, &CancellationToken::new())
		.await
		.unwrap()
		.streams
		.into_iter()
		.filter(|stream| stream.nb_frames.is_some_and(|frames| frames > 1))
		.collect()
}

/// Each frame's duration in whole milliseconds.
fn delays_ms(avif: &Path, track: u32) -> Vec<f64> {
	let out = Command::new("ffprobe")
		.args(["-v", "error", "-select_streams"])
		.arg(track.to_string())
		.args(["-show_entries", "packet=duration_time", "-of", "csv=p=0"])
		.arg(avif)
		.output()
		.unwrap();
	String::from_utf8_lossy(&out.stdout)
		.lines()
		.filter_map(|line| line.trim().parse::<f64>().ok())
		.map(|seconds| (seconds * 1000.0).round())
		.collect()
}

/// The first frame of `track` as 8-bit samples of `pix_fmt`, and its width.
fn first_frame(avif: &Path, track: u32, pix_fmt: &str) -> (Vec<u8>, usize) {
	let out = Command::new("ffmpeg")
		.args(["-v", "error", "-i"])
		.arg(avif)
		.arg("-map")
		.arg(format!("0:{track}"))
		.args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", pix_fmt, "-"])
		.output()
		.unwrap();
	assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
	let width = Command::new("ffprobe")
		.args(["-v", "error", "-select_streams"])
		.arg(track.to_string())
		.args(["-show_entries", "stream=width", "-of", "csv=p=0"])
		.arg(avif)
		.output()
		.unwrap();
	(
		out.stdout,
		String::from_utf8_lossy(&width.stdout).trim().parse().unwrap(),
	)
}

fn clip(dir: &Path, name: &str, video: &str, extra: &[&str]) -> PathBuf {
	let path = dir.join(name);
	let mut args = vec!["-f", "lavfi", "-i", video];
	args.extend_from_slice(extra);
	args.push(path.to_str().unwrap());
	support::ffmpeg(&args);
	path
}

#[tokio::test]
async fn a_60p_video_becomes_an_animated_avif_with_every_frame_at_its_rate() {
	let dir = tempfile::tempdir().unwrap();
	let source = clip(
		dir.path(),
		"sixty.mp4",
		"testsrc2=s=320x240:r=60:d=1",
		&[
			"-f", "lavfi", "-i", "sine=d=1", "-c:v", "mpeg4", "-q:v", "2", "-c:a", "aac",
		],
	);

	let (outcome, _) = run(&as_avif(&source, JobOptions::default()), dir.path()).await;

	let (output, warnings) = written(&outcome);
	assert_eq!(output, dir.path().join("sixty.cry.avif"));
	let tracks = sequences(&output).await;
	assert_eq!(tracks.len(), 1, "{tracks:?}");
	assert_eq!(tracks[0].nb_frames, Some(60));
	assert_eq!((tracks[0].width, tracks[0].height), (Some(320), Some(240)));
	let delays = delays_ms(&output, tracks[0].index);
	assert_eq!(delays.len(), 60);
	assert!(delays.iter().all(|delay| [16.0, 17.0].contains(delay)), "{delays:?}");
	assert!((tracks[0].fps().unwrap() - 60.0).abs() < 0.5, "{:?}", tracks[0]);
	assert_eq!(warnings, [AUDIO_DROPPED], "{warnings:?}");
	assert_eq!(
		animated::plays(&fs::read(&output).unwrap()),
		Some(None),
		"loops forever"
	);
}

#[tokio::test]
async fn an_animation_without_a_thread_budget_holds_the_whole_pool_while_it_encodes() {
	let dir = tempfile::tempdir().unwrap();
	let source = clip(
		dir.path(),
		"clip.mp4",
		"testsrc2=s=320x240:r=10:d=1",
		&["-c:v", "mpeg4", "-q:v", "2"],
	);
	let engine = Engine::new(support::toolchain().await)
		.with_video_jobs(2)
		.with_thread_budget(8);
	let sink = CliSink {
		output_dir: Some(dir.path().to_path_buf()),
		output_file: None,
		overwrite: false,
	};
	let held = Mutex::new(Vec::new());

	let outcome = engine
		.run(
			&as_avif(&source, JobOptions::default()),
			&sink,
			&|event| {
				if let JobEvent::Stage(stage @ (Stage::Encode | Stage::Verify)) = event {
					held.lock().unwrap().push((stage, engine.thread_budget().in_use()));
				}
			},
			&CancellationToken::new(),
		)
		.await;

	written(&outcome);
	let held = held.into_inner().unwrap();
	assert!(held.iter().any(|(stage, _)| *stage == Stage::Encode), "{held:?}");
	assert!(
		held.iter().all(|(_, in_use)| *in_use == 8),
		"its share of 4 raised to the whole 8: {held:?}"
	);
	assert_eq!(engine.thread_budget().in_use(), 0, "all of it given back after the job");
}

#[tokio::test]
async fn audio_and_subtitles_are_dropped_with_one_folded_warning_each() {
	let dir = tempfile::tempdir().unwrap();
	let cues = dir.path().join("cues.srt");
	fs::write(&cues, "1\n00:00:00,500 --> 00:00:01,000\nhello\n").unwrap();
	let one = support::black_clip_of(dir.path(), "one.mkv", 2, &cues, "srt");
	let source = support::converted(
		dir.path(),
		"two.mkv",
		&[],
		&one,
		&["-map", "0", "-map", "0:a", "-c", "copy"],
	);

	let (outcome, events) = run(&as_avif(&source, JobOptions::default()), dir.path()).await;

	let (_, warnings) = written(&outcome);
	assert_eq!(warnings.len(), 2, "{warnings:?}");
	assert_eq!(warnings[0], format!("{AUDIO_DROPPED} (×2)"));
	assert!(warnings[1].starts_with(SUBTITLES_DROPPED), "{warnings:?}");
	assert!(warnings[1].contains("--burn-subs"), "{warnings:?}");
	let reported: Vec<&JobEvent> = events
		.iter()
		.filter(|event| matches!(event, JobEvent::Warning(_)))
		.collect();
	assert_eq!(reported.len(), 2, "{reported:?}");
}

#[tokio::test]
async fn burned_subtitles_are_drawn_into_the_animation() {
	let dir = tempfile::tempdir().unwrap();
	let cues = dir.path().join("cues.srt");
	fs::write(&cues, "1\n00:00:00,500 --> 00:00:01,000\nhello\n").unwrap();
	let source = support::black_clip_of(dir.path(), "talk.mkv", 2, &cues, "srt");
	let burned = JobOptions {
		burn_subs: Some(BurnSubs {
			pick: SubtitlePick::Default,
			size_percent: 10,
		}),
		..JobOptions::default()
	};

	let (with_burn, _) = run(&as_avif(&source, burned), &dir.path().join("burned")).await;
	let (plain, _) = run(&as_avif(&source, JobOptions::default()), &dir.path().join("plain")).await;

	let (burned_output, warnings) = written(&with_burn);
	assert!(
		!warnings.iter().any(|warning| warning.starts_with(SUBTITLES_DROPPED)),
		"{warnings:?}"
	);
	let lit = support::lit_span(&burned_output, 1).unwrap();
	assert!((lit.0 - 0.5).abs() < 0.05 && (lit.1 - 0.96).abs() < 0.05, "{lit:?}");
	assert_eq!(support::lit_span(&written(&plain).0, 1), None);
}

#[tokio::test]
async fn a_bitmap_track_is_burned_into_420_and_444_frames_alike() {
	let dir = tempfile::tempdir().unwrap();
	let pgs = support::pgs_box(dir.path(), 500, 1_500);
	let picture = |codec| support::Picture {
		source: "color=c=black",
		seconds: 2,
		codec,
		second_angle: false,
		marked_frame: None,
	};
	let half = support::subtitled_clip(
		dir.path(),
		"half.mkv",
		&picture(&["-c:v", "mpeg4", "-q:v", "2"]),
		&pgs,
		"copy",
	);
	let full = support::subtitled_clip(
		dir.path(),
		"full.mkv",
		&picture(&["-c:v", "ffv1", "-pix_fmt", "yuv444p"]),
		&pgs,
		"copy",
	);
	let burned = JobOptions {
		burn_subs: Some(BurnSubs {
			pick: SubtitlePick::Default,
			size_percent: 10,
		}),
		..JobOptions::default()
	};

	for (name, source) in [("half", &half), ("full", &full)] {
		let (outcome, _) = run(&as_avif(source, burned.clone()), &dir.path().join(name)).await;

		let (output, _) = written(&outcome);
		let lit = support::lit_span(&output, 1).unwrap();
		assert!(
			(lit.0 - 0.5).abs() < 0.05 && (lit.1 - 1.46).abs() < 0.05,
			"{name}: {lit:?}"
		);
	}
}

#[tokio::test]
async fn a_trim_keeps_only_its_frames() {
	let dir = tempfile::tempdir().unwrap();
	let source = support::mpeg4(dir.path(), "long.mp4", 3);
	let trimmed = JobOptions {
		trim: Some(Trim {
			start_s: 1.0,
			end_s: Some(2.0),
		}),
		..JobOptions::default()
	};

	let (outcome, _) = run(&as_avif(&source, trimmed), dir.path()).await;

	let tracks = sequences(&written(&outcome).0).await;
	assert_eq!(tracks.len(), 1, "{tracks:?}");
	assert!(
		tracks[0].nb_frames.is_some_and(|frames| frames.abs_diff(25) <= 1),
		"{:?}",
		tracks[0]
	);
}

#[tokio::test]
async fn a_trimmed_animation_larger_than_its_part_of_the_source_says_so() {
	let dir = tempfile::tempdir().unwrap();
	let source = clip(
		dir.path(),
		"noisy.mp4",
		"testsrc2=s=160x120:r=25:d=4,noise=alls=60:allf=t+u",
		&["-c:v", "mpeg4", "-q:v", "31"],
	);
	let trimmed = JobOptions {
		trim: Some(Trim {
			start_s: 1.0,
			end_s: Some(3.0),
		}),
		..JobOptions::default()
	};

	let (outcome, _) = run(&as_avif(&source, trimmed), dir.path()).await;

	let (_, warnings) = written(&outcome);
	assert!(
		warnings
			.iter()
			.any(|warning| warning.starts_with("the output is not smaller than the same part of the source")),
		"{warnings:?}"
	);
}

#[tokio::test]
async fn the_record_tells_what_was_done_to_the_picture_as_for_a_video() {
	let dir = tempfile::tempdir().unwrap();
	let cues = dir.path().join("cues.srt");
	fs::write(&cues, "1\n00:00:01,000 --> 00:00:02,000\nhello\n").unwrap();
	let source = support::black_clip_of(dir.path(), "talk.mkv", 3, &cues, "srt");
	let options = JobOptions {
		trim: Some(Trim {
			start_s: 0.5,
			end_s: Some(2.5),
		}),
		resize: Some(Resize {
			width: Some(160),
			height: None,
		}),
		burn_subs: Some(BurnSubs {
			pick: SubtitlePick::Default,
			size_percent: 10,
		}),
		..JobOptions::default()
	};

	let (outcome, _) = run(&as_avif(&source, options), dir.path()).await;

	let JobOutcome::Encoded { decision, .. } = &outcome else {
		panic!("{outcome:?}")
	};
	let picture = decision.video.as_ref().expect("the picture facts are recorded");
	assert_eq!(
		picture.trim,
		Some(cryarchiver_engine::decision::Trim {
			start_s: 0.5,
			length_s: 2.0
		})
	);
	assert!(picture.resized.is_some_and(|(width, _)| width == 160), "{picture:?}");
	assert!(picture.burned_subtitles, "{picture:?}");
	assert_eq!(
		(picture.scan, picture.deinterlaced),
		(Some(cryarchiver_engine::decision::Scan::Progressive), false)
	);
	assert!(decision.image.is_some(), "{decision:?}");
}

#[tokio::test]
async fn a_resize_scales_every_frame_keeping_the_aspect_ratio() {
	let dir = tempfile::tempdir().unwrap();
	let source = support::mpeg4(dir.path(), "wide.mp4", 1);
	let resized = JobOptions {
		resize: Some(Resize {
			width: Some(161),
			height: None,
		}),
		strip: true,
		..JobOptions::default()
	};

	let (outcome, _) = run(&as_avif(&source, resized), dir.path()).await;

	let (output, warnings) = written(&outcome);
	let tracks = sequences(&output).await;
	assert_eq!(tracks.len(), 1, "{tracks:?}");
	assert_eq!((tracks[0].width, tracks[0].height), (Some(161), Some(121)));
	assert_eq!(tracks[0].nb_frames, Some(25));
	assert!(
		warnings.contains(&"--strip doesn't apply to a video written as an animated AVIF".to_owned()),
		"{warnings:?}"
	);
}

#[tokio::test]
async fn an_anamorphic_video_is_scaled_to_the_size_it_is_shown_at() {
	let dir = tempfile::tempdir().unwrap();
	let source = clip(
		dir.path(),
		"wide.mkv",
		"testsrc2=s=240x180:r=10:d=1,setsar=4/3",
		&["-c:v", "mpeg4", "-q:v", "2"],
	);

	let (outcome, _) = run(&as_avif(&source, JobOptions::default()), dir.path()).await;

	let (output, warnings) = written(&outcome);
	let tracks = sequences(&output).await;
	assert_eq!((tracks[0].width, tracks[0].height), (Some(320), Some(180)));
	assert!(
		!warnings.iter().any(|warning| warning.contains("upscaled")),
		"{warnings:?}"
	);
}

#[tokio::test]
async fn a_transparent_video_keeps_its_alpha() {
	let dir = tempfile::tempdir().unwrap();
	let source = clip(
		dir.path(),
		"half.webm",
		"testsrc2=s=160x120:r=10:d=1,format=yuva420p,geq=lum='lum(X,Y)':cb='cb(X,Y)':cr='cr(X,Y)':a='if(lt(X,80),255,0)'",
		&["-c:v", "libvpx-vp9", "-pix_fmt", "yuva420p", "-crf", "20", "-b:v", "0"],
	);

	let (outcome, _) = run(&as_avif(&source, JobOptions::default()), dir.path()).await;

	let output = written(&outcome).0;
	let tracks = sequences(&output).await;
	assert_eq!(tracks.len(), 2, "{tracks:?}");
	let (alpha, width) = first_frame(&output, tracks[1].index, "gray");
	let row = &alpha[..width];
	assert!(row[..70].iter().all(|&value| value > 245), "{row:?}");
	assert!(row[90..].iter().all(|&value| value < 10), "{row:?}");
}

#[tokio::test]
async fn a_resized_transparent_video_keeps_its_alpha_at_the_new_size() {
	let dir = tempfile::tempdir().unwrap();
	let source = clip(
		dir.path(),
		"half.webm",
		"testsrc2=s=160x120:r=10:d=1,format=yuva420p,geq=lum='lum(X,Y)':cb='cb(X,Y)':cr='cr(X,Y)':a='if(lt(X,80),255,0)'",
		&["-c:v", "libvpx-vp9", "-pix_fmt", "yuva420p", "-crf", "20", "-b:v", "0"],
	);
	let resized = JobOptions {
		resize: Some(Resize {
			width: Some(80),
			height: None,
		}),
		..JobOptions::default()
	};

	let (outcome, _) = run(&as_avif(&source, resized), dir.path()).await;

	let output = written(&outcome).0;
	let tracks = sequences(&output).await;
	let sizes: Vec<_> = tracks.iter().map(|track| (track.width, track.height)).collect();
	assert_eq!(sizes, [(Some(80), Some(60)); 2]);
	let (alpha, width) = first_frame(&output, tracks[1].index, "gray");
	let row = &alpha[..width];
	assert!(row[..35].iter().all(|&value| value > 245), "{row:?}");
	assert!(row[45..].iter().all(|&value| value < 10), "{row:?}");
}

#[tokio::test]
async fn a_clip_with_too_many_frames_fails_before_any_work() {
	let dir = tempfile::tempdir().unwrap();
	let second = clip(
		dir.path(),
		"second.mkv",
		"color=c=gray:s=16x16:r=60:d=1",
		&["-c:v", "mpeg4"],
	);
	let long = support::converted(
		dir.path(),
		"long.mkv",
		&["-stream_loop", "2100"],
		&second,
		&["-c", "copy"],
	);

	let (outcome, events) = run(&as_avif(&long, JobOptions::default()), dir.path()).await;

	let reason = failure(&outcome);
	assert!(reason.contains("use --ss/--to to make a shorter clip"), "{reason}");
	assert!(
		!events
			.iter()
			.any(|event| matches!(event, JobEvent::Stage(Stage::Analyze | Stage::Encode))),
		"{events:?}"
	);
}

#[tokio::test]
async fn a_bitrate_cap_is_refused_for_an_animated_avif() {
	let dir = tempfile::tempdir().unwrap();
	let source = support::mpeg4(dir.path(), "clip.mp4", 1);
	let capped = JobOptions {
		max_bitrate_kbps: Some(2_000),
		..JobOptions::default()
	};

	let (outcome, _) = run(&as_avif(&source, capped), dir.path()).await;

	assert!(failure(&outcome).contains("--max-bitrate"), "{outcome:?}");
	assert!(!dir.path().join("clip.cry.avif").exists());
}

#[tokio::test]
async fn a_video_asked_for_as_jpeg_xl_still_fails() {
	let dir = tempfile::tempdir().unwrap();
	let source = support::mpeg4(dir.path(), "clip.mp4", 1);
	let job = Job {
		options: JobOptions {
			format: Some(OutputFormat::Jxl),
			..JobOptions::default()
		},
		..as_avif(&source, JobOptions::default())
	};

	let (outcome, _) = run(&job, dir.path()).await;

	assert_eq!(
		failure(&outcome),
		"encode failed: animated JPEG XL output isn't supported; use -f avif"
	);
}

#[tokio::test]
async fn an_interlaced_video_is_deinterlaced_to_its_field_rate() {
	let dir = tempfile::tempdir().unwrap();
	let source = clip(
		dir.path(),
		"fields.mkv",
		"testsrc2=s=320x240:r=50:d=1,interlace=scan=tff,setfield=tff",
		&["-c:v", "mpeg2video", "-b:v", "8M", "-flags", "+ilme+ildct"],
	);

	let (outcome, _) = run(&as_avif(&source, JobOptions::default()), dir.path()).await;

	let output = written(&outcome).0;
	let tracks = sequences(&output).await;
	assert_eq!(tracks.len(), 1, "{tracks:?}");
	assert_eq!(tracks[0].nb_frames, Some(50), "{:?}", tracks[0]);
	assert_eq!(delays_ms(&output, tracks[0].index), [20.0; 50]);
}

#[tokio::test]
async fn variable_frame_timing_is_kept() {
	let dir = tempfile::tempdir().unwrap();
	let source = clip(
		dir.path(),
		"uneven.mkv",
		"testsrc2=s=160x120:r=10:d=2,settb=1/1000,setpts='if(lt(N,10),N*50,500+(N-10)*100)'",
		&[
			"-fps_mode",
			"passthrough",
			"-enc_time_base",
			"1/1000",
			"-c:v",
			"mpeg4",
			"-q:v",
			"2",
		],
	);

	let (outcome, _) = run(&as_avif(&source, JobOptions::default()), dir.path()).await;

	let output = written(&outcome).0;
	let tracks = sequences(&output).await;
	assert_eq!(tracks[0].nb_frames, Some(20), "{:?}", tracks[0]);
	let delays = delays_ms(&output, tracks[0].index);
	assert_eq!(delays[..10], [50.0; 10], "{delays:?}");
	assert_eq!(delays[10..19], [100.0; 9], "{delays:?}");
}

#[tokio::test]
async fn a_limited_range_video_keeps_its_colours() {
	let dir = tempfile::tempdir().unwrap();
	let source = clip(
		dir.path(),
		"brick.mkv",
		"color=c=0xC04020:s=64x64:r=10:d=1,scale=out_color_matrix=bt709:out_range=tv,format=yuv420p,\
		 setparams=range=tv:color_primaries=bt709:color_trc=bt709:colorspace=bt709",
		&["-c:v", "mpeg4", "-q:v", "1"],
	);

	let (outcome, _) = run(&as_avif(&source, JobOptions::default()), dir.path()).await;

	let output = written(&outcome).0;
	let tracks = sequences(&output).await;
	let (rgb, width) = first_frame(&output, tracks[0].index, "rgb24");
	let centre = (32 * width + 32) * 3;
	let pixel = &rgb[centre..centre + 3];
	for (found, wanted) in pixel.iter().zip([0xC0u8, 0x40, 0x20]) {
		assert!(found.abs_diff(wanted) <= 6, "{pixel:?}");
	}
}

#[tokio::test]
async fn hdr_video_keeps_its_primaries_transfer_and_matrix() {
	let dir = tempfile::tempdir().unwrap();
	let source = clip(
		dir.path(),
		"pq.mkv",
		"testsrc2=s=160x120:r=10:d=1,format=yuv420p,\
		 setparams=range=tv:color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc",
		&["-c:v", "mpeg4", "-q:v", "2"],
	);

	let (outcome, _) = run(&as_avif(&source, JobOptions::default()), dir.path()).await;

	let output = written(&outcome).0;
	let tracks = sequences(&output).await;
	assert_eq!(tracks[0].color_primaries.as_deref(), Some("bt2020"), "{:?}", tracks[0]);
	assert_eq!(
		tracks[0].color_transfer.as_deref(),
		Some("smpte2084"),
		"{:?}",
		tracks[0]
	);
	assert_eq!(tracks[0].color_space.as_deref(), Some("bt2020nc"), "{:?}", tracks[0]);
	let nclx = parse_meta(&fs::read(&output).unwrap()).unwrap().primary_nclx().unwrap();
	assert_eq!((nclx.primaries, nclx.transfer, nclx.matrix), (9, 16, 9));
}

#[tokio::test]
async fn an_sd_video_is_tagged_with_the_matrix_its_frames_were_converted_with() {
	let dir = tempfile::tempdir().unwrap();
	let source = clip(
		dir.path(),
		"ntsc.mkv",
		"color=c=0xC04020:s=64x64:r=30:d=1,scale=out_color_matrix=smpte170m:out_range=tv,format=yuv420p,\
		 setparams=range=tv:color_primaries=smpte170m:color_trc=smpte170m:colorspace=smpte170m",
		&["-c:v", "mpeg4", "-q:v", "1"],
	);

	let (outcome, _) = run(&as_avif(&source, JobOptions::default()), dir.path()).await;

	let output = written(&outcome).0;
	let tracks = sequences(&output).await;
	assert_eq!(tracks[0].color_space.as_deref(), Some("smpte170m"), "{:?}", tracks[0]);
	let (rgb, width) = first_frame(&output, tracks[0].index, "rgb24");
	let centre = (32 * width + 32) * 3;
	let pixel = &rgb[centre..centre + 3];
	for (found, wanted) in pixel.iter().zip([0xC0u8, 0x40, 0x20]) {
		assert!(found.abs_diff(wanted) <= 6, "{pixel:?}");
	}
}
