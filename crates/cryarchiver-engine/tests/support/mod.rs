#![allow(dead_code)]

pub mod heif;
pub mod tiff;
pub mod ultra_hdr;

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

/// Where `still_home_video` moves, as a `crop` argument.
pub const HOME_VIDEO_INSET: &str = "260:208:200:200";

/// Like a tripod home video: a still, noisy picture with a small moving inset. idet at the analysis threshold tells
/// the field order of only about a third of its frames and leaves the rest undetermined. `scan` is `tff` or `bff`
/// for 25i, `50p` for the pictures its fields come from, and anything else for 25p in an interlaced container.
pub fn still_home_video(dir: &Path, name: &str, scan: &str) -> PathBuf {
	let (rate, fields) = match scan {
		"tff" | "bff" => (50, format!("interlace=scan={scan},setfield={scan}")),
		"50p" => (50, "setfield=prog".to_owned()),
		_ => (25, "setfield=tff".to_owned()),
	};
	let graph = format!(
		"testsrc2=s=720x576:r={rate}:d=0.02,loop=loop=-1:size=1,setpts=N/{rate}/TB,trim=duration=4,\
		 noise=alls=6:allf=t[still];testsrc2=s=260x208:r={rate}:d=4[inset];[still][inset]overlay=x=200:y=200,{fields}"
	);
	let path = dir.join(name);

	ffmpeg(&[
		"-filter_complex",
		&graph,
		"-c:v",
		"mpeg2video",
		"-b:v",
		"8M",
		"-flags",
		"+ilme+ildct",
		path.to_str().unwrap(),
	]);
	path
}

/// `still_home_video` interlaced as `scan` (`tff` or `bff`) but stored as progressive H.264, so that nothing but the
/// picture tells its fields.
pub fn unflagged_home_video(dir: &Path, name: &str, scan: &str) -> PathBuf {
	let graph = format!(
		"testsrc2=s=720x576:r=50:d=0.02,loop=loop=-1:size=1,setpts=N/50/TB,trim=duration=4,noise=alls=6:allf=t[still];\
		 testsrc2=s=260x208:r=50:d=4[inset];[still][inset]overlay=x=200:y=200,interlace=scan={scan},setfield=prog"
	);
	let path = dir.join(name);

	ffmpeg(&[
		"-filter_complex",
		&graph,
		"-c:v",
		"libx264",
		"-crf",
		"18",
		path.to_str().unwrap(),
	]);
	path
}

/// idet's single-frame verdicts inside `region` (a `crop` argument): (TFF + BFF, progressive, all frames). Still
/// areas are undetermined either way, and the multi-frame verdicts carry one guess across them.
pub fn idet_verdicts(path: &Path, region: &str) -> (u64, u64, u64) {
	let filters = format!("crop={region},idet");
	let out = Command::new("ffmpeg")
		.args(["-hide_banner", "-nostdin", "-i"])
		.arg(path)
		.args(["-map", "0:v:0", "-vf", &filters, "-f", "null", "-"])
		.output()
		.unwrap();
	let stderr = String::from_utf8_lossy(&out.stderr);
	let line = stderr
		.lines()
		.rfind(|line| line.contains("Single frame detection:"))
		.unwrap_or_else(|| panic!("{stderr}"));
	let counts: Vec<u64> = line
		.split("detection:")
		.nth(1)
		.unwrap()
		.split(':')
		.skip(1)
		.filter_map(|part| part.split_whitespace().next()?.parse().ok())
		.collect();

	(counts[0] + counts[1], counts[2], counts.iter().sum())
}

/// The PSNR of `path` after `filters` against `truth` inside `region`, pairing frames by their position.
pub fn psnr_in(path: &Path, filters: &str, truth: &Path, region: &str) -> f64 {
	let by_position = format!("format=yuv420p,crop={region},settb=1/1000,setpts=N");
	let graph = format!("[0:v:0]{filters}{by_position}[measured];[1:v:0]{by_position}[truth];[measured][truth]psnr");
	let out = Command::new("ffmpeg")
		.args(["-hide_banner", "-nostdin", "-i"])
		.arg(path)
		.arg("-i")
		.arg(truth)
		.args(["-filter_complex", &graph, "-f", "null", "-"])
		.output()
		.unwrap();
	let stderr = String::from_utf8_lossy(&out.stderr);

	stderr
		.lines()
		.rev()
		.find_map(|line| line.split(" average:").nth(1)?.split_whitespace().next()?.parse().ok())
		.unwrap_or_else(|| panic!("{stderr}"))
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
			marked_frame: None,
		},
		subtitles,
		codec,
	)
}

/// `black_clip` lasting `seconds`, without a second angle.
pub fn black_clip_of(dir: &Path, name: &str, seconds: u32, subtitles: &Path, codec: &str) -> PathBuf {
	subtitled_clip(
		dir,
		name,
		&Picture {
			source: "color=c=black",
			seconds,
			codec: &["-c:v", "mpeg4"],
			second_angle: false,
			marked_frame: None,
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
	/// A frame that gets a white 40×40 square in its top-left corner (`MARKER`), to find it again after a cut.
	pub marked_frame: Option<u32>,
}

pub const MARKER: &str = "crop=40:40:0:0";

/// A 320×240, 25 fps clip with a tone and `subtitles` muxed with `codec`. The subtitles keep their own times
/// (`-copyts`), so a track whose first cue is late still starts late.
pub fn subtitled_clip(dir: &Path, name: &str, picture: &Picture<'_>, subtitles: &Path, codec: &str) -> PathBuf {
	let clip = dir.join(name);
	let joint = if picture.source.contains('=') { ':' } else { '=' };
	let marker = picture.marked_frame.map_or_else(String::new, |frame| {
		format!(",drawbox=x=0:y=0:w=40:h=40:c=white:t=fill:enable='eq(n\\,{frame})'")
	});
	let video = format!("{}{joint}s=320x240:r=25:d={}{marker}", picture.source, picture.seconds);
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

/// `source` rewritten into `dir/name` by ffmpeg, with `before` as input options and `after` as output options.
pub fn converted(dir: &Path, name: &str, before: &[&str], source: &Path, after: &[&str]) -> PathBuf {
	let out = dir.join(name);
	let mut args = before.to_vec();
	args.extend(["-i", source.to_str().unwrap()]);
	args.extend(after);
	args.push(out.to_str().unwrap());
	ffmpeg(&args);
	out
}

/// A variable-rate MPEG-4 video declared at 10 fps: every 1.5 s, 10 frames 50 ms apart, then 10 frames 100 ms apart.
pub fn variable_rate(dir: &Path, name: &str, seconds: u32) -> PathBuf {
	let graph = format!(
		"testsrc2=s=160x120:r=10:d={seconds},settb=1/1000,\
		 setpts='floor(N/20)*1500+if(lt(mod(N,20),10),mod(N,20)*50,500+(mod(N,20)-10)*100)'"
	);
	let path = dir.join(name);

	ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		&graph,
		"-c:v",
		"mpeg4",
		"-q:v",
		"2",
		"-enc_time_base:v",
		"1/1000",
		"-fps_mode",
		"passthrough",
		path.to_str().unwrap(),
	]);
	path
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
	lit_span_in(path, stream, "null")
}

/// `lit_span` for the part of the picture that the `region` filter (such as `MARKER` or `BELOW_MARKER`) crops out.
pub fn lit_span_in(path: &Path, stream: usize, region: &str) -> Option<(f64, f64)> {
	let out = Command::new("ffmpeg")
		.args(["-v", "error", "-i"])
		.arg(path)
		.arg("-map")
		.arg(format!("0:v:{stream}"))
		.arg("-vf")
		.arg(format!(
			"{region},format=gray,signalstats,metadata=mode=print:key=lavfi.signalstats.YMAX:file=-"
		))
		.args(["-f", "null", "-"])
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

/// The average 8-bit Y, U and V of `region` (such as `BOX_TOP`) in the first frame where anything is drawn there.
pub fn colour_in(path: &Path, region: &str) -> Option<[f64; 3]> {
	let out = Command::new("ffmpeg")
		.args(["-v", "error", "-i"])
		.arg(path)
		.args(["-map", "0:v:0", "-vf"])
		.arg(format!(
			"{region},format=yuv420p,signalstats,metadata=mode=print:file=-"
		))
		.args(["-f", "null", "-"])
		.output()
		.unwrap();

	let mut averages = [0.0; 3];
	for line in String::from_utf8_lossy(&out.stdout).lines() {
		let Some((key, value)) = line
			.strip_prefix("lavfi.signalstats.")
			.and_then(|line| line.split_once('='))
		else {
			continue;
		};
		let plane = match key {
			"YAVG" => 0,
			"UAVG" => 1,
			"VAVG" => 2,
			_ => continue,
		};

		averages[plane] = value.parse().unwrap();
		if plane == 2 && averages[0] > 32.0 {
			return Some(averages);
		}
	}
	None
}

/// When `MARKER` is first lit and when the first audio track first clicks, on the file's own timestamps.
pub fn marker_and_click(path: &Path) -> (f64, f64) {
	let printed = |stream: &str, filter: &str| {
		let out = Command::new("ffmpeg")
			.args(["-v", "error", "-copyts", "-i"])
			.arg(path)
			.args([
				"-map",
				stream,
				if stream.contains(":v") { "-vf" } else { "-af" },
				filter,
			])
			.args(["-f", "null", "-"])
			.output()
			.unwrap();
		String::from_utf8_lossy(&out.stdout).into_owned()
	};
	let first = |text: &str, key: &str, lit: fn(f64) -> bool| {
		let mut time = 0.0;
		for line in text.lines() {
			if let Some(pts) = line
				.split_whitespace()
				.find_map(|field| field.strip_prefix("pts_time:"))
			{
				time = pts.parse().unwrap();
			} else if let Some(value) = line.strip_prefix(key)
				&& value.parse::<f64>().is_ok_and(lit)
			{
				return time;
			}
		}
		panic!("{key} never lit in {}", path.display())
	};

	let marker = printed(
		"0:v:0",
		&format!("{MARKER},format=gray,signalstats,metadata=mode=print:key=lavfi.signalstats.YMAX:file=-"),
	);
	let click = printed(
		"0:a:0",
		"aresample=48000,asetnsamples=48,astats=metadata=1:reset=1,\
		 ametadata=mode=print:key=lavfi.astats.Overall.Peak_level:file=-",
	);
	(
		first(&marker, "lavfi.signalstats.YMAX=", |ymax| ymax > 128.0),
		first(&click, "lavfi.astats.Overall.Peak_level=", |peak| peak > -6.0),
	)
}

/// When the first audio track is first louder than -30 dB, on the file's own timestamps.
pub fn first_sound(path: &Path) -> f64 {
	let out = Command::new("ffmpeg")
		.args(["-v", "error", "-copyts", "-i"])
		.arg(path)
		.args([
			"-map",
			"0:a:0",
			"-af",
			"aresample=48000,asetnsamples=48,astats=metadata=1:reset=1,\
			 ametadata=mode=print:key=lavfi.astats.Overall.Peak_level:file=-",
			"-f",
			"null",
			"-",
		])
		.output()
		.unwrap();

	let mut time = 0.0;
	for line in String::from_utf8_lossy(&out.stdout).lines() {
		if let Some(pts) = line
			.split_whitespace()
			.find_map(|field| field.strip_prefix("pts_time:"))
		{
			time = pts.parse().unwrap();
		} else if let Some(value) = line.strip_prefix("lavfi.astats.Overall.Peak_level=")
			&& value.parse::<f64>().is_ok_and(|peak| peak > -30.0)
		{
			return time;
		}
	}
	panic!("no sound in {}", path.display())
}

/// The picture below `MARKER`'s rows, where subtitles are drawn.
pub const BELOW_MARKER: &str = "crop=320:200:0:40";

/// A PGS (bitmap) subtitle file for a 320×240 picture: a white 120×30 box shown from `from_ms` until `to_ms`.
/// ffmpeg can't turn text subtitles into bitmaps, so the segments are written by hand. Like a real track, it
/// begins at its first cue.
pub fn pgs_box(dir: &Path, from_ms: u32, to_ms: u32) -> PathBuf {
	pgs_bands(dir, from_ms, to_ms, &[WHITE])
}

pub const WHITE: [u8; 3] = [235, 128, 128];

/// The box at x 100..220, y 180..210.
pub const BOX_TOP: &str = "crop=100:10:110:182";
pub const BOX_BOTTOM: &str = "crop=100:10:110:198";

/// `pgs_box` with the box's rows split into equal horizontal bands, one per `colours` entry (Y, Cb, Cr) from the
/// top.
pub fn pgs_bands(dir: &Path, from_ms: u32, to_ms: u32, colours: &[[u8; 3]]) -> PathBuf {
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
	let mut palette = vec![0, 0];
	for (entry, [luma, cb, cr]) in (1_u8..).zip(colours) {
		palette.extend([entry, *luma, *cr, *cb, 255]);
	}
	let [high, low] = width.to_be_bytes();
	let band = usize::from(height) / colours.len();
	let rle: Vec<u8> = (0..usize::from(height))
		.flat_map(|row| {
			let entry = u8::try_from((row / band).min(colours.len() - 1) + 1).unwrap();
			[0x00, 0xC0 | high, low, entry, 0x00, 0x00]
		})
		.collect();
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

	let shades = colours
		.iter()
		.flatten()
		.map(u8::to_string)
		.collect::<Vec<_>>()
		.join("-");
	let path = dir.join(format!("box-{from_ms}-{to_ms}-{shades}.sup"));
	std::fs::write(&path, [shown, clear(to_ms, 1)].concat()).unwrap();
	path
}
