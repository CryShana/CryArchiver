use std::io::Cursor;
use std::path::Path;

use image::codecs::gif::GifDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::metadata::LoopCount;
use image::{AnimationDecoder, DynamicImage, Frames};
use tokio_util::sync::CancellationToken;

use crate::container::isobmff;
use crate::images::blocking;
use crate::images::decode::IMAGE_DECODE_ERRORS;
use crate::images::ssim;
use crate::metadata::{ImageFormat, sniff};
use crate::outcome::{ErrorKind, JobError, KeepReason};
use crate::preset::Speed;
use crate::probe::{self, MediaInfo};
use crate::process::{self, STALL_TIMEOUT, StdoutMode, ToolCommand};
use crate::toolchain::Toolchain;
use crate::video::run_ffmpeg;

pub const MAX_SAMPLED_FRAMES: u32 = 10;
const MAX_FRAMES: u32 = 100_000;
const GIF_DEFAULT_DELAY_MS: u32 = 100;
const DELAY_TOLERANCE_MS: u32 = 1;
const TRANSPARENT_FRAMES: &str = r"format=rgba,alphaextract,signalstats,metadata=mode=select:key=lavfi.signalstats.YMIN:value=255:function=expr:expr='lt(VALUE1\,VALUE2)'";
const ALPHA_GRAPH: &str =
	"[0:v]format=yuva444p10le,split[c][a];[c]format=yuv444p10le[color];[a]alphaextract,format=gray10le[alpha]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timeline {
	pub frames: u32,
	pub delays_ms: Vec<u32>,
	pub plays: Option<u32>,
	pub width: u32,
	pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Animation {
	pub timeline: Timeline,
	pub alpha: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnimatedSettings {
	pub crf: u8,
	pub cpu_used: u8,
}

pub fn scan(bytes: &[u8]) -> Result<Option<Timeline>, String> {
	let unreadable = |err: image::ImageError| format!("the animation is unreadable: {err}");
	let (loop_count, frames, counts_repeats) = match sniff(bytes) {
		ImageFormat::Gif => {
			let decoder = GifDecoder::new(Cursor::new(bytes)).map_err(unreadable)?;
			(decoder.loop_count(), decoder.into_frames(), true)
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

fn walk_frames(frames: Frames<'_>, plays: Option<u32>) -> Result<Timeline, String> {
	let mut timeline = Timeline {
		frames: 0,
		delays_ms: Vec::new(),
		plays,
		width: 0,
		height: 0,
	};

	for frame in frames {
		if timeline.frames == MAX_FRAMES {
			return Err(format!("the animation has more than {MAX_FRAMES} frames"));
		}

		let frame = frame.map_err(|err| format!("frame {} is unreadable: {err}", timeline.frames + 1))?;
		let (numer, denom) = frame.delay().numer_denom_ms();
		timeline
			.delays_ms
			.push((numer + denom / 2).checked_div(denom).unwrap_or(0));
		(timeline.width, timeline.height) = frame.buffer().dimensions();
		timeline.frames += 1;
	}
	Ok(timeline)
}

pub async fn has_alpha(tools: &Toolchain, source: &Path, cancel: &CancellationToken) -> Result<bool, JobError> {
	let mut cmd = ToolCommand::new(&tools.ffmpeg, STALL_TIMEOUT);
	cmd.args(["-hide_banner", "-nostdin", "-v", "error", "-i"])
		.arg(process::tool_path(source))
		.args(["-vf", TRANSPARENT_FRAMES, "-fps_mode", "passthrough", "-f", "null", "-"]);

	let out = run_ffmpeg(&cmd, |_| {}, cancel).await?;
	Ok(out
		.last_progress
		.and_then(|progress| progress.frame)
		.is_some_and(|frames| frames > 0))
}

pub fn plan_animation(speed: Speed, libaom: bool) -> Result<AnimatedSettings, KeepReason> {
	if !libaom {
		return Err(KeepReason::Unsupported("animations need ffmpeg with libaom-av1".into()));
	}

	let crf = match speed {
		Speed::Fast => 16,
		Speed::Moderate => 10,
		Speed::Slow => 6,
	};
	Ok(AnimatedSettings { crf, cpu_used: 6 })
}

pub fn encode_command(
	ffmpeg: &Path,
	source: &Path,
	output: &Path,
	animation: &Animation,
	settings: AnimatedSettings,
) -> ToolCommand {
	let mut cmd = ToolCommand::new(ffmpeg, STALL_TIMEOUT);
	cmd.count_lines_containing(IMAGE_DECODE_ERRORS);
	cmd.args(["-hide_banner", "-nostdin", "-y", "-v", "error", "-i"])
		.arg(process::tool_path(source));

	if animation.alpha {
		cmd.args([
			"-filter_complex",
			ALPHA_GRAPH,
			"-map",
			"[color]",
			"-map",
			"[alpha]",
			"-fps_mode:v",
		]);
	} else {
		cmd.args(["-pix_fmt", "yuv444p10le", "-fps_mode"]);
	}
	cmd.args(["passthrough", "-enc_time_base:v", "demux", "-c:v", "libaom-av1", "-crf"])
		.arg(settings.crf.to_string())
		.arg("-cpu-used")
		.arg(settings.cpu_used.to_string())
		.args(["-row-mt", "1"]);

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
	cancel: &CancellationToken,
) -> Result<f64, JobError> {
	let info = probe::probe(tools, output, cancel)
		.await
		.map_err(|err| failed(format!("ffprobe could not read the output: {err}")))?;
	let tracks = sequence_tracks(&info, animation)?;
	check_delays(tools, source, output, tracks[0], &animation.timeline, cancel).await?;

	let bytes = std::fs::read(output).map_err(|err| failed(err.to_string()))?;
	let timeline = &animation.timeline;
	if plays(&bytes) != Some(timeline.plays.map(u64::from)) {
		return Err(failed(format!(
			"the loop count is {:?}, not {:?}",
			plays(&bytes),
			timeline.plays
		)));
	}

	let frames = sampled_frames(timeline.frames);
	let reference = decode_frames(&tools.ffmpeg, source, &frames, None, cancel).await?;
	let decoded = decode_frames(&tools.ffmpeg, output, &frames, Some(&tracks), cancel).await?;
	if reference.len() != frames.len() || decoded.len() != frames.len() {
		return Err(failed("fewer frames decoded than sampled"));
	}

	blocking(cancel, move || {
		reference
			.iter()
			.zip(&decoded)
			.map(|(reference, decoded)| ssim::score(reference, decoded))
			.try_fold(f64::INFINITY, |worst, score| score.map(|score| worst.min(score)))
	})
	.await?
	.map_err(failed)
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
		return Err(failed(format!(
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
		return Err(failed(format!(
			"the output has {:?} frames, not {}",
			color.nb_frames, timeline.frames
		)));
	}
	if (color.width, color.height) != (Some(timeline.width), Some(timeline.height)) {
		return Err(failed("the output frame size differs from the source"));
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
	let gif = std::fs::read(source).is_ok_and(|bytes| sniff(&bytes) == ImageFormat::Gif);
	let expected = expected_delays(timeline, gif);

	let matches = delays.len() == expected.len()
		&& delays
			.iter()
			.zip(&expected)
			.all(|(found, wanted)| found.abs_diff(*wanted) <= DELAY_TOLERANCE_MS);
	if matches {
		Ok(())
	} else {
		Err(failed("the frame timing differs from the source"))
	}
}

async fn decode_frames(
	ffmpeg: &Path,
	input: &Path,
	frames: &[u32],
	tracks: Option<&[u32]>,
	cancel: &CancellationToken,
) -> Result<Vec<DynamicImage>, JobError> {
	let select = frames
		.iter()
		.map(|frame| format!(r"eq(n\,{frame})"))
		.collect::<Vec<_>>()
		.join("+");
	let mut cmd = ToolCommand::new(ffmpeg, STALL_TIMEOUT);
	cmd.args(["-hide_banner", "-nostdin", "-v", "error", "-i"])
		.arg(process::tool_path(input));

	match tracks {
		Some([color, alpha]) => {
			cmd.arg("-filter_complex").arg(format!(
				"[0:{color}][0:{alpha}]alphamerge,select='{select}',format=rgba64be"
			));
		}
		Some(tracks) => {
			cmd.arg("-map")
				.arg(format!("0:{}", tracks[0]))
				.arg("-vf")
				.arg(format!("select='{select}',format=rgba64be"));
		}
		None => {
			cmd.arg("-vf").arg(format!("select='{select}',format=rgba"));
		}
	}
	cmd.args(["-fps_mode", "passthrough", "-c:v", "pam", "-f", "image2pipe", "-"]);

	let out = process::run(&cmd, StdoutMode::Capture, |_| {}, cancel).await?;
	if !out.status.success() {
		return Err(failed(format!(
			"ffmpeg could not decode sampled frames: {}",
			process::failure_reason(&out.stderr_tail)
		)));
	}
	blocking(cancel, move || split_pams(&out.stdout)).await?.map_err(failed)
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

fn failed(reason: impl std::fmt::Display) -> JobError {
	ErrorKind::EncodeFailed(format!("verification failed: {reason}")).into()
}
