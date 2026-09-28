use crate::container::{Container, MATROSKA_AUDIO, OGG_OPUS};
use crate::outcome::KeepReason;
use crate::preset::{AUDIO_STEP_KBPS, Overrides, Speed};
use crate::probe::{MediaInfo, Stream};

pub const OPUS_RATE: u32 = 48_000;

const MAX_CHANNELS: u32 = 8;
const EFFICIENT_HEADROOM: f64 = 1.2;
const RELABEL: &[(&str, &str)] = &[
	("5.1(side)", "5.1"),
	("5.0(side)", "5.0"),
	("quad(side)", "quad"),
	("downmix", "stereo"),
];
const LOSSLESS: &[&str] = &["flac", "alac", "ape", "wavpack", "tta", "truehd", "mlp", "tak"];
const EFFICIENT: &[&str] = &["opus", "vorbis"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutFix {
	Keep(&'static str),
	Relabel(&'static str),
	Remix(&'static str),
}

impl LayoutFix {
	pub fn target(self) -> &'static str {
		let (Self::Keep(layout) | Self::Relabel(layout) | Self::Remix(layout)) = self;
		layout
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodePlan {
	pub bitrate_kbps: u32,
	pub channels: u32,
	pub layout: LayoutFix,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackPlan {
	Encode(EncodePlan),
	Copy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackJob {
	pub stream_index: u32,
	pub sample_rate: u32,
	pub plan: TrackPlan,
}

impl TrackJob {
	pub fn output_rate(&self) -> u32 {
		match self.plan {
			TrackPlan::Encode(_) => OPUS_RATE,
			TrackPlan::Copy => self.sample_rate,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePlan {
	pub container: Container,
	pub tracks: Vec<TrackJob>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioPlan {
	Keep(KeepReason),
	Encode(FilePlan),
}

pub fn plan_file(info: &MediaInfo, speed: Speed, overrides: &Overrides) -> AudioPlan {
	let streams: Vec<&Stream> = info.audio_streams().collect();
	if streams.is_empty() {
		return AudioPlan::Keep(KeepReason::Unsupported("no audio streams".into()));
	}

	let container_bitrate = if info.streams.len() == 1 {
		info.format.bit_rate
	} else {
		None
	};

	let mut tracks = Vec::with_capacity(streams.len());
	for stream in streams {
		let Some(sample_rate) = stream.sample_rate.filter(|&rate| rate > 0) else {
			return AudioPlan::Keep(KeepReason::Unsupported("unknown sample rate".into()));
		};

		match plan_track(stream, container_bitrate, speed, overrides) {
			Ok(plan) => tracks.push(TrackJob {
				stream_index: stream.index,
				sample_rate,
				plan,
			}),
			Err(reason) => return AudioPlan::Keep(KeepReason::Unsupported(reason)),
		}
	}

	if tracks.iter().all(|track| track.plan == TrackPlan::Copy) {
		return AudioPlan::Keep(KeepReason::AlreadyEfficient(
			"already Opus or Vorbis at an efficient bitrate".into(),
		));
	}

	let container = if tracks.len() == 1 { OGG_OPUS } else { MATROSKA_AUDIO };
	AudioPlan::Encode(FilePlan { container, tracks })
}

pub fn plan_track(
	stream: &Stream,
	container_bitrate: Option<u64>,
	speed: Speed,
	overrides: &Overrides,
) -> Result<TrackPlan, String> {
	let Some(channels) = stream.channels.filter(|&count| count > 0) else {
		return Err("unknown channel count".into());
	};

	if channels > MAX_CHANNELS {
		return Err(format!(
			"{channels} channels is more than Opus supports ({MAX_CHANNELS})"
		));
	}

	let target = table_rate(channels, speed, overrides);
	let source_kbps = stream
		.bit_rate
		.or(container_bitrate)
		.map(|bps| u32::try_from(bps / 1000).unwrap_or(u32::MAX));

	let efficient = EFFICIENT.contains(&stream.codec_name.as_str());
	if efficient && source_kbps.is_none_or(|kbps| f64::from(kbps) <= f64::from(target) * EFFICIENT_HEADROOM) {
		return Ok(TrackPlan::Copy);
	}

	let bitrate_kbps = if is_lossless(stream) {
		target
	} else {
		let fast_floor = table_rate(channels, Speed::Fast, &Overrides::default());
		target.min(source_kbps.unwrap_or(target).max(fast_floor))
	};

	let layout = layout_fix(stream.channel_layout.as_deref(), channels);
	Ok(TrackPlan::Encode(EncodePlan {
		bitrate_kbps,
		channels,
		layout,
	}))
}

pub fn table_rate(channels: u32, speed: Speed, overrides: &Overrides) -> u32 {
	let (mono, stereo, five_one, seven_one) = match speed {
		Speed::Fast => (48, 96, 192, 288),
		Speed::Moderate => (64, 128, 256, 384),
		Speed::Slow => (80, 160, 320, 448),
	};

	let rate = match channels {
		1 => mono,
		2 => stereo,
		6 => five_one,
		8 => seven_one,
		other => other * stereo / 2,
	};

	let Some(custom) = overrides.normalized().audio_stereo_kbps else {
		return rate;
	};

	let step = u64::from(stereo * AUDIO_STEP_KBPS);
	let steps = (u64::from(rate) * u64::from(custom) + step / 2) / step;
	u32::try_from(steps * u64::from(AUDIO_STEP_KBPS)).unwrap_or(u32::MAX)
}

fn is_lossless(stream: &Stream) -> bool {
	let codec = stream.codec_name.as_str();
	LOSSLESS.contains(&codec)
		|| codec.starts_with("pcm_")
		|| (codec == "dts" && stream.profile.as_deref() == Some("DTS-HD MA"))
}

fn layout_fix(layout: Option<&str>, channels: u32) -> LayoutFix {
	let standard = standard_layout(channels);

	match layout.filter(|name| !is_unordered(name)) {
		None => LayoutFix::Relabel(standard),
		Some(name) if name == standard => LayoutFix::Keep(standard),
		Some(name) => RELABEL
			.iter()
			.find(|(from, _)| *from == name)
			.map_or(LayoutFix::Remix(standard), |(_, to)| LayoutFix::Relabel(to)),
	}
}

fn is_unordered(name: &str) -> bool {
	name.is_empty() || name == "unknown" || name.ends_with(" channels")
}

fn standard_layout(channels: u32) -> &'static str {
	match channels {
		1 => "mono",
		2 => "stereo",
		3 => "3.0",
		4 => "quad",
		5 => "5.0",
		6 => "5.1",
		7 => "6.1",
		_ => "7.1",
	}
}

#[cfg(test)]
mod tests {
	use std::collections::BTreeMap;

	use super::*;
	use crate::probe::Format;

	const PRESET: &Overrides = &Overrides {
		video: None,
		image: None,
		audio_stereo_kbps: None,
	};

	fn stream(codec: &str, channels: u32, layout: Option<&str>, kbps: Option<u64>) -> Stream {
		Stream {
			codec_type: "audio".into(),
			codec_name: codec.into(),
			sample_rate: Some(48_000),
			channels: Some(channels),
			channel_layout: layout.map(Into::into),
			bit_rate: kbps.map(|k| k * 1000),
			..Stream::default()
		}
	}

	#[allow(clippy::unnecessary_wraps)]
	fn encode(bitrate_kbps: u32, channels: u32, layout: LayoutFix) -> Result<TrackPlan, String> {
		Ok(TrackPlan::Encode(EncodePlan {
			bitrate_kbps,
			channels,
			layout,
		}))
	}

	fn info(streams: Vec<Stream>, bit_rate: Option<u64>) -> MediaInfo {
		MediaInfo {
			streams,
			format: Format {
				bit_rate,
				..Format::default()
			},
			..MediaInfo::default()
		}
	}

	#[test]
	fn lossless_sources_use_the_table_rate() {
		assert_eq!(
			plan_track(&stream("flac", 2, Some("stereo"), None), None, Speed::Moderate, PRESET),
			encode(128, 2, LayoutFix::Keep("stereo"))
		);
		assert_eq!(
			plan_track(&stream("flac", 2, Some("stereo"), None), None, Speed::Slow, PRESET),
			encode(160, 2, LayoutFix::Keep("stereo"))
		);
		assert_eq!(
			plan_track(&stream("pcm_s24le", 8, Some("7.1"), None), None, Speed::Fast, PRESET),
			encode(288, 8, LayoutFix::Keep("7.1"))
		);
	}

	#[test]
	fn a_stereo_override_scales_every_layout_in_steps_of_eight() {
		let custom = Overrides {
			audio_stereo_kbps: Some(160),
			..Overrides::default()
		};
		let rates = |speed: Speed, overrides: &Overrides| {
			[1, 2, 3, 6, 8].map(|channels| table_rate(channels, speed, overrides))
		};

		assert_eq!(rates(Speed::Moderate, &custom), [80, 160, 240, 320, 480]);
		assert_eq!(rates(Speed::Moderate, PRESET), [64, 128, 192, 256, 384]);
		assert_eq!(rates(Speed::Fast, PRESET), [48, 96, 144, 192, 288]);
		assert_eq!(rates(Speed::Slow, PRESET), [80, 160, 240, 320, 448]);
		assert_eq!(
			rates(
				Speed::Slow,
				&Overrides {
					audio_stereo_kbps: Some(96),
					..Overrides::default()
				}
			),
			[48, 96, 144, 192, 272]
		);
		assert_eq!(
			table_rate(
				1,
				Speed::Fast,
				&Overrides {
					audio_stereo_kbps: Some(72),
					..Overrides::default()
				}
			),
			40,
			"48 · 72 / 96 = 36 is a tie between 32 and 40, and rounds up"
		);
		assert_eq!(
			plan_track(&stream("flac", 6, Some("5.1"), None), None, Speed::Moderate, &custom),
			encode(320, 6, LayoutFix::Keep("5.1"))
		);
	}

	#[test]
	fn the_table_reads_the_normalized_override() {
		let stereo = |kbps: u32| {
			table_rate(
				2,
				Speed::Moderate,
				&Overrides {
					audio_stereo_kbps: Some(kbps),
					..Overrides::default()
				},
			)
		};

		assert_eq!(stereo(1000), 320);
		assert_eq!(stereo(8), 64);
		assert_eq!(stereo(100), 104);
		assert_eq!(stereo(u32::MAX), 320);
	}

	#[test]
	fn an_override_keeps_the_source_cap_and_the_fast_floor() {
		let custom = Overrides {
			audio_stereo_kbps: Some(192),
			..Overrides::default()
		};

		assert_eq!(
			plan_track(
				&stream("mp3", 2, Some("stereo"), Some(128)),
				None,
				Speed::Moderate,
				&custom
			),
			encode(128, 2, LayoutFix::Keep("stereo"))
		);
		assert_eq!(
			plan_track(&stream("mp3", 2, Some("stereo"), Some(64)), None, Speed::Fast, &custom),
			encode(96, 2, LayoutFix::Keep("stereo"))
		);
		assert_eq!(
			plan_track(
				&stream("mp3", 2, Some("stereo"), Some(320)),
				None,
				Speed::Moderate,
				&custom
			),
			encode(192, 2, LayoutFix::Keep("stereo"))
		);
	}

	#[test]
	fn dts_hd_ma_counts_as_lossless() {
		let mut dts = stream("dts", 6, Some("5.1(side)"), Some(1509));
		dts.profile = Some("DTS-HD MA".into());

		assert_eq!(
			plan_track(&dts, None, Speed::Moderate, PRESET),
			encode(256, 6, LayoutFix::Relabel("5.1"))
		);
	}

	#[test]
	fn lossy_sources_are_capped_between_the_fast_row_and_the_table() {
		assert_eq!(
			plan_track(
				&stream("ac3", 6, Some("5.1(side)"), Some(448)),
				None,
				Speed::Moderate,
				PRESET
			),
			encode(256, 6, LayoutFix::Relabel("5.1"))
		);
		assert_eq!(
			plan_track(
				&stream("mp3", 2, Some("stereo"), Some(112)),
				None,
				Speed::Moderate,
				PRESET
			),
			encode(112, 2, LayoutFix::Keep("stereo"))
		);
		assert_eq!(
			plan_track(
				&stream("mp3", 2, Some("stereo"), Some(64)),
				None,
				Speed::Moderate,
				PRESET
			),
			encode(96, 2, LayoutFix::Keep("stereo"))
		);
		assert_eq!(
			plan_track(
				&stream("aac", 2, Some("stereo"), None),
				Some(256_000),
				Speed::Moderate,
				PRESET
			),
			encode(128, 2, LayoutFix::Keep("stereo"))
		);
	}

	#[test]
	fn efficient_sources_are_copied_unless_oversized() {
		assert_eq!(
			plan_track(
				&stream("opus", 1, Some("mono"), None),
				Some(32_000),
				Speed::Moderate,
				PRESET
			),
			Ok(TrackPlan::Copy)
		);
		assert_eq!(
			plan_track(
				&stream("vorbis", 2, Some("stereo"), Some(160)),
				None,
				Speed::Moderate,
				PRESET
			),
			encode(128, 2, LayoutFix::Keep("stereo"))
		);
	}

	#[test]
	fn efficient_sources_with_unknown_bitrate_are_left_alone() {
		assert_eq!(
			plan_track(&stream("opus", 2, Some("stereo"), None), None, Speed::Moderate, PRESET),
			Ok(TrackPlan::Copy)
		);
	}

	#[test]
	fn layouts_are_relabelled_remixed_or_rejected() {
		assert_eq!(
			plan_track(
				&stream("flac", 6, Some("hexagonal"), None),
				None,
				Speed::Moderate,
				PRESET
			),
			encode(256, 6, LayoutFix::Remix("5.1"))
		);
		assert_eq!(
			plan_track(
				&stream("flac", 8, Some("7.1(wide)"), None),
				None,
				Speed::Moderate,
				PRESET
			),
			encode(384, 8, LayoutFix::Remix("7.1"))
		);
		assert_eq!(
			plan_track(&stream("flac", 3, Some("2.1"), None), None, Speed::Moderate, PRESET),
			encode(192, 3, LayoutFix::Remix("3.0"))
		);
		assert_eq!(
			plan_track(&stream("pcm_s16le", 2, None, None), None, Speed::Moderate, PRESET),
			encode(128, 2, LayoutFix::Relabel("stereo"))
		);
		assert_eq!(
			plan_track(
				&stream("pcm_s16le", 6, Some("6 channels"), None),
				None,
				Speed::Moderate,
				PRESET
			),
			encode(256, 6, LayoutFix::Relabel("5.1"))
		);
		assert!(
			plan_track(
				&stream("pcm_s16le", 10, Some("5.1.4"), None),
				None,
				Speed::Moderate,
				PRESET
			)
			.is_err()
		);
	}

	#[test]
	fn files_pick_a_container() {
		let flac = stream("flac", 2, Some("stereo"), None);
		let second = Stream {
			index: 1,
			..flac.clone()
		};

		let AudioPlan::Encode(single) = plan_file(&info(vec![flac.clone()], None), Speed::Moderate, PRESET) else {
			panic!("one track encodes")
		};
		let AudioPlan::Encode(double) = plan_file(&info(vec![flac, second], None), Speed::Moderate, PRESET) else {
			panic!("two tracks encode")
		};

		assert_eq!(single.container, OGG_OPUS);
		assert_eq!(single.tracks[0].output_rate(), OPUS_RATE);
		assert_eq!(double.container, MATROSKA_AUDIO);
		assert_eq!(
			double.tracks.iter().map(|track| track.stream_index).collect::<Vec<_>>(),
			[0, 1]
		);
	}

	#[test]
	fn whole_file_keep_rules() {
		let opus = stream("opus", 1, Some("mono"), None);
		let wide = stream("pcm_s16le", 10, Some("5.1.4"), None);
		let no_rate = Stream {
			sample_rate: None,
			..stream("flac", 2, Some("stereo"), None)
		};

		assert!(matches!(
			plan_file(&info(vec![opus], Some(32_000)), Speed::Moderate, PRESET),
			AudioPlan::Keep(KeepReason::AlreadyEfficient(_))
		));
		assert!(matches!(
			plan_file(&info(vec![wide], None), Speed::Moderate, PRESET),
			AudioPlan::Keep(KeepReason::Unsupported(_))
		));
		assert!(matches!(
			plan_file(&info(vec![no_rate], None), Speed::Moderate, PRESET),
			AudioPlan::Keep(KeepReason::Unsupported(_))
		));
	}

	#[test]
	fn cover_art_does_not_count_as_audio_bitrate() {
		let opus = stream("opus", 2, Some("stereo"), None);
		let cover = Stream {
			index: 1,
			codec_type: "video".into(),
			codec_name: "mjpeg".into(),
			disposition: BTreeMap::from([("attached_pic".to_string(), 1)]),
			..Stream::default()
		};

		assert!(matches!(
			plan_file(&info(vec![opus, cover], Some(400_000)), Speed::Moderate, PRESET),
			AudioPlan::Keep(KeepReason::AlreadyEfficient(_))
		));
	}

	#[test]
	fn files_without_audio_are_unsupported() {
		assert!(matches!(
			plan_file(&info(Vec::new(), None), Speed::Moderate, PRESET),
			AudioPlan::Keep(KeepReason::Unsupported(_))
		));
	}
}
