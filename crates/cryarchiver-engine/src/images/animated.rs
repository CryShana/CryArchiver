use std::io::{Cursor, Read};
use std::num::NonZeroU32;
use std::path::Path;

use image::codecs::gif::GifDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::metadata::LoopCount;
use image::{AnimationDecoder, DynamicImage, Frames};
use tokio_util::sync::CancellationToken;

use crate::container::isobmff;
use crate::images::decode::IMAGE_DECODE_ERRORS;
use crate::images::ssim;
use crate::metadata::{ImageFormat, gif, sniff};
use crate::outcome::{ErrorKind, JobError};
use crate::preset::Speed;
use crate::probe::{self, MediaInfo};
use crate::process::{self, STALL_TIMEOUT, StdoutMode, ToolCommand, blocking};
use crate::toolchain::Toolchain;
use crate::video::run_ffmpeg;

pub const MAX_SAMPLED_FRAMES: u32 = 10;
pub const MAX_FRAMES: u32 = 100_000;
pub const NEEDS_LIBAOM: &str = "an animated AVIF needs ffmpeg with libaom-av1";
const GIF_DEFAULT_DELAY_MS: u32 = 100;
const DELAY_TOLERANCE_MS: u32 = 1;
const HEAD_BYTES: u64 = 16;
const TRANSPARENT_FRAMES: &str = r"format=rgba,alphaextract,signalstats,metadata=mode=select:key=lavfi.signalstats.YMIN:value=255:function=expr:expr='lt(VALUE1\,VALUE2)'";
const ALPHA_SPLIT: &str = "split[c][a];[c]format=yuv444p10le[color];[a]alphaextract,format=gray10le[alpha]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timeline {
	pub frames: u32,
	pub delays_ms: Vec<u32>,
	pub plays: Option<u32>,
	pub width: u32,
	pub height: u32,
}

/// The YUV matrix an animated image's RGB frames are encoded with: the one ravif uses for stills.
pub const IMAGE_MATRIX: &str = "smpte170m";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Animation {
	pub timeline: Timeline,
	pub alpha: bool,
	/// The output size, when `--resize` scales the frames.
	pub size: Option<(u32, u32)>,
	/// The YUV matrix the RGB frames are encoded with and tagged as, as `scale` names it.
	pub matrix: String,
}

impl Animation {
	/// The scale for the encode and for the frames it is scored against: the same filter on the same 16-bit RGB
	/// frames, so both are scaled alike. Transparent frames are scaled premultiplied, or the colour hidden under
	/// transparent pixels would darken the partly transparent edges.
	fn scale(&self) -> Option<String> {
		let (width, height) = self.size?;
		let scale = format!("scale=w={width}:h={height}:flags=lanczos+accurate_rnd");

		Some(if self.alpha {
			format!("format=rgba64le,premultiply=inplace=1,{scale},unpremultiply=inplace=1")
		} else {
			format!("format=rgba64le,{scale}")
		})
	}

	/// The frames, scaled if asked, in `pix_fmt` by the animation's matrix and tagged with that matrix by name:
	/// `scale` tags `smpte170m` as the same matrix's other name, `bt470bg`.
	fn yuv(&self, pix_fmt: &str) -> String {
		let scaled = self.scale().map(|scale| format!("{scale},")).unwrap_or_default();
		let matrix = &self.matrix;

		format!("{scaled}scale=out_color_matrix={matrix}:out_range=tv,format={pix_fmt},setparams=colorspace={matrix}")
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanError {
	Unreadable(String),
	TooManyFrames,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnimatedSettings {
	pub crf: u8,
	pub cpu_used: u8,
}

pub fn scan(bytes: &[u8]) -> Result<Option<Timeline>, ScanError> {
	let unreadable = |err: image::ImageError| ScanError::Unreadable(format!("the animation is unreadable: {err}"));
	let (loop_count, frames, counts_repeats) = match sniff(bytes) {
		ImageFormat::Gif => {
			let decoder = GifDecoder::new(Cursor::new(bytes)).map_err(unreadable)?;
			if gif::declares_looping(bytes) {
				(decoder.loop_count(), decoder.into_frames(), true)
			} else {
				(LoopCount::Finite(NonZeroU32::MIN), decoder.into_frames(), false)
			}
		}
		ImageFormat::Png => {
			let decoder = PngDecoder::new(Cursor::new(bytes)).map_err(unreadable)?;
			if !decoder.is_apng().map_err(unreadable)? {
				return Ok(None);
			}
			let decoder = decoder.apng().map_err(unreadable)?;
			(decoder.loop_count(), decoder.into_frames(), false)
		}
		ImageFormat::WebP => {
			let decoder = WebPDecoder::new(Cursor::new(bytes)).map_err(unreadable)?;
			if !decoder.has_animation() {
				return Ok(None);
			}
			(decoder.loop_count(), decoder.into_frames(), false)
		}
		_ => return Ok(None),
	};

	let plays = match loop_count {
		LoopCount::Infinite => None,
		LoopCount::Finite(count) if counts_repeats => Some(count.get().saturating_add(1)),
		LoopCount::Finite(count) => Some(count.get()),
	};
	walk_frames(frames, plays).map(Some)
}

fn walk_frames(frames: Frames<'_>, plays: Option<u32>) -> Result<Timeline, ScanError> {
	let mut timeline = Timeline {
		frames: 0,
		delays_ms: Vec::new(),
		plays,
		width: 0,
		height: 0,
	};

	for frame in frames {
		if timeline.frames == MAX_FRAMES {
			return Err(ScanError::TooManyFrames);
		}

		let frame = frame
			.map_err(|err| ScanError::Unreadable(format!("frame {} is unreadable: {err}", timeline.frames + 1)))?;
		let (numer, denom) = frame.delay().numer_denom_ms();
		timeline
			.delays_ms
			.push((numer + denom / 2).checked_div(denom).unwrap_or(0));
		(timeline.width, timeline.height) = frame.buffer().dimensions();
		timeline.frames += 1;
	}
	Ok(timeline)
}

pub fn too_many_frames(frames: u64) -> Option<String> {
	(frames > u64::from(MAX_FRAMES)).then(|| {
		format!(
			"the clip has {frames} frames, more than the {MAX_FRAMES} an animation can have; use --ss/--to to make a \
			 shorter clip"
		)
	})
}

/// The timeline of frames stored as a video, from ffprobe's `pts_time,duration_time` of each packet: each frame
/// lasts until the next one's time, and the last one its own duration, or the one before it when it has none.
pub fn video_timeline(packets: &str, (width, height): (u32, u32)) -> Timeline {
	let mut frames: Vec<(i64, Option<i64>)> = packets
		.lines()
		.filter_map(|line| {
			let mut fields = line.split(',');
			let pts = fields.next()?.trim().parse::<f64>().ok()?;
			let duration = fields.next().and_then(|field| field.trim().parse::<f64>().ok());
			Some((whole_ms(pts), duration.map(whole_ms)))
		})
		.collect();
	frames.sort_by_key(|&(pts, _)| pts);

	let mut delays_ms: Vec<u32> = frames
		.windows(2)
		.map(|pair| u32::try_from(pair[1].0 - pair[0].0).unwrap_or(0))
		.collect();
	if let Some(&(_, duration)) = frames.last() {
		let last = duration
			.and_then(|duration| u32::try_from(duration).ok())
			.filter(|&duration| duration > 0)
			.or(delays_ms.last().copied())
			.unwrap_or(0);
		delays_ms.push(last);
	}

	Timeline {
		frames: u32::try_from(frames.len()).unwrap_or(u32::MAX),
		delays_ms,
		plays: None,
		width,
		height,
	}
}

pub async fn read_video_timeline(
	tools: &Toolchain,
	frames: &Path,
	size: (u32, u32),
	cancel: &CancellationToken,
) -> Result<Timeline, JobError> {
	let mut cmd = ToolCommand::new(&tools.ffprobe, STALL_TIMEOUT);
	cmd.args([
		"-v",
		"error",
		"-select_streams",
		"0",
		"-show_entries",
		"packet=pts_time,duration_time",
		"-of",
		"csv=p=0",
		"-i",
	])
	.arg(process::tool_path(frames));

	let out = process::run(&cmd, StdoutMode::Capture, |_| {}, cancel).await?;
	if !out.status.success() {
		return Err(ErrorKind::EncodeFailed(format!(
			"ffprobe could not read the prepared frames: {}",
			process::failure_reason(&out.stderr_tail)
		))
		.into());
	}
	Ok(video_timeline(&String::from_utf8_lossy(&out.stdout), size))
}

pub async fn has_alpha(
	tools: &Toolchain,
	source: &Path,
	threads: usize,
	cancel: &CancellationToken,
) -> Result<bool, JobError> {
	let cmd = has_alpha_command(&tools.ffmpeg, source, threads);

	let out = run_ffmpeg(&cmd, |_| {}, cancel).await?;
	Ok(out
		.last_progress
		.and_then(|progress| progress.frame)
		.is_some_and(|frames| frames > 0))
}

fn has_alpha_command(ffmpeg: &Path, source: &Path, threads: usize) -> ToolCommand {
	let mut cmd = ToolCommand::new(ffmpeg, STALL_TIMEOUT);
	cmd.args(["-hide_banner", "-nostdin", "-v", "error"])
		.threads(Some(threads))
		.arg("-i")
		.arg(process::tool_path(source))
		.args(["-vf", TRANSPARENT_FRAMES, "-fps_mode", "passthrough", "-f", "null", "-"]);
	cmd
}

pub fn plan_animation(speed: Speed) -> AnimatedSettings {
	let crf = match speed {
		Speed::Fast => 16,
		Speed::Moderate => 10,
		Speed::Slow => 6,
	};
	AnimatedSettings { crf, cpu_used: 6 }
}

pub fn encode_command(
	ffmpeg: &Path,
	source: &Path,
	output: &Path,
	animation: &Animation,
	settings: AnimatedSettings,
	threads: usize,
) -> ToolCommand {
	let mut cmd = ToolCommand::new(ffmpeg, STALL_TIMEOUT);
	cmd.count_lines_containing(IMAGE_DECODE_ERRORS);
	cmd.args(["-hide_banner", "-nostdin", "-y", "-v", "error"])
		.threads(Some(threads))
		.arg("-i")
		.arg(process::tool_path(source));

	if animation.alpha {
		cmd.arg("-filter_complex")
			.arg(format!("[0:v]{},{ALPHA_SPLIT}", animation.yuv("yuva444p10le")))
			.args(["-map", "[color]", "-map", "[alpha]", "-fps_mode:v"]);
	} else {
		cmd.arg("-vf").arg(animation.yuv("yuv444p10le")).arg("-fps_mode");
	}
	cmd.args(["passthrough", "-enc_time_base:v", "demux", "-c:v", "libaom-av1", "-crf"])
		.arg(settings.crf.to_string())
		.arg("-cpu-used")
		.arg(settings.cpu_used.to_string())
		.args(["-row-mt", "1", "-threads"])
		.arg(threads.max(1).to_string());

	let timeline = &animation.timeline;
	let last = timeline.frames.saturating_sub(1);
	let last_delay_s = f64::from(timeline.delays_ms.last().copied().unwrap_or(0)) / 1000.0;
	cmd.arg("-bsf:v")
		.arg(format!(
			r"setts=duration=if(eq(N\,{last})\,{last_delay_s}/TB\,DURATION)"
		))
		.arg("-loop")
		.arg(timeline.plays.unwrap_or(0).to_string())
		.args(["-f", "avif"])
		.arg(output);
	cmd
}

pub fn expected_delays(timeline: &Timeline, gif: bool) -> Vec<u32> {
	timeline
		.delays_ms
		.iter()
		.map(|&delay| if gif && delay == 0 { GIF_DEFAULT_DELAY_MS } else { delay })
		.collect()
}

pub fn sampled_frames(frames: u32) -> Vec<u32> {
	let count = frames.min(MAX_SAMPLED_FRAMES);
	(0..count)
		.map(|index| {
			let frame = u64::from(index) * u64::from(frames) / u64::from(count);
			u32::try_from(frame).expect("a sampled index is below the frame count")
		})
		.collect()
}

pub fn plays(bytes: &[u8]) -> Option<Option<u64>> {
	let moov = isobmff::find_box(bytes, *b"moov").ok()??;
	let trak = isobmff::find_box(moov.body, *b"trak").ok()??;
	let tkhd = isobmff::find_box(trak.body, *b"tkhd").ok()??;
	let edts = isobmff::find_box(trak.body, *b"edts").ok()??;
	let elst = isobmff::find_box(edts.body, *b"elst").ok()??;

	let track_duration = match tkhd.body.first()? {
		0 => u64::from(u32::from_be_bytes(tkhd.body.get(20..24)?.try_into().ok()?)),
		_ => u64::from_be_bytes(tkhd.body.get(28..36)?.try_into().ok()?),
	};
	let segment_duration = match elst.body.first()? {
		0 => u64::from(u32::from_be_bytes(elst.body.get(8..12)?.try_into().ok()?)),
		_ => u64::from_be_bytes(elst.body.get(8..16)?.try_into().ok()?),
	};
	if segment_duration == 0 {
		return None;
	}

	let repeats = elst.body.get(3).is_some_and(|flags| flags & 1 == 1);
	let count = track_duration / segment_duration;
	Some((!repeats || count < u64::from(u32::MAX)).then_some(count))
}

pub async fn verify_animation(
	tools: &Toolchain,
	source: &Path,
	output: &Path,
	animation: &Animation,
	threads: usize,
	cancel: &CancellationToken,
) -> Result<f64, JobError> {
	let info = probe::probe(tools, output, cancel)
		.await
		.map_err(|err| JobError::verification(format!("ffprobe could not read the output: {err}")))?;
	let tracks = sequence_tracks(&info, animation)?;
	check_delays(tools, source, output, tracks[0], &animation.timeline, cancel).await?;

	let bytes = std::fs::read(output).map_err(|err| JobError::verification(err.to_string()))?;
	let timeline = &animation.timeline;
	if plays(&bytes) != Some(timeline.plays.map(u64::from)) {
		return Err(JobError::verification(format!(
			"the loop count is {:?}, not {:?}",
			plays(&bytes),
			timeline.plays
		)));
	}

	let frames = sampled_frames(timeline.frames);
	let reference = decode_frames(
		&decode_frames_command(
			&tools.ffmpeg,
			source,
			&frames,
			Sampled::Source(animation.scale()),
			threads,
		),
		cancel,
	)
	.await?;
	let decoded = decode_frames(
		&decode_frames_command(&tools.ffmpeg, output, &frames, Sampled::Output(&tracks), threads),
		cancel,
	)
	.await?;
	if reference.len() != frames.len() || decoded.len() != frames.len() {
		return Err(JobError::verification("fewer frames decoded than sampled"));
	}

	blocking(cancel, move || {
		reference
			.iter()
			.zip(&decoded)
			.map(|(reference, decoded)| ssim::score(reference, decoded))
			.try_fold(f64::INFINITY, |worst, score| score.map(|score| worst.min(score)))
	})
	.await?
	.map_err(JobError::verification)
}

fn sequence_tracks(info: &MediaInfo, animation: &Animation) -> Result<Vec<u32>, JobError> {
	let tracks: Vec<u32> = info
		.streams
		.iter()
		.filter(|stream| stream.is_video() && stream.nb_frames.is_some_and(|frames| frames > 1))
		.map(|stream| stream.index)
		.collect();
	let expected = if animation.alpha { 2 } else { 1 };
	if tracks.len() != expected {
		return Err(JobError::verification(format!(
			"expected {expected} animated tracks, found {}",
			tracks.len()
		)));
	}

	let timeline = &animation.timeline;
	let color = info
		.streams
		.iter()
		.find(|stream| stream.index == tracks[0])
		.expect("listed above");
	if color.nb_frames != Some(u64::from(timeline.frames)) {
		return Err(JobError::verification(format!(
			"the output has {:?} frames, not {}",
			color.nb_frames, timeline.frames
		)));
	}
	let (width, height) = animation.size.unwrap_or((timeline.width, timeline.height));
	if (color.width, color.height) != (Some(width), Some(height)) {
		return Err(JobError::verification(format!(
			"the output frames are not {width}×{height}"
		)));
	}
	Ok(tracks)
}

async fn check_delays(
	tools: &Toolchain,
	source: &Path,
	output: &Path,
	track: u32,
	timeline: &Timeline,
	cancel: &CancellationToken,
) -> Result<(), JobError> {
	let mut cmd = ToolCommand::new(&tools.ffprobe, STALL_TIMEOUT);
	cmd.args(["-v", "error", "-select_streams"])
		.arg(track.to_string())
		.args(["-show_entries", "packet=duration_time", "-of", "csv=p=0", "-i"])
		.arg(process::tool_path(output));
	let out = process::run(&cmd, StdoutMode::Capture, |_| {}, cancel).await?;

	let delays: Vec<u32> = String::from_utf8_lossy(&out.stdout)
		.lines()
		.filter_map(|line| line.trim().parse::<f64>().ok())
		.map(milliseconds)
		.collect();
	let gif = head(source).is_ok_and(|bytes| sniff(&bytes) == ImageFormat::Gif);
	let expected = expected_delays(timeline, gif);

	let matches = delays.len() == expected.len()
		&& delays
			.iter()
			.zip(&expected)
			.all(|(found, wanted)| found.abs_diff(*wanted) <= DELAY_TOLERANCE_MS);
	if matches {
		Ok(())
	} else {
		Err(JobError::verification("the frame timing differs from the source"))
	}
}

/// Enough of a file to tell a GIF.
fn head(path: &Path) -> std::io::Result<Vec<u8>> {
	let mut head = Vec::new();
	std::fs::File::open(path)?.take(HEAD_BYTES).read_to_end(&mut head)?;
	Ok(head)
}

enum Sampled<'a> {
	/// The source, scaled by the encode's scale, if any.
	Source(Option<String>),
	/// The output's colour track, and its alpha track after it, if any.
	Output(&'a [u32]),
}

async fn decode_frames(cmd: &ToolCommand, cancel: &CancellationToken) -> Result<Vec<DynamicImage>, JobError> {
	let out = process::run(cmd, StdoutMode::Capture, |_| {}, cancel).await?;
	if !out.status.success() {
		return Err(JobError::verification(format!(
			"ffmpeg could not decode sampled frames: {}",
			process::failure_reason(&out.stderr_tail)
		)));
	}
	blocking(cancel, move || split_pams(&out.stdout))
		.await?
		.map_err(JobError::verification)
}

/// Decodes `frames` of `input` as PAM images on stdout. The select still decodes every frame up to the last one.
fn decode_frames_command(
	ffmpeg: &Path,
	input: &Path,
	frames: &[u32],
	sampled: Sampled<'_>,
	threads: usize,
) -> ToolCommand {
	let select = frames
		.iter()
		.map(|frame| format!(r"eq(n\,{frame})"))
		.collect::<Vec<_>>()
		.join("+");
	let mut cmd = ToolCommand::new(ffmpeg, STALL_TIMEOUT);
	cmd.args(["-hide_banner", "-nostdin", "-v", "error"])
		.threads(Some(threads))
		.arg("-i")
		.arg(process::tool_path(input));

	match sampled {
		Sampled::Output([color, alpha]) => {
			cmd.arg("-filter_complex").arg(format!(
				"[0:{color}][0:{alpha}]alphamerge,select='{select}',format=rgba64be"
			));
		}
		Sampled::Output(tracks) => {
			cmd.arg("-map")
				.arg(format!("0:{}", tracks[0]))
				.arg("-vf")
				.arg(format!("select='{select}',format=rgba64be"));
		}
		Sampled::Source(scale) => {
			let scaled = scale.map(|scale| format!("{scale},")).unwrap_or_default();
			cmd.arg("-vf").arg(format!("select='{select}',{scaled}format=rgba"));
		}
	}
	cmd.args(["-fps_mode", "passthrough", "-c:v", "pam", "-f", "image2pipe", "-"]);
	cmd
}

pub fn split_pams(bytes: &[u8]) -> Result<Vec<DynamicImage>, String> {
	let mut images = Vec::new();
	let mut at = 0;

	while at < bytes.len() {
		let header_end = bytes[at..]
			.windows(7)
			.position(|window| window == b"ENDHDR\n")
			.map(|offset| at + offset + 7)
			.ok_or("a PAM header is cut off")?;
		let header = String::from_utf8_lossy(&bytes[at..header_end]);
		let field = |name: &str| {
			header
				.lines()
				.find_map(|line| line.strip_prefix(name)?.trim().parse::<usize>().ok())
				.ok_or_else(|| format!("the PAM header has no {name}"))
		};
		let sample_bytes = if field("MAXVAL")? > 255 { 2 } else { 1 };
		let end = header_end + field("WIDTH")? * field("HEIGHT")? * field("DEPTH")? * sample_bytes;

		let frame = bytes.get(at..end).ok_or("a PAM frame is cut off")?;
		images
			.push(image::load_from_memory_with_format(frame, image::ImageFormat::Pnm).map_err(|err| err.to_string())?);
		at = end;
	}
	Ok(images)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn milliseconds(seconds: f64) -> u32 {
	(seconds * 1000.0).round().max(0.0) as u32
}

#[allow(clippy::cast_possible_truncation)]
fn whole_ms(seconds: f64) -> i64 {
	(seconds * 1000.0).round() as i64
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::video::joined;

	const BUDGETED: &str = "-threads 3 -filter_threads 3 -filter_complex_threads 3 -i ";

	#[test]
	fn the_alpha_scan_decodes_on_the_thread_budget() {
		let args = joined(&has_alpha_command(Path::new("ffmpeg"), Path::new("a.gif"), 3));

		assert!(args.contains(BUDGETED), "{args}");
	}

	#[test]
	fn the_sampled_frames_of_both_files_are_decoded_on_the_thread_budget() {
		let ffmpeg = Path::new("ffmpeg");
		let frames = [0, 4, 9];

		let source = joined(&decode_frames_command(
			ffmpeg,
			Path::new("a.gif"),
			&frames,
			Sampled::Source(None),
			3,
		));
		let output = joined(&decode_frames_command(
			ffmpeg,
			Path::new("out.avif"),
			&frames,
			Sampled::Output(&[0]),
			3,
		));
		let with_alpha = joined(&decode_frames_command(
			ffmpeg,
			Path::new("out.avif"),
			&frames,
			Sampled::Output(&[0, 1]),
			3,
		));

		for args in [&source, &output, &with_alpha] {
			assert!(args.contains(BUDGETED), "{args}");
		}
	}
}
