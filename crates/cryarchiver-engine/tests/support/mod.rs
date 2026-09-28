#![allow(dead_code)]

pub mod heif;
pub mod tiff;

use std::path::{Path, PathBuf};
use std::process::Command;

use cryarchiver_engine::toolchain::{ToolConfig, Toolchain};
use tokio_util::sync::CancellationToken;

pub async fn toolchain() -> Toolchain {
	let config = ToolConfig { ffmpeg: None };
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

pub fn smooth_png(dir: &Path, name: &str, width: u32, height: u32) -> PathBuf {
	let path = dir.join(name);
	image::ImageBuffer::from_fn(width, height, |x, y| {
		image::Rgb([
			u8::try_from((x * 255) / width.max(1)).unwrap(),
			u8::try_from((y * 255) / height.max(1)).unwrap(),
			u8::try_from((x / 8 + y / 8) % 2 * 100 + 50).unwrap(),
		])
	})
	.save(&path)
	.unwrap();
	path
}

pub fn png_chunk(kind: [u8; 4], body: &[u8]) -> Vec<u8> {
	let mut bytes = u32::try_from(body.len()).unwrap().to_be_bytes().to_vec();
	bytes.extend_from_slice(&kind);
	bytes.extend_from_slice(body);
	bytes.extend_from_slice(&crc32fast::hash(&[kind.as_slice(), body].concat()).to_be_bytes());
	bytes
}

pub fn with_png_chunk(png: &[u8], chunk: &[u8]) -> Vec<u8> {
	let idat = png.windows(4).position(|window| window == b"IDAT").unwrap() - 4;
	[&png[..idat], chunk, &png[idat..]].concat()
}

pub fn with_jpeg_segment(jpeg: &[u8], marker: u8, payload: &[u8]) -> Vec<u8> {
	let mut out = jpeg[..2].to_vec();
	out.extend_from_slice(&[0xFF, marker]);
	out.extend_from_slice(&u16::try_from(payload.len() + 2).unwrap().to_be_bytes());
	out.extend_from_slice(payload);
	out.extend_from_slice(&jpeg[2..]);
	out
}

pub fn photo_png(dir: &Path, name: &str, width: u32, height: u32) -> PathBuf {
	let path = dir.join(name);
	let (x1, y1) = (width - 1, height - 1);
	let source = format!(
		"gradients=s={width}x{height}:c0=0x3060a0:c1=0xe0b070:nb_colors=2:x0=0:y0=0:x1={x1}:y1={y1}:speed=0,\
		 noise=alls=3:allf=u:all_seed=1,format=rgb24"
	);
	ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		source.as_str(),
		"-frames:v",
		"1",
		path.to_str().unwrap(),
	]);
	path
}

pub fn clip_with_extras(dir: &Path) -> PathBuf {
	let cues = dir.join("cues.srt");
	std::fs::write(
		&cues,
		"1\n00:00:01,000 --> 00:00:02,000\none\n\n2\n00:00:05,000 --> 00:00:06,000\nfive\n\n3\n00:00:08,000 --> 00:00:09,000\neight\n",
	)
	.unwrap();
	let chapters = dir.join("chapters.txt");
	std::fs::write(
		&chapters,
		";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=4000\ntitle=A\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=4000\nEND=7000\ntitle=B\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=7000\nEND=10000\ntitle=C\n",
	)
	.unwrap();
	let font = dir.join("font.ttf");
	std::fs::write(&font, b"not a real font").unwrap();

	let clip = dir.join("extras.mkv");
	ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=320x240:r=25:d=10",
		"-f",
		"lavfi",
		"-i",
		"sine=duration=10",
		"-i",
		cues.to_str().unwrap(),
		"-i",
		chapters.to_str().unwrap(),
		"-map",
		"0",
		"-map",
		"1",
		"-map",
		"2",
		"-map_chapters",
		"3",
		"-c:v",
		"mpeg4",
		"-q:v",
		"2",
		"-c:a",
		"aac",
		"-c:s",
		"srt",
		"-attach",
		font.to_str().unwrap(),
		"-metadata:s:t",
		"mimetype=application/x-truetype-font",
		clip.to_str().unwrap(),
	]);
	clip
}
