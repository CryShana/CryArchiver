mod support;

use std::path::{Path, PathBuf};

use cryarchiver_engine::images::animated::{
	Animation, ScanError, Timeline, encode_command, expected_delays, has_alpha, plan_animation, plays, sampled_frames,
	scan, split_pams, verify_animation,
};
use cryarchiver_engine::outcome::KeepReason;
use cryarchiver_engine::preset::Speed;
use cryarchiver_engine::process::{StdoutMode, run};
use tokio_util::sync::CancellationToken;

const TRANSPARENT_SOURCE: &str =
	"testsrc2=s=96x64:d=0.5:r=10,format=rgba,geq=r='r(X,Y)':g='g(X,Y)':b='b(X,Y)':a='if(lt(X,48),255,0)'";
const KEEP_GIF_TRANSPARENCY: [&str; 2] = [
	"-filter_complex",
	"split[a][b];[a]palettegen=reserve_transparent=1[p];[b][p]paletteuse=alpha_threshold=128",
];

fn animation_file(dir: &Path, name: &str, source: &str, extra: &[&str]) -> PathBuf {
	let path = dir.join(name);
	let mut args = vec!["-f", "lavfi", "-i", source];
	args.extend_from_slice(extra);
	args.push(path.to_str().unwrap());
	support::ffmpeg(&args);
	path
}

#[test]
fn gif_and_apng_timelines_are_read_with_their_loop_counts() {
	let dir = tempfile::tempdir().unwrap();
	let gif = animation_file(dir.path(), "a.gif", "testsrc2=s=64x48:d=0.5:r=10", &["-loop", "2"]);
	let apng = animation_file(
		dir.path(),
		"a.apng",
		"testsrc2=s=64x48:d=0.5:r=10",
		&["-plays", "0", "-f", "apng"],
	);
	let still = animation_file(dir.path(), "a.png", "testsrc2=s=64x48", &["-frames:v", "1"]);

	let gif = scan(&std::fs::read(gif).unwrap()).unwrap().unwrap();
	let apng = scan(&std::fs::read(apng).unwrap()).unwrap().unwrap();

	assert_eq!((gif.frames, gif.plays, gif.width, gif.height), (5, Some(3), 64, 48));
	assert_eq!(gif.delays_ms, [100; 5]);
	assert_eq!((apng.frames, apng.plays), (5, None));
	assert_eq!(scan(&std::fs::read(still).unwrap()).unwrap(), None);
}

#[test]
fn a_truncated_gif_stops_at_its_first_bad_frame() {
	let dir = tempfile::tempdir().unwrap();
	let gif = std::fs::read(animation_file(dir.path(), "a.gif", "testsrc2=s=64x48:d=1:r=10", &[])).unwrap();

	let err = scan(&gif[..gif.len() / 2]).unwrap_err();

	assert!(
		matches!(&err, ScanError::Unreadable(reason) if reason.contains("unreadable")),
		"{err:?}"
	);
}

#[tokio::test]
async fn transparency_is_measured_on_ffmpegs_decode() {
	let dir = tempfile::tempdir().unwrap();
	let tools = support::toolchain().await;
	let cancel = CancellationToken::new();
	let transparent = animation_file(dir.path(), "t.gif", TRANSPARENT_SOURCE, &KEEP_GIF_TRANSPARENCY);
	let opaque = animation_file(dir.path(), "o.gif", "testsrc2=s=96x64:d=0.5:r=10", &[]);

	assert!(has_alpha(&tools, &transparent, &cancel).await.unwrap());
	assert!(!has_alpha(&tools, &opaque, &cancel).await.unwrap());
}

#[test]
fn settings_follow_the_speed_and_need_libaom() {
	assert_eq!(plan_animation(Speed::Fast, true).unwrap().crf, 16);
	assert_eq!(plan_animation(Speed::Moderate, true).unwrap().crf, 10);
	assert_eq!(plan_animation(Speed::Slow, true).unwrap().crf, 6);
	assert!(matches!(
		plan_animation(Speed::Fast, false),
		Err(KeepReason::Unsupported(reason)) if reason.contains("libaom")
	));
}

#[test]
fn frames_are_sampled_evenly_and_zero_gif_delays_use_ffmpegs_default() {
	let timeline = Timeline {
		frames: 3,
		delays_ms: vec![0, 50, 0],
		plays: None,
		width: 1,
		height: 1,
	};

	assert_eq!(sampled_frames(3), [0, 1, 2]);
	assert_eq!(sampled_frames(422), [0, 42, 84, 126, 168, 211, 253, 295, 337, 379]);
	let huge = sampled_frames(u32::MAX);
	assert_eq!(huge.len(), 10);
	assert!(huge.windows(2).all(|pair| pair[0] < pair[1]) && huge[9] < u32::MAX);
	assert_eq!(expected_delays(&timeline, true), [100, 50, 100]);
	assert_eq!(expected_delays(&timeline, false), [0, 50, 0]);
}

#[test]
fn concatenated_pam_frames_are_split() {
	let frame = |value: u8| {
		let mut bytes = b"P7\nWIDTH 2\nHEIGHT 1\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n".to_vec();
		bytes.extend_from_slice(&[value; 8]);
		bytes
	};

	let frames = split_pams(&[frame(10), frame(200)].concat()).unwrap();

	assert_eq!(frames.len(), 2);
	assert_eq!(frames[1].to_rgba8().get_pixel(1, 0).0, [200; 4]);
	assert!(split_pams(&frame(10)[..40]).is_err());
}

async fn encode_and_verify(dir: &Path, source: &Path) -> (Animation, f64) {
	let tools = support::toolchain().await;
	let cancel = CancellationToken::new();
	let animation = Animation {
		timeline: scan(&std::fs::read(source).unwrap()).unwrap().unwrap(),
		alpha: has_alpha(&tools, source, &cancel).await.unwrap(),
	};
	let output = dir.join("out.avif");
	let cmd = encode_command(
		&tools.ffmpeg,
		source,
		&output,
		&animation,
		plan_animation(Speed::Fast, true).unwrap(),
	);

	assert!(
		run(&cmd, StdoutMode::Capture, |_| {}, &cancel)
			.await
			.unwrap()
			.status
			.success()
	);
	let score = verify_animation(&tools, source, &output, &animation, &cancel)
		.await
		.unwrap();
	assert_eq!(
		plays(&std::fs::read(&output).unwrap()),
		Some(animation.timeline.plays.map(u64::from))
	);
	(animation, score)
}

#[tokio::test]
async fn an_opaque_apng_becomes_a_verified_animated_avif() {
	let dir = tempfile::tempdir().unwrap();
	let apng = animation_file(
		dir.path(),
		"a.apng",
		"testsrc2=s=160x120:d=1:r=10",
		&["-plays", "3", "-f", "apng"],
	);

	let (animation, score) = encode_and_verify(dir.path(), &apng).await;

	assert!(!animation.alpha);
	assert!(score >= 80.0, "{score}");
}

#[tokio::test]
async fn a_transparent_gif_keeps_its_alpha_track() {
	let dir = tempfile::tempdir().unwrap();
	let gif = animation_file(dir.path(), "t.gif", TRANSPARENT_SOURCE, &KEEP_GIF_TRANSPARENCY);

	let (animation, score) = encode_and_verify(dir.path(), &gif).await;

	assert!(animation.alpha);
	assert!(score >= 80.0, "{score}");
}

#[tokio::test]
async fn a_transparent_apng_keeps_its_alpha_track() {
	let dir = tempfile::tempdir().unwrap();
	let apng = animation_file(dir.path(), "t.apng", TRANSPARENT_SOURCE, &["-plays", "3", "-f", "apng"]);

	let (animation, score) = encode_and_verify(dir.path(), &apng).await;

	assert!(animation.alpha);
	assert!(score >= 80.0, "{score}");
}

#[test]
fn a_gif_without_a_loop_extension_plays_once() {
	let dir = tempfile::tempdir().unwrap();
	let once = animation_file(dir.path(), "once.gif", "testsrc2=s=64x48:d=0.5:r=10", &["-loop", "-1"]);
	let forever = animation_file(
		dir.path(),
		"forever.gif",
		"testsrc2=s=64x48:d=0.5:r=10",
		&["-loop", "0"],
	);

	let once = std::fs::read(once).unwrap();
	let forever = std::fs::read(forever).unwrap();

	assert!(!once.windows(11).any(|window| window == b"NETSCAPE2.0"));
	assert_eq!(scan(&once).unwrap().unwrap().plays, Some(1));
	assert_eq!(scan(&forever).unwrap().unwrap().plays, None);
}

#[test]
fn animations_past_the_frame_limit_are_too_long_to_verify() {
	let mut gif = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff".to_vec();
	gif.extend_from_slice(b"\x21\xff\x0bNETSCAPE2.0\x03\x01\x00\x00\x00");
	for _ in 0..100_001 {
		gif.extend_from_slice(b"\x21\xf9\x04\x00\x01\x00\x00\x00");
		gif.extend_from_slice(b"\x2c\x00\x00\x00\x00\x01\x00\x01\x00\x00");
		gif.extend_from_slice(b"\x02\x02\x44\x01\x00");
	}
	gif.push(0x3B);

	assert_eq!(scan(&gif), Err(ScanError::TooManyFrames));
}
