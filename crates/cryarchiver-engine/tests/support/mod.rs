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

/// A 10 s black 320×240 mpeg4 clip with a tone and `subtitles` muxed with `codec`, and optionally a second black
/// video stream after the main one.
pub fn black_clip(dir: &Path, name: &str, subtitles: &Path, codec: &str, second_angle: bool) -> PathBuf {
	subtitled_clip(
		dir,
		name,
		&Picture {
			source: "color=c=black",
			seconds: 10,
			codec: &["-c:v", "mpeg4"],
			second_angle,
		},
		subtitles,
		codec,
	)
}

pub struct Picture<'a> {
	/// A lavfi source without its size, rate or duration, such as `color=c=black` or `testsrc2`.
	pub source: &'a str,
	pub seconds: u32,
	pub codec: &'a [&'a str],
	pub second_angle: bool,
}

/// A 320×240, 25 fps clip with a tone and `subtitles` muxed with `codec`. The subtitles keep their own times
/// (`-copyts`), so a track whose first cue is late still starts late.
pub fn subtitled_clip(dir: &Path, name: &str, picture: &Picture<'_>, subtitles: &Path, codec: &str) -> PathBuf {
	let clip = dir.join(name);
	let joint = if picture.source.contains('=') { ':' } else { '=' };
	let video = format!("{}{joint}s=320x240:r=25:d={}", picture.source, picture.seconds);
	let audio = format!("sine=duration={}", picture.seconds);
	let mut args = vec![
		"-copyts",
		"-f",
		"lavfi",
		"-i",
		&video,
		"-f",
		"lavfi",
		"-i",
		&audio,
		"-i",
		subtitles.to_str().unwrap(),
		"-map",
		"0",
	];
	if picture.second_angle {
		args.extend(["-map", "0"]);
	}
	args.extend(["-map", "1", "-map", "2"]);
	args.extend(picture.codec);
	args.extend(["-c:a", "aac", "-c:s", codec, clip.to_str().unwrap()]);
	ffmpeg(&args);
	clip
}

/// Every video frame time of stream `stream`, in order.
pub fn frame_times(path: &Path, stream: usize) -> Vec<f64> {
	let out = Command::new("ffprobe")
		.args(["-v", "error", "-select_streams"])
		.arg(format!("v:{stream}"))
		.args(["-show_entries", "frame=pts_time", "-of", "csv=p=0"])
		.arg(path)
		.output()
		.unwrap();
	String::from_utf8_lossy(&out.stdout)
		.lines()
		.filter_map(|line| line.trim().trim_end_matches(',').parse().ok())
		.collect()
}

/// The first and last frame times at which anything bright is drawn on a black picture, in video stream `stream`.
pub fn lit_span(path: &Path, stream: usize) -> Option<(f64, f64)> {
	let out = Command::new("ffmpeg")
		.args(["-v", "error", "-i"])
		.arg(path)
		.arg("-map")
		.arg(format!("0:v:{stream}"))
		.args([
			"-vf",
			"format=gray,signalstats,metadata=mode=print:key=lavfi.signalstats.YMAX:file=-",
			"-f",
			"null",
			"-",
		])
		.output()
		.unwrap();

	let mut time = 0.0;
	let mut lit = Vec::new();
	for line in String::from_utf8_lossy(&out.stdout).lines() {
		if let Some(pts) = line
			.split_whitespace()
			.find_map(|field| field.strip_prefix("pts_time:"))
		{
			time = pts.parse().unwrap();
		} else if let Some(ymax) = line.strip_prefix("lavfi.signalstats.YMAX=")
			&& ymax.parse::<f64>().unwrap() > 128.0
		{
			lit.push(time);
		}
	}
	Some((*lit.first()?, *lit.last()?))
}

/// A PGS (bitmap) subtitle file for a 320×240 picture: a white 120×30 box shown from `from_ms` until `to_ms`.
/// ffmpeg can't turn text subtitles into bitmaps, so the segments are written by hand. Like a real track, it
/// begins at its first cue.
pub fn pgs_box(dir: &Path, from_ms: u32, to_ms: u32) -> PathBuf {
	let (x, y, width, height) = (100_u16, 180_u16, 120_u16, 30_u16);
	let segment = |pts_ms: u32, kind: u8, data: &[u8]| {
		let mut bytes = b"PG".to_vec();
		bytes.extend((pts_ms * 90).to_be_bytes());
		bytes.extend(0_u32.to_be_bytes());
		bytes.push(kind);
		bytes.extend(u16::try_from(data.len()).unwrap().to_be_bytes());
		bytes.extend(data);
		bytes
	};
	let composition = |number: u16, objects: &[u8]| {
		let mut pcs = [320_u16.to_be_bytes(), 240_u16.to_be_bytes()].concat();
		pcs.push(0x10);
		pcs.extend(number.to_be_bytes());
		pcs.extend([if objects.is_empty() { 0x00 } else { 0x80 }, 0, 0]);
		pcs.push(u8::from(!objects.is_empty()));
		pcs.extend(objects);
		pcs
	};
	let window = [
		&[1, 0][..],
		&x.to_be_bytes(),
		&y.to_be_bytes(),
		&width.to_be_bytes(),
		&height.to_be_bytes(),
	]
	.concat();

	let object = [&[0, 0, 0, 0][..], &x.to_be_bytes(), &y.to_be_bytes()].concat();
	let palette = [0, 0, 1, 235, 128, 128, 255];
	let [high, low] = width.to_be_bytes();
	let rle = [0x00, 0xC0 | high, low, 0x01, 0x00, 0x00].repeat(usize::from(height));
	let length = u32::try_from(rle.len() + 4).unwrap().to_be_bytes();
	let bitmap = [
		&[0, 0, 0, 0xC0][..],
		&length[1..],
		&width.to_be_bytes(),
		&height.to_be_bytes(),
		&rle,
	]
	.concat();

	let clear = |pts_ms: u32, number: u16| {
		[
			segment(pts_ms, 0x16, &composition(number, &[])),
			segment(pts_ms, 0x17, &window),
			segment(pts_ms, 0x80, &[]),
		]
		.concat()
	};
	let shown = [
		segment(from_ms, 0x16, &composition(0, &object)),
		segment(from_ms, 0x17, &window),
		segment(from_ms, 0x14, &palette),
		segment(from_ms, 0x15, &bitmap),
		segment(from_ms, 0x80, &[]),
	]
	.concat();

	let path = dir.join(format!("box-{from_ms}-{to_ms}.sup"));
	std::fs::write(&path, [shown, clear(to_ms, 1)].concat()).unwrap();
	path
}
