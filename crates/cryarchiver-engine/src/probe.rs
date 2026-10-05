use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::process::{self, RunError, StdoutMode, ToolCommand, ToolOutput};
use crate::toolchain::Toolchain;

const PROBE_TIMEOUT: Duration = Duration::from_secs(120);
const AV1_GRAIN_TRACE: &[&str] = &[" apply_grain "];
const ESTIMATED_DURATION: &str = "Estimating duration from bitrate";
const JSON_ARGS: [&str; 8] = [
	"-v",
	"warning",
	"-hide_banner",
	"-print_format",
	"json",
	"-show_format",
	"-show_streams",
	"-show_chapters",
];

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MediaInfo {
	#[serde(default)]
	pub streams: Vec<Stream>,
	#[serde(default)]
	pub format: Format,
	#[serde(default)]
	pub chapters: Vec<Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Format {
	#[serde(default)]
	pub format_name: String,
	#[serde(default, deserialize_with = "number")]
	pub start_time: Option<f64>,
	#[serde(default, deserialize_with = "number")]
	pub duration: Option<f64>,
	#[serde(default, deserialize_with = "number")]
	pub bit_rate: Option<u64>,
	#[serde(default)]
	pub tags: BTreeMap<String, String>,
	#[serde(skip)]
	pub duration_estimated: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Stream {
	pub index: u32,
	#[serde(default)]
	pub codec_type: String,
	#[serde(default)]
	pub codec_name: String,
	#[serde(default)]
	pub codec_tag_string: String,
	#[serde(default)]
	pub profile: Option<String>,
	#[serde(default, deserialize_with = "number")]
	pub sample_rate: Option<u32>,
	#[serde(default)]
	pub channels: Option<u32>,
	#[serde(default)]
	pub channel_layout: Option<String>,
	#[serde(default, deserialize_with = "number")]
	pub bit_rate: Option<u64>,
	#[serde(default)]
	pub width: Option<u32>,
	#[serde(default)]
	pub height: Option<u32>,
	/// A pixel's width over its height, when the stream says.
	#[serde(default, deserialize_with = "aspect")]
	pub sample_aspect_ratio: Option<f64>,
	#[serde(default)]
	pub pix_fmt: Option<String>,
	#[serde(default)]
	pub color_range: Option<String>,
	#[serde(default)]
	pub color_space: Option<String>,
	#[serde(default)]
	pub color_transfer: Option<String>,
	#[serde(default)]
	pub color_primaries: Option<String>,
	#[serde(default)]
	pub chroma_location: Option<String>,
	#[serde(default)]
	pub field_order: Option<String>,
	#[serde(default, deserialize_with = "rational")]
	pub r_frame_rate: Option<f64>,
	#[serde(default, deserialize_with = "rational")]
	pub avg_frame_rate: Option<f64>,
	#[serde(default, deserialize_with = "number")]
	pub nb_read_packets: Option<u64>,
	#[serde(default, deserialize_with = "number")]
	pub nb_frames: Option<u64>,
	#[serde(default, deserialize_with = "number")]
	pub start_time: Option<f64>,
	#[serde(default, deserialize_with = "number")]
	pub duration: Option<f64>,
	#[serde(default)]
	pub disposition: BTreeMap<String, u8>,
	#[serde(default)]
	pub tags: BTreeMap<String, String>,
	#[serde(default)]
	pub side_data_list: Vec<SideData>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SideData {
	#[serde(default)]
	pub side_data_type: String,
	#[serde(flatten)]
	pub fields: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Frame {
	#[serde(default)]
	pub color_range: Option<String>,
	#[serde(default)]
	pub color_space: Option<String>,
	#[serde(default)]
	pub color_transfer: Option<String>,
	#[serde(default)]
	pub color_primaries: Option<String>,
	#[serde(default)]
	pub side_data_list: Vec<SideData>,
}

#[derive(Deserialize)]
struct Frames {
	#[serde(default)]
	frames: Vec<Frame>,
}

impl Stream {
	pub fn is_audio(&self) -> bool {
		self.codec_type == "audio"
	}

	pub fn is_video(&self) -> bool {
		self.codec_type == "video"
	}

	pub fn is_subtitle(&self) -> bool {
		self.codec_type == "subtitle"
	}

	pub fn is_attached_pic(&self) -> bool {
		self.disposition.get("attached_pic") == Some(&1)
	}

	pub fn is_real_video(&self) -> bool {
		self.is_video() && !self.is_attached_pic() && self.disposition.get("timed_thumbnails") != Some(&1)
	}

	pub fn tag(&self, key: &str) -> Option<&str> {
		self.tags
			.iter()
			.find(|(name, _)| name.eq_ignore_ascii_case(key))
			.map(|(_, value)| value.as_str())
	}

	/// Matroska's `alpha_mode`: the stream carries an alpha channel.
	pub fn alpha_signalled(&self) -> bool {
		self.tag("alpha_mode") == Some("1")
	}

	pub fn side_data(&self, kind: &str) -> Option<&SideData> {
		self.side_data_list.iter().find(|data| data.side_data_type == kind)
	}

	#[allow(clippy::cast_possible_truncation)]
	pub fn rotation(&self) -> i32 {
		self.side_data("Display Matrix")
			.and_then(|matrix| matrix.number("rotation"))
			.map_or(0, |degrees| degrees.round() as i32)
	}

	pub fn declared_duration(&self) -> Option<f64> {
		self.duration.or_else(|| self.tag("DURATION").and_then(parse_clock))
	}

	pub fn fps(&self) -> Option<f64> {
		self.avg_frame_rate.or(self.r_frame_rate)
	}
}

impl SideData {
	pub fn number(&self, key: &str) -> Option<f64> {
		match self.fields.get(key)? {
			Value::Number(value) => value.as_f64(),
			Value::String(text) => parse_ratio(text),
			_ => None,
		}
	}
}

fn parse_ratio(text: &str) -> Option<f64> {
	let Some((numerator, denominator)) = text.split_once('/') else {
		return text.parse().ok();
	};

	let (numerator, denominator): (f64, f64) = (numerator.trim().parse().ok()?, denominator.trim().parse().ok()?);
	(denominator != 0.0).then(|| numerator / denominator)
}

fn parse_clock(text: &str) -> Option<f64> {
	let mut parts = text.split(':');
	let (hours, minutes, seconds) = (parts.next()?, parts.next()?, parts.next()?);
	Some(hours.parse::<f64>().ok()? * 3600.0 + minutes.parse::<f64>().ok()? * 60.0 + seconds.parse::<f64>().ok()?)
}

fn rational<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
	D: Deserializer<'de>,
{
	Ok(Option::<String>::deserialize(deserializer)?
		.as_deref()
		.and_then(parse_ratio)
		.filter(|rate| *rate > 0.0))
}

/// ffprobe writes aspect ratios as `64:45`, and an unknown one as `0:1`.
fn aspect<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
	D: Deserializer<'de>,
{
	Ok(Option::<String>::deserialize(deserializer)?
		.as_deref()
		.and_then(|text| parse_ratio(&text.replacen(':', "/", 1)))
		.filter(|ratio| *ratio > 0.0))
}

impl MediaInfo {
	pub fn parse(json: &[u8]) -> Result<Self, serde_json::Error> {
		serde_json::from_str(&String::from_utf8_lossy(json))
	}

	pub fn audio_streams(&self) -> impl Iterator<Item = &Stream> {
		self.streams.iter().filter(|stream| stream.is_audio())
	}

	pub fn real_video_streams(&self) -> impl Iterator<Item = &Stream> {
		self.streams.iter().filter(|stream| stream.is_real_video())
	}

	pub fn duration_shortfall(&self, decoded_s: f64) -> Option<String> {
		let declared = self.format.duration.filter(|_| !self.format.duration_estimated)?;

		(declared - decoded_s > (declared * 0.02).max(1.0))
			.then(|| format!("decodes to {decoded_s:.1} s but declares {declared:.1} s"))
	}
}

#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
	#[error(transparent)]
	Run(#[from] RunError),
	#[error("ffprobe could not read the file: {}", .0.join(" | "))]
	Failed(Vec<String>),
	#[error("ffprobe returned unreadable JSON: {0}")]
	Json(#[from] serde_json::Error),
}

pub async fn probe(tools: &Toolchain, path: &Path, cancel: &CancellationToken) -> Result<MediaInfo, ProbeError> {
	let mut cmd = ToolCommand::new(&tools.ffprobe, PROBE_TIMEOUT);
	cmd.args(JSON_ARGS).arg("-i").arg(process::tool_path(path));

	let out = run_ffprobe(&cmd, cancel).await?;
	let mut info = MediaInfo::parse(&out.stdout)?;
	info.format.duration_estimated = out.stderr_tail.iter().any(|line| line.contains(ESTIMATED_DURATION));
	Ok(info)
}

/// The video's streams, and with `count_packets` how many packets each holds (`nb_read_packets`), which reads the whole
/// file.
pub async fn probe_video(
	tools: &Toolchain,
	path: &Path,
	source_bytes: u64,
	count_packets: bool,
	cancel: &CancellationToken,
) -> Result<(MediaInfo, Vec<u8>), ProbeError> {
	let mut cmd = ToolCommand::new(&tools.ffprobe, process::scaled_timeout(source_bytes));
	cmd.args(JSON_ARGS).arg("-show_programs");
	if count_packets {
		cmd.arg("-count_packets");
	}
	cmd.arg("-i").arg(process::tool_path(path));

	let out = run_ffprobe(&cmd, cancel).await?;
	let info = MediaInfo::parse(&out.stdout)?;
	Ok((info, out.stdout))
}

pub async fn first_frames(
	tools: &Toolchain,
	path: &Path,
	stream_index: u32,
	count: u32,
	cancel: &CancellationToken,
) -> Result<Vec<Frame>, ProbeError> {
	let mut cmd = ToolCommand::new(&tools.ffprobe, PROBE_TIMEOUT);
	cmd.args([
		"-v",
		"error",
		"-hide_banner",
		"-threads",
		"0",
		"-export_side_data",
		"film_grain",
		"-print_format",
		"json",
		"-select_streams",
	])
	.arg(stream_index.to_string())
	.args(["-show_frames", "-read_intervals"])
	.arg(format!("%+#{count}"))
	.args([
		"-show_entries",
		"frame=color_range,color_space,color_transfer,color_primaries,side_data_list",
		"-i",
	])
	.arg(process::tool_path(path));

	let out = run_ffprobe(&cmd, cancel).await?;
	let frames: Frames = serde_json::from_str(&String::from_utf8_lossy(&out.stdout))?;
	Ok(frames.frames)
}

pub async fn run_ffprobe(cmd: &ToolCommand, cancel: &CancellationToken) -> Result<ToolOutput, ProbeError> {
	let out = process::run(cmd, StdoutMode::Capture, |_| {}, cancel).await?;
	if !out.status.success() {
		return Err(ProbeError::Failed(out.stderr_tail));
	}

	Ok(out)
}

/// Whether an AV1 stream's first frame header carries film grain parameters, read from the bitstream rather than from
/// the decoder: libaom-av1 applies the grain without exporting it. The trace shows `apply_grain` only when the
/// sequence header sets `film_grain_params_present`. A crashed probe counts as grain, so that grain is never added on
/// top of grain the source may already carry.
pub async fn av1_film_grain(
	tools: &Toolchain,
	path: &Path,
	stream_index: u32,
	cancel: &CancellationToken,
) -> Result<bool, ProbeError> {
	let mut cmd = ToolCommand::new(&tools.ffmpeg, PROBE_TIMEOUT);
	cmd.count_lines_containing(AV1_GRAIN_TRACE);
	cmd.args(["-hide_banner", "-nostdin", "-i"])
		.arg(process::tool_path(path))
		.arg("-map")
		.arg(format!("0:{stream_index}"))
		.args([
			"-c",
			"copy",
			"-bsf:v",
			"trace_headers",
			"-frames:v",
			"1",
			"-f",
			"null",
			"-",
		]);

	match process::run(&cmd, StdoutMode::Capture, |_| {}, cancel).await {
		Ok(out) => Ok(out.status.success() && out.counted_lines > 0),
		Err(err) => grain_after_failure(err),
	}
}

fn grain_after_failure(err: RunError) -> Result<bool, ProbeError> {
	match err {
		RunError::Crashed { .. } => Ok(true),
		err => Err(err.into()),
	}
}

fn number<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
	D: Deserializer<'de>,
	T: FromStr,
{
	Ok(match Option::<serde_json::Value>::deserialize(deserializer)? {
		Some(serde_json::Value::String(text)) => text.parse().ok(),
		Some(serde_json::Value::Number(value)) => value.to_string().parse().ok(),
		_ => None,
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[cfg(windows)]
	fn crashed_status() -> std::process::ExitStatus {
		std::os::windows::process::ExitStatusExt::from_raw(0xC000_0005)
	}

	#[cfg(unix)]
	fn crashed_status() -> std::process::ExitStatus {
		std::os::unix::process::ExitStatusExt::from_raw(11)
	}

	#[test]
	fn a_grain_probe_that_crashed_counts_as_grain() {
		let crashed = RunError::Crashed {
			tool: "ffmpeg".into(),
			description: "access violation".into(),
			status: crashed_status(),
			stderr_tail: Vec::new(),
		};

		assert!(matches!(grain_after_failure(crashed), Ok(true)));
		assert!(grain_after_failure(RunError::Cancelled).is_err());
	}

	const MP3_WITH_COVER: &str = r#"{
		"streams": [
			{ "index": 0, "codec_name": "mp3", "codec_type": "audio", "sample_rate": "44100", "channels": 2,
			  "channel_layout": "stereo", "bit_rate": "342191", "disposition": { "default": 0, "attached_pic": 0 } },
			{ "index": 1, "codec_name": "mjpeg", "codec_type": "video", "width": 492, "height": 704,
			  "disposition": { "default": 0, "attached_pic": 1 }, "tags": { "comment": "Cover (front)" } }
		],
		"format": { "format_name": "mp3", "duration": "34.360000", "size": "1469712", "bit_rate": "342191",
		            "tags": { "title": "さくら" } }
	}"#;

	#[test]
	fn parses_string_numbers_and_dispositions() {
		let info = MediaInfo::parse(MP3_WITH_COVER.as_bytes()).unwrap();

		assert_eq!(info.format.duration, Some(34.36));
		assert_eq!(info.format.bit_rate, Some(342_191));
		assert!(info.streams[0].is_audio());
		assert_eq!(info.streams[0].sample_rate, Some(44_100));
		assert_eq!(info.streams[0].bit_rate, Some(342_191));
		assert!(info.streams[1].is_video());
		assert!(info.streams[1].is_attached_pic());
	}

	#[test]
	fn tolerates_missing_and_na_fields() {
		let info = MediaInfo::parse(br#"{ "streams": [ { "index": 0, "codec_type": "audio", "bit_rate": "N/A" } ] }"#)
			.unwrap();

		assert_eq!(info.streams[0].bit_rate, None);
		assert_eq!(info.format.duration, None);
	}

	#[test]
	fn replaces_invalid_utf8() {
		let info = MediaInfo::parse(b"{\"format\":{\"format_name\":\"wav\xff\"}}").unwrap();

		assert_eq!(info.format.format_name, "wav\u{fffd}");
	}

	#[test]
	fn shortfall_uses_only_trusted_durations() {
		let mut info = MediaInfo::default();
		info.format.duration = Some(35.8);

		assert!(info.duration_shortfall(14.0).unwrap().contains("declares 35.8 s"));
		assert_eq!(info.duration_shortfall(35.3), None);

		info.format.duration = Some(100.0);
		assert!(info.duration_shortfall(97.5).is_some());

		info.format.duration_estimated = true;
		assert_eq!(info.duration_shortfall(10.0), None);
	}

	const IPHONE_VIDEO: &str = r#"{
		"streams": [
			{ "index": 0, "codec_name": "hevc", "codec_type": "video", "codec_tag_string": "hvc1", "width": 1920,
			  "height": 1080, "pix_fmt": "yuv420p10le", "color_range": "tv", "color_space": "bt2020nc",
			  "color_transfer": "arib-std-b67", "color_primaries": "bt2020", "r_frame_rate": "30000/1001",
			  "avg_frame_rate": "233850/7801", "start_time": "0.021000", "duration": "52.005000", "nb_read_packets": "1559",
			  "disposition": { "default": 1, "attached_pic": 0, "timed_thumbnails": 0 },
			  "tags": { "handler_name": "Core Media Video" },
			  "side_data_list": [
				{ "side_data_type": "DOVI configuration record", "dv_profile": 8, "dv_level": 4,
				  "dv_bl_signal_compatibility_id": 4 },
				{ "side_data_type": "Display Matrix", "displaymatrix": "\n00000000: 0 65536 0\n", "rotation": -90 }
			  ] },
			{ "index": 2, "codec_type": "data", "codec_tag_string": "mebx", "tags": { "handler_name": "Core Media Metadata" } }
		],
		"chapters": [ { "id": 1, "start_time": "0.000000" } ],
		"format": { "format_name": "mov,mp4,m4a,3gp,3g2,mj2", "start_time": "-1.400000", "duration": "52.005000",
		            "tags": { "com.apple.quicktime.make": "Apple" } }
	}"#;

	#[test]
	fn parses_video_fields_side_data_and_chapters() {
		let info = MediaInfo::parse(IPHONE_VIDEO.as_bytes()).unwrap();
		let video = &info.streams[0];

		assert!(video.is_real_video());
		assert_eq!((video.width, video.height), (Some(1920), Some(1080)));
		assert_eq!(video.color_transfer.as_deref(), Some("arib-std-b67"));
		assert!((video.fps().unwrap() - 29.977).abs() < 0.001);
		assert_eq!(video.nb_read_packets, Some(1559));
		assert_eq!(video.start_time, Some(0.021));
		assert_eq!(info.format.start_time, Some(-1.4));
		assert_eq!(video.rotation(), -90);
		let dovi = video.side_data("DOVI configuration record").unwrap();
		assert_eq!(dovi.number("dv_profile"), Some(8.0));
		assert_eq!(info.streams[1].codec_name, "");
		assert_eq!(info.streams[1].codec_tag_string, "mebx");
		assert_eq!(info.chapters.len(), 1);
		assert_eq!(
			info.format.tags.get("com.apple.quicktime.make").map(String::as_str),
			Some("Apple")
		);
	}

	#[test]
	fn untagged_colour_and_missing_rates_are_none() {
		let info = MediaInfo::parse(
			br#"{ "streams": [ { "index": 0, "codec_type": "video", "codec_name": "mpeg2video",
			      "color_range": "tv", "r_frame_rate": "0/0" } ] }"#,
		)
		.unwrap();
		let video = &info.streams[0];

		assert_eq!(video.color_space, None);
		assert_eq!(video.color_range.as_deref(), Some("tv"));
		assert_eq!(video.r_frame_rate, None);
		assert_eq!(video.rotation(), 0);
	}

	#[test]
	fn tags_are_case_insensitive_and_matroska_durations_parse() {
		let info = MediaInfo::parse(
			br#"{ "streams": [ { "index": 0, "codec_type": "video", "codec_name": "vp9",
			      "tags": { "ALPHA_MODE": "1", "DURATION": "00:01:02.500000000" } } ] }"#,
		)
		.unwrap();
		let video = &info.streams[0];

		assert_eq!(video.tag("alpha_mode"), Some("1"));
		assert_eq!(video.declared_duration(), Some(62.5));
	}

	#[test]
	fn sample_aspect_ratios_are_fractions_and_unknown_ones_are_none() {
		let info = MediaInfo::parse(
			br#"{ "streams": [ { "index": 0, "codec_type": "video", "sample_aspect_ratio": "64:45" },
			      { "index": 1, "codec_type": "video", "sample_aspect_ratio": "0:1" },
			      { "index": 2, "codec_type": "video", "sample_aspect_ratio": "1:1" },
			      { "index": 3, "codec_type": "video" } ] }"#,
		)
		.unwrap();
		let aspects: Vec<Option<f64>> = info.streams.iter().map(|stream| stream.sample_aspect_ratio).collect();

		assert_eq!(aspects, [Some(64.0 / 45.0), None, Some(1.0), None]);
	}

	#[test]
	fn cover_streams_are_not_real_video() {
		let info = MediaInfo::parse(
			br#"{ "streams": [ { "index": 2, "codec_type": "video", "codec_name": "mjpeg",
			      "disposition": { "attached_pic": 1 } } ] }"#,
		)
		.unwrap();

		assert!(!info.streams[0].is_real_video());
	}

	#[test]
	fn side_data_fractions_become_numbers() {
		let frame: Frame = serde_json::from_str(
			r#"{ "color_transfer": "smpte2084", "side_data_list": [ { "side_data_type": "Mastering display metadata",
			     "red_x": "34000/50000", "max_luminance": "10000000/10000" } ] }"#,
		)
		.unwrap();
		let mastering = &frame.side_data_list[0];

		assert_eq!(mastering.number("red_x"), Some(0.68));
		assert_eq!(mastering.number("max_luminance"), Some(1000.0));
		assert_eq!(mastering.number("missing"), None);
	}
}
