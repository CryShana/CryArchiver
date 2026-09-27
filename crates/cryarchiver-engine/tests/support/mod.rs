#![allow(dead_code)]

pub mod heif;
pub mod tiff;

use std::path::{Path, PathBuf};
use std::process::Command;

use cryarchiver_engine::toolchain::{ToolConfig, Toolchain};
use tokio_util::sync::CancellationToken;

pub async fn toolchain() -> Toolchain {
	let config = ToolConfig {
		ffmpeg: None,
		cache_dir: Some(std::env::temp_dir().join("cryarchiver-test-cache")),
		refresh: false,
	};
	Toolchain::detect(&config, &CancellationToken::new())
		.await
		.expect("ffmpeg and ffprobe on PATH")
}

pub fn ffmpeg(args: &[&str]) {
	let status = Command::new("ffmpeg")
		.args(["-hide_banner", "-v", "error", "-y"])
		.args(args)
		.status()
		.unwrap();
	assert!(status.success(), "ffmpeg {args:?} failed");
}

pub fn tone(dir: &Path, name: &str, layout: &str, channels: u32, seconds: u32, codec_args: &[&str]) -> PathBuf {
	let expr = (0..channels)
		.map(|i| format!("0.3*sin(2*PI*{}*t)", 220 + i * 110))
		.collect::<Vec<_>>()
		.join("|");
	let source = format!("aevalsrc={expr}:c={layout}:s=48000:d={seconds}");
	let path = dir.join(name);

	let mut args = vec!["-f", "lavfi", "-i", source.as_str()];
	args.extend_from_slice(codec_args);
	args.push(path.to_str().unwrap());
	ffmpeg(&args);
	path
}

pub fn video(dir: &Path, name: &str, seconds: u32, args: &[&str]) -> PathBuf {
	let video = format!("testsrc2=s=320x240:r=25:d={seconds}");
	let audio = format!("sine=frequency=440:duration={seconds}");
	let path = dir.join(name);

	let mut all = vec!["-f", "lavfi", "-i", video.as_str(), "-f", "lavfi", "-i", audio.as_str()];
	all.extend_from_slice(args);
	all.push(path.to_str().unwrap());
	ffmpeg(&all);
	path
}

pub fn mpeg4(dir: &Path, name: &str, seconds: u32) -> PathBuf {
	video(
		dir,
		name,
		seconds,
		&[
			"-map", "0:v", "-map", "1:a", "-c:v", "mpeg4", "-q:v", "2", "-c:a", "aac", "-b:a", "192k",
		],
	)
}

pub fn mp3_with_cover(dir: &Path, name: &str, title: &str) -> PathBuf {
	let path = dir.join(name);
	let title_tag = format!("title={title}");
	ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"sine=frequency=440:duration=3",
		"-f",
		"lavfi",
		"-i",
		"color=c=red:s=64x64",
		"-frames:v",
		"1",
		"-map",
		"0:a",
		"-map",
		"1:v",
		"-c:a",
		"libmp3lame",
		"-b:a",
		"192k",
		"-c:v",
		"png",
		"-disposition:v",
		"attached_pic",
		"-id3v2_version",
		"3",
		"-metadata",
		title_tag.as_str(),
		path.to_str().unwrap(),
	]);
	path
}
