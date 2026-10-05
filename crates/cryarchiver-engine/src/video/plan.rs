use crate::calibration::tables;
use crate::options::{Resize, TrimWindow, VideoEncoderChoice};
use crate::outcome::KeepReason;
use crate::preset::Speed;
use crate::probe::{Frame, MediaInfo, Stream};
use crate::video::VideoStarts;
use crate::video::analyze::{Analysis, NoiseClass, ScanType, forced_decoder};
use crate::video::burn::Overlay;
use crate::video::colour::{self, ColourTags, conversion, output_tags, setparams};

const SVT_MAX_WIDTH: u32 = 16_384;
const SVT_MAX_HEIGHT: u32 = 8_704;
const OUTPUT_PIX_FMT: &str = "yuv420p10le";
const ALPHA_PIX_FMT_PREFIXES: &[&str] = &["yuva", "gbrap", "rgba", "bgra", "argb", "abgr", "ya"];
const EFFICIENT_CODECS: &[&str] = &["av1", "vp9"];
const EFFICIENCY_HEADROOM: f64 = 1.2;
const MASTERING_DISPLAY: &str = "Mastering display metadata";
const HDR10_PLUS: &str = "HDR Dynamic Metadata SMPTE2094-40 (HDR10+)";
const FILM_GRAIN_PARAMETERS: &str = "Film grain parameters";
const DOLBY_VISION_DROPPED: &str = "Dolby Vision metadata dropped; the compatible base layer is kept";
const RATE_AGREEMENT: f64 = 0.001;
const PACKET_AGREEMENT: f64 = 0.002;
const MIN_PACKET_SLACK: f64 = 2.0;
const SOURCE_FRAME_TOLERANCE: u64 = 2;
const VIDEO_BASE_BYTES: u64 = 512 << 20;
const VIDEO_BYTES_PER_PIXEL: u64 = 540;
/// The fewest threads each SVT-AV1 level of parallelism needs, a twentieth above the most cores it was measured on,
/// decoding included: 3.8 at level 2, 6.8 at level 3 and 11.8 at level 4 (4 s samples and 60 s encodes, presets 3 and
/// 5, 1080p and 4K). Level 5 couldn't be measured unsaturated, so it asks for twice level 4's threads, and level 6 is
/// left out.
const SVT_LEVELS: [(usize, u8); 4] = [(26, 5), (13, 4), (8, 3), (4, 2)];

/// A tool's thread caps under a thread budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Threads {
	pub tool: usize,
	pub svt_lp: u8,
}

/// The caps of a tool allowed `threads` threads, with SVT-AV1's highest level of parallelism whose CPU use fits them.
pub fn threads_for(threads: usize) -> Threads {
	let threads = threads.max(1);
	let svt_lp = SVT_LEVELS
		.iter()
		.find(|(fits, _)| threads >= *fits)
		.map_or(1, |(_, level)| *level);
	Threads { tool: threads, svt_lp }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoCaps {
	pub vp9_alpha: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
	Sd,
	Hd720,
	Hd1080,
	Qhd1440,
	Uhd4k,
	Uhd8k,
}

impl Resolution {
	pub fn of(width: u32, height: u32) -> Self {
		match u64::from(width) * u64::from(height) {
			..500_000 => Self::Sd,
			500_000..1_300_000 => Self::Hd720,
			1_300_000..2_800_000 => Self::Hd1080,
			2_800_000..5_000_000 => Self::Qhd1440,
			5_000_000..10_000_000 => Self::Uhd4k,
			_ => Self::Uhd8k,
		}
	}

	pub fn index(self) -> usize {
		self as usize
	}
}

#[derive(Debug, Clone, PartialEq)]
pub enum Encoder {
	SvtAv1 {
		preset: u8,
		crf: f64,
		film_grain: Option<u8>,
	},
	Aom {
		cpu_used: u8,
		crf: u8,
	},
	Vp9Alpha {
		cpu_used: u8,
		crf: u8,
	},
}

impl Encoder {
	pub fn crf(&self) -> f64 {
		match self {
			Self::SvtAv1 { crf, .. } => *crf,
			Self::Aom { crf, .. } | Self::Vp9Alpha { crf, .. } => f64::from(*crf),
		}
	}

	#[must_use]
	pub fn with_crf(&self, crf: f64) -> Self {
		match self {
			Self::SvtAv1 { preset, film_grain, .. } => Self::SvtAv1 {
				preset: *preset,
				crf: (crf * 4.0).round() / 4.0,
				film_grain: *film_grain,
			},
			Self::Aom { cpu_used, .. } => Self::Aom {
				cpu_used: *cpu_used,
				crf: rounded_crf(crf),
			},
			Self::Vp9Alpha { cpu_used, .. } => Self::Vp9Alpha {
				cpu_used: *cpu_used,
				crf: rounded_crf(crf),
			},
		}
	}

	pub fn preset(&self) -> u8 {
		match self {
			Self::SvtAv1 { preset, .. } => *preset,
			Self::Aom { cpu_used, .. } | Self::Vp9Alpha { cpu_used, .. } => *cpu_used,
		}
	}

	pub fn film_grain(&self) -> bool {
		self.grain_level().is_some()
	}

	pub fn grain_level(&self) -> Option<u8> {
		match self {
			Self::SvtAv1 { film_grain, .. } => *film_grain,
			Self::Aom { .. } | Self::Vp9Alpha { .. } => None,
		}
	}

	#[must_use]
	pub fn without_grain(&self) -> Self {
		match self {
			Self::SvtAv1 { preset, crf, .. } => Self::SvtAv1 {
				preset: *preset,
				crf: *crf,
				film_grain: None,
			},
			other => other.clone(),
		}
	}
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoPlan {
	pub streams: Vec<u32>,
	pub decoder: Option<&'static str>,
	/// Deinterlacing and the resize: what runs before burned subtitles are drawn, so they are drawn on the output's
	/// frames at its size.
	pub shape: String,
	/// The colour conversion, after any burned subtitles.
	pub filters: String,
	pub encoder: Encoder,
	pub expected_frames: Option<u64>,
	pub frame_tolerance: u64,
	pub width: u32,
	pub height: u32,
	pub colour: ColourTags,
	pub alpha: bool,
	pub hdr10: bool,
	pub warnings: Vec<String>,
	pub window: Option<TrimWindow>,
	pub starts: VideoStarts,
	pub overlay: Option<Overlay>,
	/// The text/ASS subtitle filter for the main video, before it is shifted to a command's seek.
	pub subtitles: Option<String>,
	pub threads: Option<Threads>,
	pub max_bitrate_kbps: Option<u32>,
}

/// Decoder, filters and SVT-AV1 together: about 5 GB for 4K (project layer spec §8.2).
pub fn memory_estimate(width: u32, height: u32) -> u64 {
	(u64::from(width) * u64::from(height))
		.saturating_mul(VIDEO_BYTES_PER_PIXEL)
		.saturating_add(VIDEO_BASE_BYTES)
}

pub fn main_stream(info: &MediaInfo) -> Option<&Stream> {
	let mut first_real = None;
	for stream in info.streams.iter().filter(|stream| stream.is_real_video()) {
		if stream.disposition.get("default") == Some(&1) {
			return Some(stream);
		}
		first_real.get_or_insert(stream);
	}
	first_real
}

pub fn precheck<'a>(info: &'a MediaInfo, caps: &VideoCaps) -> Result<&'a Stream, KeepReason> {
	let main = usable_main(info, caps)?;
	if let Some(reason) = already_efficient(info, main) {
		return Err(KeepReason::AlreadyEfficient(reason));
	}
	Ok(main)
}

/// The main video, unless there is none, it is Dolby Vision without a compatible base layer, or it is transparent and
/// this ffmpeg can't encode transparent VP9.
pub fn usable_main<'a>(info: &'a MediaInfo, caps: &VideoCaps) -> Result<&'a Stream, KeepReason> {
	let main = main_stream(info).ok_or_else(|| KeepReason::Unsupported("no video stream to encode".into()))?;

	if let Some((profile, compatibility)) = dolby_vision(main)
		&& (profile == 5 || compatibility == 0)
	{
		return Err(KeepReason::Unsupported(
			"Dolby Vision without a compatible base layer (profile 5)".into(),
		));
	}

	if has_alpha(main) && !caps.vp9_alpha {
		return Err(KeepReason::Unsupported(
			"transparent video needs libvpx-vp9 with alpha support".into(),
		));
	}
	Ok(main)
}

/// The output size, when it is larger than SVT-AV1 takes, so that the video is encoded with libaom-av1. Under
/// `resize` the limits apply to the output size; a resize the video can't take fails later, with its own reason.
pub fn beyond_svt(main: &Stream, resize: Option<Resize>) -> Option<(u32, u32)> {
	let (width, height) = resize
		.and_then(|resize| resize.size(shown_size(main), true).ok())
		.unwrap_or_else(|| display_size(main));
	(too_large_for_svt(width, height) && !has_alpha(main)).then_some((width, height))
}

/// `resize` is the output size under `--resize`; the encoder and its CRF follow the output size.
pub fn plan_video(
	info: &MediaInfo,
	main: &Stream,
	frames: &[Frame],
	analysis: &Analysis,
	speed: Speed,
	resize: Option<(u32, u32)>,
) -> VideoPlan {
	let (width, height) = resize.unwrap_or_else(|| display_size(main));
	let resolution = Resolution::of(width, height);
	let alpha = has_alpha(main);

	let encoder = if alpha {
		Encoder::Vp9Alpha {
			cpu_used: pick(speed, [1, 2, 4]),
			crf: tables().video.vp9_crf[resolution.index()],
		}
	} else if too_large_for_svt(width, height) {
		aom_encoder(speed, resolution)
	} else {
		Encoder::SvtAv1 {
			preset: pick(speed, [3, 5, 8]),
			crf: svt_crf(speed, resolution),
			film_grain: source_grain(analysis).filter(|_| speed != Speed::Fast && !synthesized_grain(frames)),
		}
	};

	let colour = output_tags(main);
	let mut shape: Vec<String> = Vec::new();
	match analysis.scan {
		ScanType::Progressive => {}
		ScanType::Interlaced { top_first } => shape.push(format!(
			"bwdif=mode=send_field:parity={}:deint=all",
			if top_first { "tff" } else { "bff" }
		)),
		ScanType::Telecined => shape.push("fieldmatch,decimate".into()),
	}
	if let Some(size) = resize {
		shape.push(colour::resize(main, &colour, size, alpha));
	}

	let converted = if alpha {
		colour::alpha_conversion(main, &colour)
	} else {
		conversion(main, &colour, OUTPUT_PIX_FMT)
	};
	let mut filters: Vec<String> = converted.into_iter().collect();
	filters.push(setparams(&colour));

	let mut warnings = Vec::new();
	if dolby_vision(main).is_some() {
		warnings.push(DOLBY_VISION_DROPPED.to_owned());
	}
	if has_side_data(frames, HDR10_PLUS) {
		warnings.push("HDR10+ dynamic metadata dropped; the static HDR10 metadata is kept".to_owned());
	}

	let mut streams = vec![main.index];
	streams.extend(
		info.streams
			.iter()
			.filter(|stream| stream.is_real_video() && stream.index != main.index)
			.map(|stream| stream.index),
	);

	VideoPlan {
		streams,
		decoder: forced_decoder(main),
		shape: shape.join(","),
		filters: filters.join(","),
		hdr10: matches!(encoder, Encoder::SvtAv1 { .. }) && has_side_data(frames, MASTERING_DISPLAY),
		encoder,
		expected_frames: expected_frames(main, main.declared_duration().or(info.format.duration), analysis.scan),
		frame_tolerance: frame_tolerance(analysis.scan),
		width,
		height,
		colour,
		alpha,
		warnings,
		window: None,
		starts: VideoStarts::default(),
		overlay: None,
		subtitles: None,
		threads: None,
		max_bitrate_kbps: None,
	}
}

/// What a video loses as an animated AVIF, which keeps only its colour primaries and transfer.
pub fn animation_warnings(main: &Stream, frames: &[Frame]) -> Vec<String> {
	let mut warnings = Vec::new();
	if dolby_vision(main).is_some() {
		warnings.push(DOLBY_VISION_DROPPED.to_owned());
	}
	if has_side_data(frames, MASTERING_DISPLAY) || has_side_data(frames, HDR10_PLUS) {
		warnings.push("HDR metadata dropped: animated AVIF keeps only the colour primaries and transfer".to_owned());
	}
	warnings
}

pub fn force_encoder(plan: &mut VideoPlan, choice: VideoEncoderChoice, speed: Speed) -> Result<(), String> {
	if plan.alpha {
		return Err("transparent video is always VP9, so --video-encoder doesn't apply".into());
	}

	match choice {
		VideoEncoderChoice::SvtAv1 if too_large_for_svt(plan.width, plan.height) => Err(format!(
			"{}×{} is larger than SVT-AV1 supports",
			plan.width, plan.height
		)),
		VideoEncoderChoice::SvtAv1 => Ok(()),
		VideoEncoderChoice::Aom => {
			if plan.hdr10 {
				plan.warnings
					.push("HDR10 mastering metadata isn't passed on by libaom-av1".to_owned());
				plan.hdr10 = false;
			}

			plan.encoder = aom_encoder(speed, Resolution::of(plan.width, plan.height));
			Ok(())
		}
	}
}

fn aom_encoder(speed: Speed, resolution: Resolution) -> Encoder {
	Encoder::Aom {
		cpu_used: pick(speed, [3, 4, 6]),
		crf: rounded_crf(svt_crf(speed, resolution)),
	}
}

fn expected_frames(main: &Stream, declared_s: Option<f64>, scan: ScanType) -> Option<u64> {
	decodable_frames(main, declared_s, scan, 0)
}

/// The frames the encode should get, when the packet count can be trusted: the packets less `undecoded` leading
/// ones that never decode into a frame. The count is trusted by the packets the container declares.
#[allow(clippy::cast_precision_loss)]
pub fn decodable_frames(main: &Stream, declared_s: Option<f64>, scan: ScanType, undecoded: u64) -> Option<u64> {
	let packets = main.nb_read_packets?;
	let (average_fps, real_fps) = (main.avg_frame_rate?, main.r_frame_rate?);
	let frames_by_duration = declared_s? * average_fps;

	let constant_rate = (average_fps - real_fps).abs() <= real_fps * RATE_AGREEMENT;
	let packet_slack = (frames_by_duration * PACKET_AGREEMENT).max(MIN_PACKET_SLACK);
	if !constant_rate || (packets as f64 - frames_by_duration).abs() > packet_slack {
		return None;
	}

	let packets = packets.saturating_sub(undecoded);
	Some(match scan {
		ScanType::Progressive => packets,
		ScanType::Interlaced { .. } => packets * 2,
		ScanType::Telecined => packets - packets / 5,
	})
}

fn frame_tolerance(scan: ScanType) -> u64 {
	match scan {
		ScanType::Interlaced { .. } => SOURCE_FRAME_TOLERANCE * 2,
		ScanType::Progressive | ScanType::Telecined => SOURCE_FRAME_TOLERANCE,
	}
}

pub fn svt_params(crf: f64, grain: Option<u8>, lp: Option<u8>, max_kbps: Option<u32>) -> String {
	let grain = grain.map_or_else(String::new, |level| format!(":film-grain={level}:film-grain-denoise=1"));
	let parallelism = lp.map(|lp| format!(":lp={lp}")).unwrap_or_default();
	let cap = max_kbps.map(|kbps| format!(":mbr={kbps}")).unwrap_or_default();
	format!("tune=0:keyint=5s:crf={crf}{grain}{parallelism}{cap}")
}

pub fn source_grain(analysis: &Analysis) -> Option<u8> {
	let levels = tables().video.grain_levels;
	let lightest = levels[levels.len() - 1];

	(analysis.noise == NoiseClass::Light).then(|| grain_level(analysis.noise_sigma).unwrap_or(lightest))
}

pub fn grain_level(noise_sigma: Option<f64>) -> Option<u8> {
	let noise_sigma = noise_sigma?;
	let video = &tables().video;

	video
		.grain_from_sigma
		.iter()
		.zip(video.grain_levels)
		.find(|(from, _)| noise_sigma >= **from)
		.map(|(_, level)| level)
}

/// Frames whose decoder made up their grain from film grain parameters (AV1, or H.264/HEVC film grain SEI).
pub fn synthesized_grain(frames: &[Frame]) -> bool {
	has_side_data(frames, FILM_GRAIN_PARAMETERS)
}

fn svt_crf(speed: Speed, resolution: Resolution) -> f64 {
	tables().video.svt_crf(speed)[resolution.index()]
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn rounded_crf(crf: f64) -> u8 {
	crf.round().clamp(0.0, 63.0) as u8
}

fn pick<T>(speed: Speed, [slow, moderate, fast]: [T; 3]) -> T {
	match speed {
		Speed::Slow => slow,
		Speed::Moderate => moderate,
		Speed::Fast => fast,
	}
}

fn display_size(stream: &Stream) -> (u32, u32) {
	let (width, height) = (stream.width.unwrap_or(0), stream.height.unwrap_or(0));
	if stream.rotation().rem_euclid(180) == 90 {
		(height, width)
	} else {
		(width, height)
	}
}

/// The size the picture is shown at: its pixels with the sample aspect ratio applied to their width, then rotated
/// upright.
pub fn shown_size(stream: &Stream) -> (f64, f64) {
	let width = f64::from(stream.width.unwrap_or(0)) * stream.sample_aspect_ratio.unwrap_or(1.0);
	let height = f64::from(stream.height.unwrap_or(0));
	if stream.rotation().rem_euclid(180) == 90 {
		(height, width)
	} else {
		(width, height)
	}
}

fn too_large_for_svt(width: u32, height: u32) -> bool {
	width > SVT_MAX_WIDTH || height > SVT_MAX_HEIGHT
}

pub fn has_alpha(stream: &Stream) -> bool {
	stream.tag("alpha_mode") == Some("1")
		|| stream
			.pix_fmt
			.as_deref()
			.is_some_and(|format| ALPHA_PIX_FMT_PREFIXES.iter().any(|prefix| format.starts_with(prefix)))
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn dolby_vision(stream: &Stream) -> Option<(u8, u8)> {
	let record = stream.side_data("DOVI configuration record")?;
	let profile = record.number("dv_profile")? as u8;
	let compatibility = record.number("dv_bl_signal_compatibility_id").unwrap_or(0.0) as u8;
	Some((profile, compatibility))
}

fn has_side_data(frames: &[Frame], kind: &str) -> bool {
	frames
		.iter()
		.flat_map(|frame| &frame.side_data_list)
		.any(|data| data.side_data_type == kind)
}

#[allow(clippy::cast_precision_loss)]
pub fn source_bpp(info: &MediaInfo, main: &Stream) -> Option<f64> {
	let bits_per_second = main.bit_rate.or_else(|| {
		let single_video = info.streams.iter().filter(|stream| stream.is_real_video()).count() == 1;
		let audio: u64 = info.audio_streams().filter_map(|stream| stream.bit_rate).sum();
		info.format
			.bit_rate
			.filter(|_| single_video)
			.map(|total| total.saturating_sub(audio))
	})?;
	let (width, height) = (main.width?, main.height?);
	let fps = main.fps()?;

	Some(bits_per_second as f64 / (f64::from(width) * f64::from(height) * fps))
}

pub fn efficient_at(info: &MediaInfo, main: &Stream, expected_bpp: f64) -> Option<String> {
	if !EFFICIENT_CODECS.contains(&main.codec_name.as_str()) {
		return None;
	}

	let bpp = source_bpp(info, main)?;
	(bpp <= expected_bpp * EFFICIENCY_HEADROOM)
		.then(|| format!("already {} at {bpp:.3} bits per pixel", main.codec_name.to_uppercase()))
}

fn already_efficient(info: &MediaInfo, main: &Stream) -> Option<String> {
	let expected = tables().video.expected_bpp[Resolution::of(main.width?, main.height?).index()];
	efficient_at(info, main, expected)
}

#[cfg(test)]
pub(crate) fn test_plan() -> VideoPlan {
	VideoPlan {
		streams: vec![0],
		decoder: None,
		shape: String::new(),
		filters: String::new(),
		encoder: Encoder::SvtAv1 {
			preset: 8,
			crf: 32.0,
			film_grain: None,
		},
		expected_frames: None,
		frame_tolerance: SOURCE_FRAME_TOLERANCE,
		width: 320,
		height: 240,
		colour: ColourTags {
			primaries: "bt709".into(),
			transfer: "bt709".into(),
			matrix: "bt709".into(),
			range: crate::video::colour::Range::Limited,
			chroma_location: None,
		},
		alpha: false,
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

#[cfg(test)]
mod tests {
	use super::*;

	const CAPS: VideoCaps = VideoCaps { vp9_alpha: true };
	const PROGRESSIVE_CLEAN: Analysis = Analysis {
		scan: ScanType::Progressive,
		noise: NoiseClass::Clean,
		noise_sigma: None,
		detail: Vec::new(),
	};

	#[test]
	fn memory_grows_with_the_resolution() {
		let uhd = memory_estimate(3840, 2160);
		let full_hd = memory_estimate(1920, 1080);

		assert!((4_900_000_000..5_100_000_000).contains(&uhd), "{uhd}");
		assert!((1_500_000_000..1_800_000_000).contains(&full_hd), "{full_hd}");
	}

	fn info(json: &str) -> MediaInfo {
		MediaInfo::parse(json.as_bytes()).unwrap()
	}

	#[allow(clippy::needless_pass_by_value)]
	fn plan(info: &MediaInfo, frames: &[Frame], analysis: Analysis, speed: Speed) -> VideoPlan {
		let main = precheck(info, &CAPS).unwrap();
		plan_video(info, main, frames, &analysis, speed, None)
	}

	#[test]
	fn a_resize_runs_after_deinterlacing_and_the_output_size_picks_the_crf() {
		let source = h264(r#","chroma_location":"left""#);
		let tff = Analysis {
			scan: ScanType::Interlaced { top_first: true },
			..PROGRESSIVE_CLEAN
		};

		let resized = plan_video(
			&source,
			&source.streams[0],
			&[],
			&tff,
			Speed::Moderate,
			Some((640, 360)),
		);

		assert_eq!((resized.width, resized.height), (640, 360));
		assert_eq!(
			resized.shape,
			"bwdif=mode=send_field:parity=tff:deint=all,scale=w=640:h=360:flags=lanczos+accurate_rnd:in_range=tv:\
			 out_range=tv:in_color_matrix=bt709:out_color_matrix=bt709:in_chroma_loc=left:out_chroma_loc=left,setsar=1"
		);
		assert!(resized.filters.starts_with("scale=in_range=tv:"), "{}", resized.filters);
		assert_eq!(
			resized.encoder,
			Encoder::SvtAv1 {
				preset: 5,
				crf: 28.0,
				film_grain: None,
			}
		);
		assert_eq!(resized.expected_frames, Some(500));
	}

	#[test]
	fn transparent_video_is_resized_premultiplied() {
		let alpha = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"vp9","width":640,"height":360,
			"pix_fmt":"yuv420p","tags":{"ALPHA_MODE":"1"}}]}"#,
		);

		let resized = plan_video(
			&alpha,
			&alpha.streams[0],
			&[],
			&PROGRESSIVE_CLEAN,
			Speed::Moderate,
			Some((320, 180)),
		);

		assert!(
			resized
				.shape
				.starts_with("premultiply=inplace=1,scale=w=320:h=180:flags=lanczos+accurate_rnd:"),
			"{}",
			resized.shape
		);
		assert!(
			resized.shape.ends_with(",unpremultiply=inplace=1,setsar=1"),
			"{}",
			resized.shape
		);
	}

	#[test]
	fn the_shown_size_applies_the_sample_aspect_ratio_before_the_rotation() {
		let stream = |json: &str| -> Stream { serde_json::from_str(json).unwrap() };
		let anamorphic =
			stream(r#"{"index":0,"codec_type":"video","width":720,"height":576,"sample_aspect_ratio":"64:45"}"#);
		let rotated = stream(
			r#"{"index":0,"codec_type":"video","width":100,"height":50,"sample_aspect_ratio":"2:1",
			"side_data_list":[{"side_data_type":"Display Matrix","rotation":-90}]}"#,
		);
		let square = stream(r#"{"index":0,"codec_type":"video","width":320,"height":240}"#);

		assert_eq!(shown_size(&anamorphic), (1024.0, 576.0));
		assert_eq!(shown_size(&rotated), (50.0, 200.0));
		assert_eq!(shown_size(&square), (320.0, 240.0));
	}

	fn h264(extra: &str) -> MediaInfo {
		info(&format!(
			r#"{{"streams":[{{"index":0,"codec_type":"video","codec_name":"h264","width":1920,"height":1080,
			"pix_fmt":"yuv420p","color_range":"tv","color_space":"bt709","color_transfer":"bt709",
			"color_primaries":"bt709","avg_frame_rate":"25/1","r_frame_rate":"25/1","duration":"10.000000",
			"nb_read_packets":"250"{extra}}}]}}"#
		))
	}

	#[test]
	fn the_vmaf_reference_keeps_the_source_chroma_location() {
		use crate::quality::vmaf::{Reference, Scoring, sample_command};

		let plan = plan(
			&h264(r#","chroma_location":"topleft""#),
			&[],
			PROGRESSIVE_CLEAN,
			Speed::Moderate,
		);
		let reference = Reference {
			source: std::path::Path::new("in.mkv"),
			stream: 0,
			decoder: None,
			shape: &plan.shape,
			filters: &plan.filters,
			window: None,
			start: VideoStarts::default().main,
			overlay: None,
			subtitles: None,
		};
		let scoring = Scoring {
			model: "vmaf_v0.6.1",
			threads: 1,
			budget: None,
			dir: std::path::Path::new("work"),
			log: "vmaf.json",
		};

		let args = crate::video::joined(&sample_command(
			std::path::Path::new("ffmpeg"),
			std::path::Path::new("sample.mkv"),
			&reference,
			1.0,
			4.0,
			&scoring,
		));

		assert!(
			args.contains(
				"[1:0]scale=in_range=tv:out_range=tv:in_color_matrix=bt709:out_color_matrix=bt709:in_chroma_loc=topleft:\
				 out_chroma_loc=topleft,format=yuv420p10le,setparams=field_mode=prog:range=tv:color_primaries=bt709:\
				 color_trc=bt709:colorspace=bt709:chroma_location=topleft,format=yuv420p10le"
			),
			"{args}"
		);
	}

	#[test]
	fn progressive_hd_uses_svt_at_the_table_crf() {
		let plan = plan(&h264(""), &[], PROGRESSIVE_CLEAN, Speed::Moderate);

		assert_eq!(plan.streams, [0]);
		assert_eq!(plan.decoder, None);
		assert_eq!(
			plan.filters,
			"scale=in_range=tv:out_range=tv:in_color_matrix=bt709:out_color_matrix=bt709,format=yuv420p10le,\
			 setparams=field_mode=prog:range=tv:color_primaries=bt709:color_trc=bt709:colorspace=bt709"
		);
		assert_eq!(
			plan.encoder,
			Encoder::SvtAv1 {
				preset: 5,
				crf: 29.0,
				film_grain: None,
			}
		);
		assert_eq!(plan.expected_frames, Some(250));
		assert_eq!((plan.width, plan.height), (1920, 1080));
		assert!(!plan.alpha && !plan.hdr10);
	}

	#[test]
	fn packet_counts_are_trusted_only_for_plain_constant_rate_streams() {
		let cfr = Stream {
			avg_frame_rate: Some(25.0),
			r_frame_rate: Some(25.0),
			nb_read_packets: Some(250),
			..Stream::default()
		};
		let variable = Stream {
			avg_frame_rate: Some(29.5),
			r_frame_rate: Some(30.0),
			..cfr.clone()
		};
		let pre_roll = Stream {
			nb_read_packets: Some(280),
			..cfr.clone()
		};
		let field_rate = Stream {
			r_frame_rate: Some(50.0),
			..cfr.clone()
		};

		assert_eq!(expected_frames(&cfr, Some(10.0), ScanType::Progressive), Some(250));
		assert_eq!(expected_frames(&cfr, Some(10.04), ScanType::Progressive), Some(250));
		assert_eq!(expected_frames(&cfr, None, ScanType::Progressive), None);
		assert_eq!(expected_frames(&variable, Some(10.0), ScanType::Progressive), None);
		assert_eq!(expected_frames(&pre_roll, Some(10.0), ScanType::Progressive), None);
		assert_eq!(expected_frames(&field_rate, Some(10.0), ScanType::Progressive), None);
	}

	#[test]
	fn leading_packets_that_never_decode_are_not_expected_as_frames() {
		let open_gop_cut = Stream {
			avg_frame_rate: Some(25.0),
			r_frame_rate: Some(25.0),
			nb_read_packets: Some(143),
			..Stream::default()
		};

		assert_eq!(
			decodable_frames(&open_gop_cut, Some(5.72), ScanType::Progressive, 3),
			Some(140)
		);
		assert_eq!(
			decodable_frames(&open_gop_cut, Some(5.72), ScanType::Interlaced { top_first: true }, 3),
			Some(280)
		);
		assert_eq!(
			decodable_frames(&open_gop_cut, Some(6.9), ScanType::Progressive, 3),
			None
		);
	}

	#[test]
	fn the_default_stream_is_main_else_the_first_real_video() {
		let covers_first = info(
			r#"{"streams":[
			{"index":0,"codec_type":"video","codec_name":"mjpeg","disposition":{"attached_pic":1,"default":1}},
			{"index":1,"codec_type":"video","codec_name":"h264"},
			{"index":2,"codec_type":"video","codec_name":"h264"}]}"#,
		);

		assert_eq!(main_stream(&covers_first).unwrap().index, 1);
		assert!(main_stream(&info(r#"{"streams":[{"index":0,"codec_type":"audio"}]}"#)).is_none());
	}

	#[test]
	fn speeds_pick_presets_and_crfs() {
		let fast = plan(&h264(""), &[], PROGRESSIVE_CLEAN, Speed::Fast);
		let slow = plan(&h264(""), &[], PROGRESSIVE_CLEAN, Speed::Slow);

		assert_eq!(
			fast.encoder,
			Encoder::SvtAv1 {
				preset: 8,
				crf: 35.0,
				film_grain: None,
			}
		);
		assert_eq!(
			slow.encoder,
			Encoder::SvtAv1 {
				preset: 3,
				crf: 25.0,
				film_grain: None,
			}
		);
	}

	#[test]
	fn fast_crfs_follow_the_tuned_row() {
		let buckets = [
			Resolution::Sd,
			Resolution::Hd720,
			Resolution::Hd1080,
			Resolution::Qhd1440,
			Resolution::Uhd4k,
			Resolution::Uhd8k,
		];

		assert_eq!(
			buckets.map(|resolution| rounded_crf(svt_crf(Speed::Fast, resolution))),
			[35, 35, 35, 37, 36, 38]
		);
	}

	#[test]
	fn scan_type_picks_the_deinterlacer_and_frame_count() {
		let tff = Analysis {
			scan: ScanType::Interlaced { top_first: true },
			..PROGRESSIVE_CLEAN
		};
		let telecined = Analysis {
			scan: ScanType::Telecined,
			..PROGRESSIVE_CLEAN
		};

		let deinterlaced = plan(&h264(""), &[], tff, Speed::Moderate);
		let inverse_telecined = plan(&h264(""), &[], telecined, Speed::Moderate);

		assert_eq!(deinterlaced.shape, "bwdif=mode=send_field:parity=tff:deint=all");
		assert!(deinterlaced.filters.starts_with("scale="), "{}", deinterlaced.filters);
		assert_eq!(deinterlaced.expected_frames, Some(500));
		assert_eq!(deinterlaced.frame_tolerance, 4);
		assert_eq!(inverse_telecined.shape, "fieldmatch,decimate");
		assert!(inverse_telecined.filters.starts_with("scale="));
		assert_eq!(inverse_telecined.expected_frames, Some(200));
		assert_eq!(inverse_telecined.frame_tolerance, 2);
	}

	#[test]
	fn natural_noise_adds_film_grain_except_at_fast() {
		let grainy = Analysis {
			noise: NoiseClass::Light,
			noise_sigma: Some(2.2),
			..PROGRESSIVE_CLEAN
		};

		let moderate = plan(&h264(""), &[], grainy.clone(), Speed::Moderate);
		let slow = plan(&h264(""), &[], grainy.clone(), Speed::Slow);
		let fast = plan(&h264(""), &[], grainy, Speed::Fast);

		assert_eq!(
			moderate.encoder,
			Encoder::SvtAv1 {
				preset: 5,
				crf: 29.0,
				film_grain: Some(10),
			}
		);
		assert_eq!(
			slow.encoder,
			Encoder::SvtAv1 {
				preset: 3,
				crf: 25.0,
				film_grain: Some(10),
			}
		);
		assert!(moderate.encoder.film_grain());
		assert_eq!(moderate.encoder.grain_level(), Some(10));
		assert!(!fast.encoder.film_grain());
		assert_eq!(fast.encoder.grain_level(), None);
	}

	#[test]
	fn the_noise_class_decides_grain_and_the_measured_noise_its_level() {
		let light = |noise_sigma: Option<f64>| Analysis {
			noise: NoiseClass::Light,
			noise_sigma,
			..PROGRESSIVE_CLEAN
		};
		let clean = Analysis {
			noise_sigma: Some(0.3),
			..PROGRESSIVE_CLEAN
		};

		assert_eq!(source_grain(&light(Some(1.2))), Some(8));
		assert_eq!(source_grain(&light(Some(0.5))), Some(6));
		assert_eq!(source_grain(&light(None)), Some(6));
		assert_eq!(source_grain(&clean), None);
		assert_eq!(plan(&h264(""), &[], clean, Speed::Moderate).encoder.grain_level(), None);
	}

	#[test]
	fn a_source_with_film_grain_parameters_gets_no_more_grain() {
		let grainy = Analysis {
			noise: NoiseClass::Light,
			noise_sigma: Some(1.2),
			..PROGRESSIVE_CLEAN
		};
		let synthesized: Vec<Frame> = serde_json::from_str(
			r#"[{"side_data_list":[{"side_data_type":"Film grain parameters","type":"av1","seed":"7391"}]}]"#,
		)
		.unwrap();

		let plan = plan(&h264(""), &synthesized, grainy, Speed::Slow);

		assert!(synthesized_grain(&synthesized));
		assert!(!synthesized_grain(&[]));
		assert_eq!(plan.encoder.grain_level(), None);
	}

	#[test]
	fn clean_sources_get_no_grain_at_any_speed() {
		for speed in [Speed::Slow, Speed::Moderate, Speed::Fast] {
			let plan = plan(&h264(""), &[], PROGRESSIVE_CLEAN, speed);

			assert_eq!(plan.encoder.grain_level(), None, "{speed:?}");
			assert!(!plan.encoder.film_grain(), "{speed:?}");
		}
	}

	#[test]
	fn grain_strength_follows_the_measured_noise() {
		assert_eq!(grain_level(None), None);
		assert_eq!(grain_level(Some(0.0)), None);
		assert_eq!(grain_level(Some(0.28)), None);
		assert_eq!(grain_level(Some(0.29)), Some(6));
		assert_eq!(grain_level(Some(0.99)), Some(6));
		assert_eq!(grain_level(Some(1.0)), Some(8));
		assert_eq!(grain_level(Some(1.79)), Some(8));
		assert_eq!(grain_level(Some(1.8)), Some(10));
		assert_eq!(grain_level(Some(4.0)), Some(10));
	}

	#[test]
	fn an_encoder_without_grain_keeps_its_preset_and_crf() {
		let grainy = Encoder::SvtAv1 {
			preset: 5,
			crf: 31.25,
			film_grain: Some(14),
		};

		assert_eq!(
			grainy.without_grain(),
			Encoder::SvtAv1 {
				preset: 5,
				crf: 31.25,
				film_grain: None,
			}
		);
		assert_eq!(
			Encoder::Aom { cpu_used: 4, crf: 30 }.without_grain(),
			Encoder::Aom { cpu_used: 4, crf: 30 }
		);
	}

	#[test]
	fn svt_parameters_carry_the_grain_level() {
		assert_eq!(
			svt_params(30.0, Some(14), None, None),
			"tune=0:keyint=5s:crf=30:film-grain=14:film-grain-denoise=1"
		);
		assert_eq!(svt_params(30.0, None, None, None), "tune=0:keyint=5s:crf=30");
	}

	#[test]
	fn a_bitrate_cap_becomes_svt_capped_crf() {
		assert_eq!(
			svt_params(38.0, Some(12), None, Some(12_500)),
			"tune=0:keyint=5s:crf=38:film-grain=12:film-grain-denoise=1:mbr=12500"
		);
		assert_eq!(svt_params(30.0, None, Some(2), None), "tune=0:keyint=5s:crf=30:lp=2");
	}

	#[test]
	fn a_budget_gets_the_highest_svt_level_whose_cpu_use_fits_it() {
		let levels = [0, 1, 3, 4, 7, 8, 12, 13, 25, 26, 64].map(|threads| threads_for(threads).svt_lp);

		assert_eq!(levels, [1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5]);
		assert_eq!(threads_for(8), Threads { tool: 8, svt_lp: 3 });
		assert_eq!(threads_for(0).tool, 1);
	}

	#[test]
	fn six_threads_of_eight_cores_run_a_level_that_fits_six() {
		let six = threads_for(6);

		assert_eq!(
			six,
			Threads { tool: 6, svt_lp: 2 },
			"level 2 uses about 3.8 cores, level 3 about 6.8"
		);
		assert!(SVT_LEVELS.iter().all(|(fits, level)| *level <= six.svt_lp || *fits > 6));
	}

	#[test]
	fn svt_parallelism_is_passed_only_under_a_budget() {
		assert_eq!(svt_params(30.0, None, None, None), "tune=0:keyint=5s:crf=30");
		assert_eq!(svt_params(30.0, None, Some(2), None), "tune=0:keyint=5s:crf=30:lp=2");
		assert_eq!(
			svt_params(38.0, Some(12), Some(2), None),
			"tune=0:keyint=5s:crf=38:film-grain=12:film-grain-denoise=1:lp=2"
		);
	}

	#[test]
	fn rotation_swaps_the_output_size() {
		let rotated = h264(r#","side_data_list":[{"side_data_type":"Display Matrix","rotation":-90}]"#);

		let plan = plan(&rotated, &[], PROGRESSIVE_CLEAN, Speed::Moderate);

		assert_eq!((plan.width, plan.height), (1080, 1920));
	}

	#[test]
	fn transparent_video_goes_to_vp9_or_is_kept() {
		let vp9_alpha = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"vp9","width":640,"height":360,
			"pix_fmt":"yuv420p","tags":{"ALPHA_MODE":"1"}}]}"#,
		);
		let prores = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"prores","width":640,"height":360,
			"pix_fmt":"yuva444p12le"}]}"#,
		);
		let without_libvpx = VideoCaps { vp9_alpha: false };

		let vp9 = plan(&vp9_alpha, &[], PROGRESSIVE_CLEAN, Speed::Moderate);
		assert_eq!(vp9.decoder, Some("libvpx-vp9"));
		assert!(vp9.alpha);
		assert_eq!(vp9.encoder, Encoder::Vp9Alpha { cpu_used: 2, crf: 33 });
		assert!(vp9.filters.contains("format=yuva420p"), "{}", vp9.filters);

		let from_prores = plan(&prores, &[], PROGRESSIVE_CLEAN, Speed::Fast);
		assert_eq!(from_prores.decoder, None);
		assert_eq!(from_prores.encoder, Encoder::Vp9Alpha { cpu_used: 4, crf: 33 });

		assert!(matches!(
			precheck(&prores, &without_libvpx),
			Err(KeepReason::Unsupported(_))
		));
	}

	#[test]
	fn dolby_vision_profile_5_is_kept_and_profile_8_warns() {
		let dovi = |profile: u8, compat: u8| {
			h264(&format!(
				r#","side_data_list":[{{"side_data_type":"DOVI configuration record","dv_profile":{profile},
				"dv_bl_signal_compatibility_id":{compat}}}]"#
			))
		};

		assert!(matches!(precheck(&dovi(5, 0), &CAPS), Err(KeepReason::Unsupported(_))));
		let plan = plan(&dovi(8, 4), &[], PROGRESSIVE_CLEAN, Speed::Moderate);
		assert!(
			plan.warnings.iter().any(|warning| warning.contains("Dolby Vision")),
			"{:?}",
			plan.warnings
		);
	}

	#[test]
	fn oversize_video_needs_libaom() {
		let wide = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"mjpeg","width":17000,"height":960,
			"pix_fmt":"yuvj420p","avg_frame_rate":"5/1"}]}"#,
		);

		let plan = plan(&wide, &[], PROGRESSIVE_CLEAN, Speed::Fast);

		assert_eq!(plan.encoder, Encoder::Aom { cpu_used: 6, crf: 38 });
		assert_eq!(beyond_svt(main_stream(&wide).unwrap(), None), Some((17_000, 960)));
		assert!(precheck(&wide, &CAPS).is_ok(), "the engine checks for libaom-av1");
	}

	#[test]
	fn the_svt_size_limit_applies_to_the_resized_size() {
		let wide = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"mjpeg","width":17000,"height":960,
			"pix_fmt":"yuvj420p","avg_frame_rate":"5/1"}]}"#,
		);
		let width = |width| {
			Some(Resize {
				width: Some(width),
				height: None,
			})
		};
		let h264 = h264("");
		let (wide, h264) = (main_stream(&wide).unwrap(), main_stream(&h264).unwrap());

		assert_eq!(beyond_svt(wide, width(1920)), None);
		assert_eq!(beyond_svt(wide, None), Some((17_000, 960)));
		assert_eq!(beyond_svt(h264, width(20_000)), Some((20_000, 11_250)));
		assert_eq!(beyond_svt(h264, None), None);
	}

	#[test]
	fn an_encoder_override_replaces_svt_or_refuses() {
		let mut aom = plan(&h264(""), &[], PROGRESSIVE_CLEAN, Speed::Moderate);
		let mut wide = VideoPlan {
			width: 17_000,
			height: 960,
			..test_plan()
		};
		let mut alpha = VideoPlan {
			alpha: true,
			..test_plan()
		};

		force_encoder(&mut aom, VideoEncoderChoice::Aom, Speed::Moderate).unwrap();

		assert_eq!(aom.encoder, Encoder::Aom { cpu_used: 4, crf: 29 });
		assert!(
			force_encoder(&mut wide, VideoEncoderChoice::SvtAv1, Speed::Moderate)
				.unwrap_err()
				.contains("larger than SVT-AV1")
		);
		assert!(force_encoder(&mut alpha, VideoEncoderChoice::Aom, Speed::Moderate).is_err());
	}

	#[test]
	fn efficient_sources_are_kept_by_the_bitrate_pre_gate() {
		let av1 = |kbps: u32| {
			info(&format!(
				r#"{{"streams":[{{"index":0,"codec_type":"video","codec_name":"av1","width":1920,"height":1080,
				"avg_frame_rate":"30/1","bit_rate":"{}"}},
				{{"index":1,"codec_type":"audio","codec_name":"opus","bit_rate":"128000"}}]}}"#,
				kbps * 1000
			))
		};

		assert!(matches!(
			precheck(&av1(3_000), &CAPS),
			Err(KeepReason::AlreadyEfficient(_))
		));
		assert!(precheck(&av1(34_000), &CAPS).is_ok());
		assert!(precheck(&h264(r#","bit_rate":"1000000""#), &CAPS).is_ok());
	}

	#[test]
	fn container_bitrate_stands_in_for_a_single_video_stream() {
		let webm = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"vp9","width":1920,"height":1080,
			"avg_frame_rate":"24/1"},{"index":1,"codec_type":"audio","codec_name":"opus"}],
			"format":{"bit_rate":"2500000"}}"#,
		);

		assert!(matches!(precheck(&webm, &CAPS), Err(KeepReason::AlreadyEfficient(_))));
	}

	#[test]
	fn hdr_side_data_is_recorded_and_hdr10_plus_warns() {
		let frames: Vec<Frame> = serde_json::from_str(
			r#"[{"side_data_list":[{"side_data_type":"Mastering display metadata"},
			{"side_data_type":"HDR Dynamic Metadata SMPTE2094-40 (HDR10+)"}]}]"#,
		)
		.unwrap();

		let plan = plan(&h264(""), &frames, PROGRESSIVE_CLEAN, Speed::Moderate);

		assert!(plan.hdr10);
		assert!(
			plan.warnings.iter().any(|warning| warning.starts_with("HDR10+")),
			"{:?}",
			plan.warnings
		);
	}

	#[test]
	fn every_real_video_stream_is_encoded_main_first() {
		let angles = info(
			r#"{"streams":[
			{"index":0,"codec_type":"video","codec_name":"h264","width":640,"height":360},
			{"index":1,"codec_type":"video","codec_name":"h264","width":640,"height":360,"disposition":{"default":1}},
			{"index":2,"codec_type":"video","codec_name":"mjpeg","disposition":{"attached_pic":1}}]}"#,
		);

		assert_eq!(main_stream(&angles).unwrap().index, 1);
		let plan = plan(&angles, &[], PROGRESSIVE_CLEAN, Speed::Moderate);
		assert_eq!(plan.streams, [1, 0]);
	}

	#[test]
	fn resolution_buckets_follow_pixel_counts() {
		assert_eq!(Resolution::of(854, 480), Resolution::Sd);
		assert_eq!(Resolution::of(1280, 720), Resolution::Hd720);
		assert_eq!(Resolution::of(1920, 1080), Resolution::Hd1080);
		assert_eq!(Resolution::of(3440, 1440), Resolution::Qhd1440);
		assert_eq!(Resolution::of(3840, 2160), Resolution::Uhd4k);
		assert_eq!(Resolution::of(7680, 4320), Resolution::Uhd8k);
	}

	#[test]
	fn a_new_crf_keeps_the_encoder_and_rounds_to_its_step() {
		let svt = Encoder::SvtAv1 {
			preset: 5,
			crf: 29.0,
			film_grain: Some(14),
		};
		let vp9 = Encoder::Vp9Alpha { cpu_used: 2, crf: 33 };

		assert_eq!(
			svt.with_crf(26.3),
			Encoder::SvtAv1 {
				preset: 5,
				crf: 26.25,
				film_grain: Some(14),
			}
		);
		assert_eq!(vp9.with_crf(30.6), Encoder::Vp9Alpha { cpu_used: 2, crf: 31 });
		assert_eq!(
			Encoder::Aom { cpu_used: 4, crf: 30 }.with_crf(80.0),
			Encoder::Aom { cpu_used: 4, crf: 63 }
		);
		assert_eq!((svt.preset(), vp9.preset()), (5, 2));
		assert_eq!(
			svt_params(29.25, Some(8), None, None),
			"tune=0:keyint=5s:crf=29.25:film-grain=8:film-grain-denoise=1"
		);
		assert_eq!(svt_params(29.0, None, None, None), "tune=0:keyint=5s:crf=29");
	}

	#[test]
	fn a_learned_bits_per_pixel_can_keep_an_efficient_source() {
		let av1 = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"av1","width":1920,"height":1080,
			"avg_frame_rate":"30/1","bit_rate":"34000000"}]}"#,
		);
		let main = &av1.streams[0];

		let bpp = source_bpp(&av1, main).unwrap();

		assert!((bpp - 0.546).abs() < 0.001, "{bpp}");
		assert!(efficient_at(&av1, main, 0.5).is_some());
		assert_eq!(efficient_at(&av1, main, 0.3), None);
		assert_eq!(
			efficient_at(&h264(r#","bit_rate":"1000""#), &h264("").streams[0], 10.0),
			None
		);
	}
}
