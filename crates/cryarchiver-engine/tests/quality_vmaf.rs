mod support;

use std::path::{Path, PathBuf};

use cryarchiver_engine::outcome::ErrorKind;
use cryarchiver_engine::quality::vmaf::{
	FrameWindow, Reference, Scoring, frames, pool, sample_command, window_command,
};
use tokio_util::sync::CancellationToken;

const TEN_BIT: &str = "format=yuv420p10le";

fn reference(source: &Path) -> Reference<'_> {
	Reference {
		source,
		stream: 0,
		decoder: None,
		filters: TEN_BIT,
	}
}

fn scoring<'a>(dir: &'a Path, log: &'a str) -> Scoring<'a> {
	Scoring {
		model: "vmaf_v0.6.1",
		threads: 4,
		dir,
		log,
	}
}

fn lossless_copy(source: &Path, output: &Path, before_input: &[&str], filters: &str) -> PathBuf {
	let mut args: Vec<&str> = before_input.to_vec();
	args.extend([
		"-i",
		source.to_str().unwrap(),
		"-map",
		"0:v",
		"-vf",
		filters,
		"-c:v",
		"ffv1",
	]);
	args.push(output.to_str().unwrap());
	support::ffmpeg(&args);
	output.to_path_buf()
}

#[tokio::test]
async fn a_sample_cut_like_the_reference_scores_near_the_top() {
	let dir = tempfile::tempdir().unwrap();
	let ffmpeg = support::toolchain().await.ffmpeg;
	let source = support::mpeg4(dir.path(), "clip.mp4", 4);
	let aligned = lossless_copy(
		&source,
		&dir.path().join("aligned.mkv"),
		&["-ss", "1.000", "-t", "2.000"],
		"null",
	);
	let late = lossless_copy(
		&source,
		&dir.path().join("late.mkv"),
		&["-ss", "1.080", "-t", "2.000"],
		"null",
	);
	let cancel = CancellationToken::new();

	let aligned_scoring = scoring(dir.path(), "aligned.json");
	let aligned_cmd = sample_command(&ffmpeg, &aligned, &reference(&source), 1.0, 2.0, &aligned_scoring);
	let aligned = pool(&frames(&aligned_cmd, &aligned_scoring, |_| {}, &cancel).await.unwrap()).unwrap();
	let late_scoring = scoring(dir.path(), "late.json");
	let late_cmd = sample_command(&ffmpeg, &late, &reference(&source), 1.0, 2.0, &late_scoring);
	let late = pool(&frames(&late_cmd, &late_scoring, |_| {}, &cancel).await.unwrap()).unwrap();

	assert!(aligned.mean > 99.0, "{aligned:?}");
	assert_eq!(aligned.frames, 50);
	assert!(late.mean < aligned.mean - 5.0, "{late:?} vs {aligned:?}");
	assert!(!dir.path().join("aligned.json").exists());
}

#[tokio::test]
async fn windows_pair_frames_by_position_and_pace_the_progress() {
	let dir = tempfile::tempdir().unwrap();
	let ffmpeg = support::toolchain().await.ffmpeg;
	let source = support::mpeg4(dir.path(), "clip.mp4", 4);
	let copy = lossless_copy(&source, &dir.path().join("copy.mkv"), &[], "null");
	let shifted = lossless_copy(
		&source,
		&dir.path().join("shifted.mkv"),
		&[],
		"trim=start_frame=1,setpts=PTS-STARTPTS",
	);
	let windows = [FrameWindow { first: 5, last: 14 }, FrameWindow { first: 60, last: 69 }];
	let cancel = CancellationToken::new();

	let mut paced = 0;
	let copy_scoring = scoring(dir.path(), "copy.json");
	let copy_cmd = window_command(&ffmpeg, &copy, &reference(&source), &windows, &copy_scoring);
	let copy_frames = frames(
		&copy_cmd,
		&copy_scoring,
		|progress| paced = paced.max(progress.frame.unwrap_or(0)),
		&cancel,
	)
	.await
	.unwrap();
	let same = pool(&copy_frames).unwrap();
	let shifted_scoring = scoring(dir.path(), "shifted.json");
	let shifted_cmd = window_command(&ffmpeg, &shifted, &reference(&source), &windows, &shifted_scoring);
	let off_by_one = pool(&frames(&shifted_cmd, &shifted_scoring, |_| {}, &cancel).await.unwrap()).unwrap();

	assert!(same.mean > 99.0, "{same:?}");
	assert_eq!(same.frames, 20);
	assert_eq!(paced, 100);
	assert!(off_by_one.mean < same.mean - 5.0, "{off_by_one:?} vs {same:?}");
}

#[tokio::test]
async fn a_failed_scoring_run_is_an_encode_failure() {
	let dir = tempfile::tempdir().unwrap();
	let ffmpeg = support::toolchain().await.ffmpeg;
	let source = support::mpeg4(dir.path(), "clip.mp4", 1);
	let broken = Reference {
		filters: "no_such_filter",
		..reference(&source)
	};
	let scoring = scoring(dir.path(), "broken.json");
	let cmd = sample_command(&ffmpeg, &source, &broken, 0.0, 1.0, &scoring);

	let err = frames(&cmd, &scoring, |_| {}, &CancellationToken::new())
		.await
		.unwrap_err();

	assert!(matches!(err.kind, ErrorKind::EncodeFailed(_)), "{err:?}");
}
