use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Deserializer};
use tokio_util::sync::CancellationToken;

use crate::process::{self, RunError, StdoutMode, ToolCommand};
use crate::toolchain::Toolchain;

const PROBE_TIMEOUT: Duration = Duration::from_secs(120);
const ESTIMATED_DURATION: &str = "Estimating duration from bitrate";

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MediaInfo {
	#[serde(default)]
	pub streams: Vec<Stream>,
	#[serde(default)]
	pub format: Format,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Format {
	#[serde(default)]
	pub format_name: String,
	#[serde(default, deserialize_with = "number")]
	pub duration: Option<f64>,
	#[serde(default, deserialize_with = "number")]
	pub bit_rate: Option<u64>,
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
	pub disposition: BTreeMap<String, u8>,
}

impl Stream {
	pub fn is_audio(&self) -> bool {
		self.codec_type == "audio"
	}

	pub fn is_video(&self) -> bool {
		self.codec_type == "video"
	}

	pub fn is_attached_pic(&self) -> bool {
		self.disposition.get("attached_pic") == Some(&1)
	}
}

impl MediaInfo {
	pub fn parse(json: &[u8]) -> Result<Self, serde_json::Error> {
		serde_json::from_str(&String::from_utf8_lossy(json))
	}

	pub fn audio_streams(&self) -> impl Iterator<Item = &Stream> {
		self.streams.iter().filter(|stream| stream.is_audio())
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
	cmd.args([
		"-v",
		"warning",
		"-hide_banner",
		"-print_format",
		"json",
		"-show_format",
		"-show_streams",
	])
	.arg(path);

	let out = process::run(&cmd, StdoutMode::Capture, |_| {}, cancel).await?;
	if !out.status.success() {
		return Err(ProbeError::Failed(out.stderr_tail));
	}

	let mut info = MediaInfo::parse(&out.stdout)?;
	info.format.duration_estimated = out.stderr_tail.iter().any(|line| line.contains(ESTIMATED_DURATION));
	Ok(info)
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

	const MP3_WITH_COVER: &str = r#"{
		"streams": [
			{ "index": 0, "codec_name": "mp3", "codec_type": "audio", "sample_rate": "44100", "channels": 2,
			  "channel_layout": "stereo", "bit_rate": "342191", "disposition": { "default": 0, "attached_pic": 0 } },
			{ "index": 1, "codec_name": "mjpeg", "codec_type": "video", "width": 492, "height": 704,
			  "disposition": { "default": 0, "attached_pic": 1 }, "tags": { "comment": "Cover (front)" } }
		],
		"format": { "format_name": "mp3", "duration": "34.360000", "size": "1469712", "bit_rate": "342191",
		            "tags": { "title": "テスト" } }
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
}
