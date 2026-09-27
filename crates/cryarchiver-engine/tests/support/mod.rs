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

pub fn exif_block(orientation: u16, width: u32, height: u32) -> Vec<u8> {
	let mut tiff = b"II*\0".to_vec();
	tiff.extend_from_slice(&8u32.to_le_bytes());

	let exif_at = 8 + 2 + 3 * 12 + 4;
	tiff.extend_from_slice(&3u16.to_le_bytes());
	for (tag, kind, value) in [
		(0x0112u16, 3u16, u32::from(orientation)),
		(0x0131, 2, 0x0074_7874),
		(0x8769, 4, exif_at),
	] {
		tiff.extend_from_slice(&tag.to_le_bytes());
		tiff.extend_from_slice(&kind.to_le_bytes());
		tiff.extend_from_slice(&(if kind == 2 { 4u32 } else { 1 }).to_le_bytes());
		tiff.extend_from_slice(&value.to_le_bytes());
	}
	tiff.extend_from_slice(&0u32.to_le_bytes());

	tiff.extend_from_slice(&2u16.to_le_bytes());
	for (tag, value) in [(0xA002u16, width), (0xA003, height)] {
		tiff.extend_from_slice(&tag.to_le_bytes());
		tiff.extend_from_slice(&4u16.to_le_bytes());
		tiff.extend_from_slice(&1u32.to_le_bytes());
		tiff.extend_from_slice(&value.to_le_bytes());
	}
	tiff.extend_from_slice(&0u32.to_le_bytes());
	tiff
}

pub fn jpeg_with_exif(dir: &Path, name: &str, width: u32, height: u32, orientation: u16) -> PathBuf {
	let path = dir.join(name);
	let source = format!("testsrc2=s={width}x{height}");
	ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		source.as_str(),
		"-frames:v",
		"1",
		"-q:v",
		"2",
		path.to_str().unwrap(),
	]);

	let jpeg = std::fs::read(&path).unwrap();
	let payload = [b"Exif\0\0".as_slice(), &exif_block(orientation, width, height)].concat();
	let mut out = jpeg[..2].to_vec();
	out.extend_from_slice(&[0xFF, 0xE1]);
	out.extend_from_slice(&u16::try_from(payload.len() + 2).unwrap().to_be_bytes());
	out.extend_from_slice(&payload);
	out.extend_from_slice(&jpeg[2..]);
	std::fs::write(&path, out).unwrap();
	path
}
