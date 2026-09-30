mod support;

use std::path::Path;

use cryarchiver_engine::video::VideoStarts;
use cryarchiver_engine::video::colour::{ColourTags, Range};
use cryarchiver_engine::video::plan::{Encoder, VideoPlan};
use cryarchiver_engine::video::streams::StreamPlan;
use cryarchiver_engine::video::verify::{Expected, Synced, alpha_ssim, check_alpha, check_sync, verify_output};
use tokio_util::sync::CancellationToken;

fn plan(alpha: bool, filters: &str, decoder: Option<&'static str>) -> VideoPlan {
	VideoPlan {
		streams: vec![0],
		decoder,
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
