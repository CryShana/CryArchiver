use std::fmt;

use crate::images::decode::deep_pix_fmt;
use crate::metadata::ImageFormat;
use crate::probe::Stream;
use crate::video::analyze::{Analysis, NoiseClass, ScanType};
use crate::video::plan::{Resolution, VideoPlan};

const MPEG4: &[&str] = &["mpeg4", "msmpeg4v2", "msmpeg4v3", "h263", "flv1"];
const INTERMEDIATE: &[&str] = &["prores", "dnxhd", "cfhd", "hqx", "hap"];
const LOSSLESS: &[&str] = &[
	"rawvideo", "ffv1", "utvideo", "huffyuv", "ffvhuff", "magicyuv", "png", "qtrle", "v210", "r210",
];
const TELECINE_KEEPS: f64 = 0.8;
const RATE_SLACK: f64 = 0.01;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecFamily {
	H264,
	Hevc,
	Mpeg2,
	Mpeg4,
	Vp8,
	Vp9,
	Av1,
	Intermediate,
	Lossless,
	Other,
}

impl CodecFamily {
	pub fn of(codec: &str) -> Self {
		match codec {
			"h264" => Self::H264,
			"hevc" => Self::Hevc,
			"mpeg1video" | "mpeg2video" => Self::Mpeg2,
			"vp8" => Self::Vp8,
			"vp9" => Self::Vp9,
			"av1" => Self::Av1,
			_ if MPEG4.contains(&codec) => Self::Mpeg4,
			_ if INTERMEDIATE.contains(&codec) => Self::Intermediate,
			_ if LOSSLESS.contains(&codec) => Self::Lossless,
			_ => Self::Other,
		}
	}

	fn name(self) -> &'static str {
		match self {
			Self::H264 => "h264",
			Self::Hevc => "hevc",
			Self::Mpeg2 => "mpeg2",
			Self::Mpeg4 => "mpeg4",
			Self::Vp8 => "vp8",
			Self::Vp9 => "vp9",
			Self::Av1 => "av1",
			Self::Intermediate => "intermediate",
			Self::Lossless => "lossless",
			Self::Other => "other",
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FpsBucket {
	UpTo25,
	UpTo31,
	UpTo50,
	UpTo61,
	Above61,
	Unknown,
}

impl FpsBucket {
	pub fn of(fps: Option<f64>) -> Self {
		match fps {
			Some(fps) if fps <= 25.0 + RATE_SLACK => Self::UpTo25,
			Some(fps) if fps <= 31.0 => Self::UpTo31,
			Some(fps) if fps <= 50.0 + RATE_SLACK => Self::UpTo50,
			Some(fps) if fps <= 61.0 => Self::UpTo61,
			Some(_) => Self::Above61,
			None => Self::Unknown,
		}
	}

	fn name(self) -> &'static str {
		match self {
			Self::UpTo25 => "fps25",
			Self::UpTo31 => "fps31",
			Self::UpTo50 => "fps50",
			Self::UpTo61 => "fps61",
			Self::Above61 => "fps61+",
			Self::Unknown => "fps?",
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DynamicRange {
	Sdr8,
	Sdr10,
	Pq,
	Hlg,
}

impl DynamicRange {
	pub fn of(transfer: &str, pix_fmt: Option<&str>) -> Self {
		match transfer {
			"smpte2084" => Self::Pq,
			"arib-std-b67" => Self::Hlg,
			_ if pix_fmt.is_some_and(deep_pix_fmt) => Self::Sdr10,
			_ => Self::Sdr8,
		}
	}

	fn name(self) -> &'static str {
		match self {
			Self::Sdr8 => "sdr8",
			Self::Sdr10 => "sdr10",
			Self::Pq => "pq",
			Self::Hlg => "hlg",
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoGroup {
	pub codec: CodecFamily,
	pub resolution: Resolution,
	pub fps: FpsBucket,
	pub range: DynamicRange,
	pub scan: ScanType,
	pub grain: Option<u8>,
	/// Noisy to the encoder though no grain is planned (Fast, or grain the decoder makes up).
	pub noisy: bool,
	/// The analysis found no still flat area to measure the noise on, so neither noisy nor clean is known.
	pub unmeasured: bool,
	pub alpha: bool,
}

impl VideoGroup {
	pub fn of(main: &Stream, analysis: &Analysis, plan: &VideoPlan) -> Self {
		let output_fps = main.fps().map(|fps| match analysis.scan {
			ScanType::Progressive => fps,
			ScanType::Interlaced { .. } => fps * 2.0,
			ScanType::Telecined => fps * TELECINE_KEEPS,
		});

		Self {
			codec: CodecFamily::of(&main.codec_name),
			resolution: Resolution::of(plan.width, plan.height),
			fps: FpsBucket::of(output_fps),
			range: DynamicRange::of(&plan.colour.transfer, main.pix_fmt.as_deref()),
			scan: analysis.scan,
			grain: plan.encoder.grain_level(),
			noisy: analysis.noise == NoiseClass::Light,
			unmeasured: analysis.noise_sigma.is_none(),
			alpha: plan.alpha,
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MegapixelBucket {
	Under1,
	Under4,
	Under12,
	Under24,
	UpTo64,
	Above64,
}

impl MegapixelBucket {
	pub fn of(width: u32, height: u32) -> Self {
		match u64::from(width) * u64::from(height) {
			..1_000_000 => Self::Under1,
			1_000_000..4_000_000 => Self::Under4,
			4_000_000..12_000_000 => Self::Under12,
			12_000_000..24_000_000 => Self::Under24,
			24_000_000..=64_000_000 => Self::UpTo64,
			_ => Self::Above64,
		}
	}

	fn name(self) -> &'static str {
		match self {
			Self::Under1 => "mp1",
			Self::Under4 => "mp4",
			Self::Under12 => "mp12",
			Self::Under24 => "mp24",
			Self::UpTo64 => "mp64",
			Self::Above64 => "mp64+",
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageGroup {
	pub format: ImageFormat,
	pub graphic: bool,
	pub alpha: bool,
	pub deep: bool,
	pub megapixels: MegapixelBucket,
}

impl ImageGroup {
	pub fn new(format: ImageFormat, graphic: bool, alpha: bool, deep: bool, width: u32, height: u32) -> Self {
		Self {
			format,
			graphic,
			alpha,
			deep,
			megapixels: MegapixelBucket::of(width, height),
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
	Video(VideoGroup),
	Image(ImageGroup),
}

#[derive(Debug, Clone, PartialEq)]
pub struct CacheKey {
	pub group: Group,
	pub encoder: String,
	pub preset: u8,
	pub target: f64,
	pub floor: Option<f64>,
}

impl fmt::Display for CacheKey {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self.group {
			Group::Video(group) => write!(
				f,
				"video/{}/{}/{}/{}/{}/{}/{}",
				group.codec.name(),
				resolution_name(group.resolution),
				group.fps.name(),
				group.range.name(),
				scan_name(group.scan),
				grain_name(group.grain, group.noisy, group.unmeasured),
				alpha_name(group.alpha)
			)?,
			Group::Image(group) => write!(
				f,
				"image/{}/{}/{}/{}/{}",
				format_name(group.format),
				if group.graphic { "graphic" } else { "photo" },
				alpha_name(group.alpha),
				if group.deep { "deep" } else { "8bit" },
				group.megapixels.name()
			)?,
		}

		write!(f, "|{}|p{}|t{:.1}", self.encoder, self.preset, self.target)?;
		if let Some(floor) = self.floor {
			write!(f, "/{floor:.1}")?;
		}
		Ok(())
	}
}

fn resolution_name(resolution: Resolution) -> &'static str {
	match resolution {
		Resolution::Sd => "sd",
		Resolution::Hd720 => "720",
		Resolution::Hd1080 => "1080",
		Resolution::Qhd1440 => "1440",
		Resolution::Uhd4k => "4k",
		Resolution::Uhd8k => "8k",
	}
}

fn scan_name(scan: ScanType) -> &'static str {
	match scan {
		ScanType::Progressive => "progressive",
		ScanType::Interlaced { .. } => "interlaced",
		ScanType::Telecined => "telecined",
	}
}

fn grain_name(grain: Option<u8>, noisy: bool, unmeasured: bool) -> String {
	match (grain, noisy, unmeasured) {
		(Some(level), _, _) => format!("grain{level}"),
		(None, true, _) => "noisy".to_owned(),
		(None, false, true) => "unmeasured".to_owned(),
		(None, false, false) => "clean".to_owned(),
	}
}

fn alpha_name(alpha: bool) -> &'static str {
	if alpha { "alpha" } else { "opaque" }
}

fn format_name(format: ImageFormat) -> &'static str {
	match format {
		ImageFormat::Jpeg => "jpeg",
		ImageFormat::Png => "png",
		ImageFormat::Gif => "gif",
		ImageFormat::WebP => "webp",
		ImageFormat::Tiff => "tiff",
		ImageFormat::Heif => "heif",
		ImageFormat::Avif => "avif",
		ImageFormat::Jxl => "jxl",
		ImageFormat::Other => "other",
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::video::plan::{Encoder, test_plan};

	fn stream(json: &str) -> Stream {
		serde_json::from_str(json).unwrap()
	}

	fn progressive(noise_sigma: Option<f64>) -> Analysis {
		Analysis {
			scan: ScanType::Progressive,
			noise: if noise_sigma.is_some_and(|sigma| sigma >= 0.4) {
				NoiseClass::Light
			} else {
				NoiseClass::Clean
			},
			noise_sigma,
			detail: Vec::new(),
		}
	}

	fn grained(film_grain: Option<u8>) -> VideoPlan {
		VideoPlan {
			encoder: Encoder::SvtAv1 {
				preset: 5,
				crf: 29.0,
				film_grain,
			},
			..test_plan()
		}
	}

	fn h264_25() -> Stream {
		stream(
			r#"{"index":0,"codec_type":"video","codec_name":"h264","pix_fmt":"yuv420p",
			"r_frame_rate":"25/1","avg_frame_rate":"25/1"}"#,
		)
	}

	#[test]
	fn codecs_fall_into_families() {
		assert_eq!(CodecFamily::of("h264"), CodecFamily::H264);
		assert_eq!(CodecFamily::of("mpeg2video"), CodecFamily::Mpeg2);
		assert_eq!(CodecFamily::of("msmpeg4v3"), CodecFamily::Mpeg4);
		assert_eq!(CodecFamily::of("prores"), CodecFamily::Intermediate);
		assert_eq!(CodecFamily::of("ffv1"), CodecFamily::Lossless);
		assert_eq!(CodecFamily::of("theora"), CodecFamily::Other);
	}

	#[test]
	fn frame_rates_fall_into_the_spec_buckets() {
		assert_eq!(FpsBucket::of(Some(23.976)), FpsBucket::UpTo25);
		assert_eq!(FpsBucket::of(Some(25.0)), FpsBucket::UpTo25);
		assert_eq!(FpsBucket::of(Some(29.97)), FpsBucket::UpTo31);
		assert_eq!(FpsBucket::of(Some(50.0)), FpsBucket::UpTo50);
		assert_eq!(FpsBucket::of(Some(59.94)), FpsBucket::UpTo61);
		assert_eq!(FpsBucket::of(Some(119.88)), FpsBucket::Above61);
		assert_eq!(FpsBucket::of(None), FpsBucket::Unknown);
	}

	#[test]
	fn dynamic_range_comes_from_the_transfer_and_the_source_depth() {
		assert_eq!(DynamicRange::of("smpte2084", Some("yuv420p10le")), DynamicRange::Pq);
		assert_eq!(DynamicRange::of("arib-std-b67", Some("yuv420p10le")), DynamicRange::Hlg);
		assert_eq!(DynamicRange::of("bt709", Some("yuv420p10le")), DynamicRange::Sdr10);
		assert_eq!(DynamicRange::of("bt709", Some("yuv420p")), DynamicRange::Sdr8);
		assert_eq!(DynamicRange::of("bt709", Some("nv12")), DynamicRange::Sdr8);
		assert_eq!(DynamicRange::of("bt709", None), DynamicRange::Sdr8);
	}

	#[test]
	fn deinterlaced_video_is_grouped_by_its_output_rate() {
		let main = stream(
			r#"{"index":0,"codec_type":"video","codec_name":"mpeg2video","pix_fmt":"yuv420p",
			"r_frame_rate":"30000/1001","avg_frame_rate":"30000/1001"}"#,
		);
		let interlaced = Analysis {
			scan: ScanType::Interlaced { top_first: true },
			..progressive(None)
		};
		let plan = VideoPlan {
			width: 1920,
			height: 1080,
			..test_plan()
		};

		let group = VideoGroup::of(&main, &interlaced, &plan);

		assert_eq!(group.fps, FpsBucket::UpTo61);
		assert_eq!(group.codec, CodecFamily::Mpeg2);
		assert_eq!(group.resolution, Resolution::Hd1080);
	}

	#[test]
	fn cache_keys_name_the_encoder_version_preset_and_target() {
		let main = h264_25();
		let analysis = progressive(Some(1.2));
		let key = |encoder: &str, preset: u8, target: f64| {
			CacheKey {
				group: Group::Video(VideoGroup::of(&main, &analysis, &grained(Some(14)))),
				encoder: encoder.to_owned(),
				preset,
				target,
				floor: Some(89.0),
			}
			.to_string()
		};

		let base = key("svt-av1-4.2", 5, 93.0);

		assert_eq!(
			base,
			"video/h264/sd/fps25/sdr8/progressive/grain14/opaque|svt-av1-4.2|p5|t93.0/89.0"
		);
		for other in [
			key("svt-av1-4.3", 5, 93.0),
			key("svt-av1-4.2", 4, 93.0),
			key("svt-av1-4.2", 5, 95.0),
		] {
			assert_ne!(other, base);
		}
	}

	#[test]
	fn groups_of_different_planned_grain_never_share_a_key() {
		let main = h264_25();
		let key = |noise_sigma: Option<f64>, film_grain: Option<u8>| {
			CacheKey {
				group: Group::Video(VideoGroup::of(&main, &progressive(noise_sigma), &grained(film_grain))),
				encoder: "svt-av1-4.2".to_owned(),
				preset: 5,
				target: 91.0,
				floor: Some(86.5),
			}
			.to_string()
		};

		let clean = key(Some(0.1), None);
		let grain8 = key(Some(0.5), Some(8));
		let grain14 = key(Some(1.2), Some(14));
		let grain20 = key(Some(2.2), Some(20));

		assert!(clean.contains("/progressive/clean/"), "{clean}");
		assert!(grain8.contains("/progressive/grain8/"), "{grain8}");
		assert!(grain14.contains("/progressive/grain14/"), "{grain14}");
		assert!(grain20.contains("/progressive/grain20/"), "{grain20}");
		assert_ne!(grain8, grain20);

		let noisy_without_grain = key(Some(1.2), None);
		assert!(
			noisy_without_grain.contains("/progressive/noisy/"),
			"{noisy_without_grain}"
		);
		assert_ne!(noisy_without_grain, clean);
	}

	#[test]
	fn a_source_whose_noise_could_not_be_measured_has_its_own_group() {
		let main = h264_25();
		let key = |noise_sigma: Option<f64>| {
			CacheKey {
				group: Group::Video(VideoGroup::of(&main, &progressive(noise_sigma), &grained(None))),
				encoder: "svt-av1-4.2".to_owned(),
				preset: 5,
				target: 93.0,
				floor: Some(89.0),
			}
			.to_string()
		};

		let unmeasured = key(None);

		assert!(unmeasured.contains("/progressive/unmeasured/"), "{unmeasured}");
		assert!(key(Some(0.1)).contains("/progressive/clean/"));
	}

	#[test]
	fn image_keys_name_format_kind_alpha_depth_and_size() {
		let key = CacheKey {
			group: Group::Image(ImageGroup::new(ImageFormat::Jpeg, false, false, false, 6000, 4000)),
			encoder: "ravif-0.13".into(),
			preset: 6,
			target: 85.0,
			floor: None,
		};

		assert_eq!(key.to_string(), "image/jpeg/photo/opaque/8bit/mp64|ravif-0.13|p6|t85.0");
		assert_eq!(MegapixelBucket::of(320, 240), MegapixelBucket::Under1);
		assert_eq!(MegapixelBucket::of(4000, 3000), MegapixelBucket::Under24);
		assert_eq!(MegapixelBucket::of(1293, 48_277), MegapixelBucket::UpTo64);
		assert_eq!(MegapixelBucket::of(10_800, 6_000), MegapixelBucket::Above64);
	}
}
