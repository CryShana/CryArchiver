mod support;

use std::path::Path;

use cryarchiver_engine::outcome::JobError;
use cryarchiver_engine::toolchain::Toolchain;
use cryarchiver_engine::video::VideoStarts;
use cryarchiver_engine::video::colour::{ColourTags, Range};
use cryarchiver_engine::video::plan::{Encoder, VideoPlan};
use cryarchiver_engine::video::streams::StreamPlan;
use cryarchiver_engine::video::verify::{
	Expected, FrameTiming, Retiming, Synced, alpha_ssim, check_alpha, check_frame_times, check_sync, verify_output,
};
use tokio_util::sync::CancellationToken;

fn plan(alpha: bool, filters: &str, decoder: Option<&'static str>) -> VideoPlan {
	VideoPlan {
		streams: vec![0],
		decoder,
		shape: String::new(),
		filters: filters.into(),
		encoder: if alpha {
			Encoder::Vp9Alpha { cpu_used: 4, crf: 33 }
		} else {
			Encoder::SvtAv1 {
				preset: 10,
				crf: 32.0,
				film_grain: None,
			}
		},
		expected_frames: None,
		frame_tolerance: 2,
		width: 320,
		height: 240,
		colour: ColourTags {
			primaries: "bt709".into(),
			transfer: "bt709".into(),
			matrix: "bt709".into(),
			range: Range::Limited,
			chroma_location: None,
		},
		alpha,
		hdr10: false,
		warnings: Vec::new(),
		window: None,
		starts: VideoStarts::default(),
		overlay: None,
		subtitles: None,
		threads: None,
		max_bitrate_kbps: None,
	}
}

const TAGS: &str = "setparams=field_mode=prog:range=tv:color_primaries=bt709:color_trc=bt709:colorspace=bt709";

fn alpha_source(dir: &Path) -> std::path::PathBuf {
	let source = dir.join("alpha.webm");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=320x240:r=25:d=2,format=yuva420p,geq=lum='lum(X,Y)':cb='cb(X,Y)':cr='cr(X,Y)':a='if(lt(X,160),255,40)'",
		"-c:v",
		"libvpx-vp9",
		"-pix_fmt",
		"yuva420p",
		source.to_str().unwrap(),
	]);
	source
}

#[tokio::test]
async fn a_tagged_av1_output_passes_and_a_mislabelled_one_fails() {
	let dir = tempfile::tempdir().unwrap();
	let output = dir.path().join("mux.mkv");
	let filters = format!("format=yuv420p10le,{TAGS}");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=320x240:r=25:d=1",
		"-vf",
		&filters,
		"-c:v",
		"libsvtav1",
		"-preset",
		"12",
		output.to_str().unwrap(),
	]);
	let tools = support::toolchain().await;
	let good = plan(false, "", None);
	let mut wrong = plan(false, "", None);
	wrong.colour.transfer = "smpte2084".into();
	let streams = StreamPlan::default();
	let passes = Expected {
		video: &good,
		streams: &streams,
		source_chapters: 0,
		attached_files: 0,
		start_offsets: Vec::new(),
		source_fps: None,
	};
	let fails = Expected {
		video: &wrong,
		streams: &streams,
		source_chapters: 0,
		attached_files: 0,
		start_offsets: Vec::new(),
		source_fps: None,
	};

	verify_output(&tools, &output, &passes, &CancellationToken::new())
		.await
		.unwrap();
	let err = verify_output(&tools, &output, &fails, &CancellationToken::new())
		.await
		.unwrap_err();
	assert!(err.to_string().contains("colour tags"), "{err}");
}

#[tokio::test]
async fn alpha_survives_a_libvpx_round_trip_and_an_opaque_encode_is_caught() {
	let dir = tempfile::tempdir().unwrap();
	let source = alpha_source(dir.path());
	let (good, opaque) = (dir.path().join("good.mkv"), dir.path().join("opaque.mkv"));
	let s = source.to_str().unwrap();
	let encode = ["-c:v", "libvpx-vp9", "-pix_fmt", "yuva420p", "-crf", "32", "-b:v", "0"];
	let mut args = vec!["-c:v", "libvpx-vp9", "-i", s];
	args.extend_from_slice(&encode);
	args.push(good.to_str().unwrap());
	support::ffmpeg(&args);
	let mut args = vec!["-i", s];
	args.extend_from_slice(&encode);
	args.push(opaque.to_str().unwrap());
	support::ffmpeg(&args);
	let tools = support::toolchain().await;
	let video = plan(true, "format=yuva420p", Some("libvpx-vp9"));

	let kept = alpha_ssim(&tools, &good, &source, &video, &CancellationToken::new())
		.await
		.unwrap();
	let lost = alpha_ssim(&tools, &opaque, &source, &video, &CancellationToken::new())
		.await
		.unwrap();

	assert!(check_alpha(kept).is_ok(), "{kept}");
	assert!(check_alpha(lost).is_err(), "{lost}");
}

#[tokio::test]
async fn an_output_whose_audio_lags_its_video_fails_the_sync_check() {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("audio-first.m2ts");
	support::ffmpeg(&[
		"-itsoffset",
		"0.3",
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=320x240:r=25:d=4",
		"-f",
		"lavfi",
		"-i",
		"sine=duration=4.3",
		"-map",
		"0",
		"-map",
		"1",
		"-c:v",
		"mpeg2video",
		"-c:a",
		"mp2",
		"-mpegts_m2ts_mode",
		"1",
		source.to_str().unwrap(),
	]);
	let s = source.to_str().unwrap();
	let (synced, lagging) = (dir.path().join("synced.mkv"), dir.path().join("lagging.mkv"));
	support::ffmpeg(&["-i", s, "-map", "0", "-c", "copy", synced.to_str().unwrap()]);
	support::ffmpeg(&[
		"-i",
		s,
		"-itsoffset",
		"0.3",
		"-i",
		s,
		"-map",
		"0:v",
		"-map",
		"1:a",
		"-c",
		"copy",
		lagging.to_str().unwrap(),
	]);
	let tools = support::toolchain().await;
	let streams = [
		Synced {
			source: 0,
			output: 0,
			audio: false,
		},
		Synced {
			source: 1,
			output: 1,
			audio: true,
		},
	];
	let check = |output: std::path::PathBuf| {
		let (tools, source, streams) = (&tools, &source, &streams);
		async move {
			check_sync(
				tools,
				(source, &output),
				streams,
				(0.0, false),
				0.04,
				&CancellationToken::new(),
			)
			.await
		}
	};

	check(synced).await.unwrap();
	let err = check(lagging).await.unwrap_err();

	assert!(err.to_string().contains("stream 1 starts"), "{err}");
}

#[tokio::test]
async fn a_variable_rate_output_merged_onto_the_declared_rate_fails_the_frame_time_check() {
	let dir = tempfile::tempdir().unwrap();
	let source = support::variable_rate(dir.path(), "variable.mkv", 2);
	let kept = encoded_av1(&source, "kept.mkv", &["-enc_time_base:v", "1/1000"]);
	let merged = encoded_av1(&source, "merged.mkv", &[]);
	let tools = support::toolchain().await;

	Box::pin(frame_times_of(&tools, &source, &kept)).await.unwrap();
	let err = Box::pin(frame_times_of(&tools, &source, &merged)).await.unwrap_err();

	assert!(
		err.to_string()
			.contains("the video frame at 0.100 s is at no frame time of the source"),
		"{err}"
	);
}

#[tokio::test]
async fn a_source_that_shows_a_time_twice_passes_the_frame_time_check_when_the_output_does_too() {
	let dir = tempfile::tempdir().unwrap();
	let clean = dir.path().join("clean.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=160x120:r=10:d=2",
		"-c:v",
		"mpeg4",
		clean.to_str().unwrap(),
	]);
	let repeated = support::converted(
		dir.path(),
		"repeated.mkv",
		&[],
		&clean,
		&["-c", "copy", "-bsf:v", "setts=ts=if(eq(N\\,5)\\,PREV_OUTPTS\\,PTS)"],
	);
	let output = encoded_av1(&repeated, "output.mkv", &["-enc_time_base:v", "1/1000"]);
	let tools = support::toolchain().await;

	let times = support::frame_times(&output, 0);

	assert!(
		times.windows(2).any(|pair| (pair[1] - pair[0]).abs() < 0.0005),
		"{times:?}"
	);
	Box::pin(frame_times_of(&tools, &repeated, &output)).await.unwrap();
}

fn encoded_av1(source: &Path, name: &str, time_base: &[&str]) -> std::path::PathBuf {
	let output = source.with_file_name(name);
	let mut args = vec!["-i", source.to_str().unwrap(), "-c:v", "libsvtav1", "-preset", "12"];
	args.extend_from_slice(time_base);
	args.extend(["-fps_mode", "passthrough", output.to_str().unwrap()]);
	support::ffmpeg(&args);
	output
}

async fn frame_times_of(tools: &Toolchain, source: &Path, output: &Path) -> Result<(), JobError> {
	check_frame_times(
		tools,
		(source, output),
		0,
		FrameTiming {
			retiming: Retiming::Kept,
			window: None,
			discontinuous: false,
		},
		&CancellationToken::new(),
	)
	.await
}
