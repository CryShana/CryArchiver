use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use crate::options::VideoEncoderChoice;
use crate::process::{self, RunError, StdoutMode, ToolCommand, ToolOutput};
use crate::quality::vmaf;
use crate::text::display_path;

const MIN_FFMPEG: Version = Version { major: 8, minor: 1 };
const MIN_SVT_AV1: Version = Version { major: 4, minor: 0 };
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const VMAF_PROBE_INPUT: &str = "testsrc2=s=64x64:d=0.2";
const FULL_BUILD_HINT: &str = "install a full ffmpeg build (Windows: gyan.dev \"full\" or BtbN GPL builds; Linux: BtbN static builds or a distro ffmpeg with libsvtav1)";
const SOXR_PROBE: &[&str] = &[
	"-hide_banner",
	"-nostdin",
	"-f",
	"lavfi",
	"-i",
	"anullsrc=r=44100:cl=stereo",
	"-t",
	"0.1",
	"-af",
	"aresample=resampler=soxr:osr=48000",
	"-f",
	"null",
	"-",
];
const OPUS_SURROUND_PROBE: &[&str] = &[
	"-hide_banner",
	"-nostdin",
	"-f",
	"lavfi",
	"-i",
	"anullsrc=r=48000:cl=5.1",
	"-t",
	"0.1",
	"-c:a",
	"libopus",
	"-b:a",
	"256k",
	"-mapping_family",
	"1",
	"-f",
	"null",
	"-",
];
const SVT_PROBE: &[&str] = &[
	"-hide_banner",
	"-nostdin",
	"-f",
	"lavfi",
	"-i",
	"testsrc2=s=64x64:d=0.1",
	"-c:v",
	"libsvtav1",
	"-f",
	"null",
	"-",
];
const SVT_SETTINGS_PROBE: &[&str] = &[
	"-hide_banner",
	"-nostdin",
	"-f",
	"lavfi",
	"-i",
	"testsrc2=s=65x33:d=0.2,format=rgb24",
	"-vf",
	"format=yuv420p10le,setparams=range=tv:color_primaries=bt2020:color_trc=arib-std-b67:colorspace=bt2020nc",
	"-c:v",
	"libsvtav1",
	"-preset",
	"8",
	"-err_detect",
	"explode",
	"-dolbyvision",
	"0",
	"-svtav1-params",
	"tune=0:keyint=5s:crf=30.25:film-grain=8:film-grain-denoise=1",
	"-f",
	"matroska",
];
const PROBE_TAGS: [(&str, &str); 4] = [
	("color_range", "tv"),
	("color_space", "bt2020nc"),
	("color_transfer", "arib-std-b67"),
	("color_primaries", "bt2020"),
];
const VP9_ALPHA_PROBE: &[&str] = &[
	"-hide_banner",
	"-nostdin",
	"-f",
	"lavfi",
	"-i",
	"color=c=red@0.5:s=64x64:d=0.2,format=yuva420p",
	"-c:v",
	"libvpx-vp9",
	"-pix_fmt",
	"yuva420p",
	"-f",
	"webm",
];
pub const VIDEO_FILTERS: &[&str] = &[
	"bwdif",
	"idet",
	"fieldmatch",
	"decimate",
	"blackdetect",
	"freezedetect",
	"hqdn3d",
	"psnr",
	"setparams",
	"scale",
];
pub const ALPHA_FILTERS: &[&str] = &["alphaextract", "ssim"];
pub const IMAGE_FILTERS: &[&str] = &["alphaextract", "alphamerge", "signalstats", "metadata", "sidedata"];
const PAM: &str = "pam";
pub const LIBJXL: &str = "libjxl";
const AV1_DECODERS: &[&str] = &["libdav1d", "libaom-av1"];
pub const LIBAOM: &str = "libaom-av1";
const BLURAY_PROTOCOL: &str = "bluray";
const LIBAOM_FILES: &str = "animated images, video larger than 16384×8704 and --video-encoder aom fail without it";
const BLURAY_FILES: &str = "Blu-ray discs and folders can't be converted without it";
pub const LIBVPX_VP9: &str = "libvpx-vp9";
pub const SVT_AV1: &str = "svt-av1";
const EVERY_KIND: &[MediaKind] = MediaKind::ALL;
const AUDIO_AND_VIDEO: &[MediaKind] = &[MediaKind::Audio, MediaKind::Video];
const VIDEO_ONLY: &[MediaKind] = &[MediaKind::Video];
const VIDEO_AND_IMAGES: &[MediaKind] = &[MediaKind::Video, MediaKind::Image];
const IMAGES_ONLY: &[MediaKind] = &[MediaKind::Image];
const OPTIONAL: &[MediaKind] = &[];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
	Audio,
	Video,
	Image,
}

impl MediaKind {
	pub const ALL: &'static [Self] = &[Self::Audio, Self::Video, Self::Image];
}

impl fmt::Display for MediaKind {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::Audio => "audio",
			Self::Video => "video",
			Self::Image => "image",
		})
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Version {
	pub major: u32,
	pub minor: u32,
}

impl fmt::Display for Version {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{}.{}", self.major, self.minor)
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FfmpegBuild {
	Release(Version),
	Git(String),
}

#[derive(Debug, Clone, Default)]
pub struct ToolConfig {
	pub ffmpeg: Option<PathBuf>,
}

#[allow(clippy::unsafe_derive_deserialize, clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Toolchain {
	pub ffmpeg: PathBuf,
	pub ffprobe: PathBuf,
	pub build: FfmpegBuild,
	/// The version exactly as `ffmpeg -version` prints it, such as `n9.0.1-6-g9d4ca21220-20260821`.
	#[serde(default)]
	pub build_id: String,
	pub encoders: BTreeSet<String>,
	pub decoders: BTreeSet<String>,
	pub filters: BTreeSet<String>,
	pub svt_av1: Option<Version>,
	/// Every number of the SVT-AV1 version its log shows, such as `4.2.0`.
	#[serde(default)]
	pub svt_av1_release: Option<String>,
	/// The libvpx version the VP9 encoder reports, such as `1.16.0`.
	#[serde(default)]
	pub libvpx_release: Option<String>,
	pub svt_settings: bool,
	pub colour_tags: bool,
	pub vp9_alpha: bool,
	pub soxr: bool,
	pub opus_surround: bool,
	#[serde(default)]
	pub vmaf: bool,
	#[serde(default)]
	pub demuxers: BTreeSet<String>,
	#[serde(default)]
	pub protocols: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
	Ok,
	Warn,
	Missing,
}

#[derive(Debug, Clone)]
pub struct ReportLine {
	pub status: Status,
	pub label: String,
	pub detail: String,
	pub needed_for: &'static [MediaKind],
}

#[derive(Debug, thiserror::Error)]
pub enum ToolchainError {
	#[error("{0} was not found: pass --ffmpeg <path> or add it to PATH")]
	NotFound(&'static str),
	#[error("could not run {path}: {message}", path = display_path(.0), message = .1)]
	Unusable(PathBuf, String),
}

impl Toolchain {
	/// These binaries with nothing found in them: for an engine that never runs a tool, or a test that adds what it
	/// needs.
	pub fn bare(ffmpeg: PathBuf, ffprobe: PathBuf, build: FfmpegBuild) -> Self {
		Self {
			ffmpeg,
			ffprobe,
			build,
			build_id: String::new(),
			encoders: BTreeSet::new(),
			decoders: BTreeSet::new(),
			filters: BTreeSet::new(),
			svt_av1: None,
			svt_av1_release: None,
			libvpx_release: None,
			svt_settings: false,
			colour_tags: false,
			vp9_alpha: false,
			soxr: false,
			opus_surround: false,
			vmaf: false,
			demuxers: BTreeSet::new(),
			protocols: BTreeSet::new(),
		}
	}

	pub async fn detect(config: &ToolConfig, cancel: &CancellationToken) -> Result<Self, ToolchainError> {
		let (ffmpeg, ffprobe) = locate(config.ffmpeg.as_deref())?;
		Box::pin(Self::probe(ffmpeg, ffprobe, cancel)).await
	}

	async fn probe(ffmpeg: PathBuf, ffprobe: PathBuf, cancel: &CancellationToken) -> Result<Self, ToolchainError> {
		let (version, encoders, decoders, filters, soxr, opus_surround, svt_av1, video, vmaf, demuxers, protocols) = tokio::join!(
			run_ffmpeg(&ffmpeg, &["-hide_banner", "-version"], cancel),
			run_ffmpeg(&ffmpeg, &["-hide_banner", "-encoders"], cancel),
			run_ffmpeg(&ffmpeg, &["-hide_banner", "-decoders"], cancel),
			run_ffmpeg(&ffmpeg, &["-hide_banner", "-filters"], cancel),
			succeeds(&ffmpeg, SOXR_PROBE, cancel),
			succeeds(&ffmpeg, OPUS_SURROUND_PROBE, cancel),
			svt_version(&ffmpeg, cancel),
			video_probes(&ffmpeg, &ffprobe, cancel),
			vmaf_models(&ffmpeg, cancel),
			run_ffmpeg(&ffmpeg, &["-hide_banner", "-demuxers"], cancel),
			run_ffmpeg(&ffmpeg, &["-hide_banner", "-protocols"], cancel),
		);

		let first_line = stdout_text(&version?).lines().next().unwrap_or_default().to_owned();
		let build = parse_ffmpeg_version(&first_line).ok_or_else(|| {
			ToolchainError::Unusable(ffmpeg.clone(), format!("unrecognised version line: {first_line}"))
		})?;
		let build_id = build_id(&first_line);
		let encoders = parse_codecs(&stdout_text(&encoders?));
		let decoders = parse_codecs(&stdout_text(&decoders?));
		let filters = parse_filters(&stdout_text(&filters?));
		let demuxers = parse_demuxers(&stdout_text(&demuxers?));
		let protocols = parse_protocols(&stdout_text(&protocols?));

		let soxr = soxr?;
		let opus_surround = opus_surround? && encoders.contains("libopus");
		let (svt_av1, svt_av1_release) = match svt_av1? {
			Some(found) if encoders.contains("libsvtav1") => (Some(found.version), found.release),
			_ => (None, None),
		};
		let VideoProbes {
			svt_settings,
			colour_tags,
			vp9_alpha,
			libvpx_release,
		} = video?;
		let vp9_alpha = vp9_alpha && ALPHA_FILTERS.iter().all(|name| filters.contains(*name));
		let vmaf = vmaf? && filters.contains(vmaf::FILTER);

		Ok(Self {
			ffmpeg,
			ffprobe,
			build,
			build_id,
			encoders,
			decoders,
			filters,
			svt_av1,
			svt_av1_release,
			libvpx_release,
			svt_settings,
			colour_tags,
			vp9_alpha,
			soxr,
			opus_surround,
			vmaf,
			demuxers,
			protocols,
		})
	}

	/// The encoder part of a learned-quality key, which changes whenever the encoder may: SVT-AV1 by its full version,
	/// libvpx by the version it reports, and every other library, whose version ffmpeg doesn't report, by the ffmpeg
	/// build it came with.
	pub fn encoder_id(&self, library: &str) -> String {
		let own = match library {
			SVT_AV1 => self.svt_av1_release.as_deref(),
			LIBVPX_VP9 => self.libvpx_release.as_deref(),
			_ => None,
		};

		match own {
			Some(release) => format!("{library}-{release}"),
			None => format!("{library}-ffmpeg-{}", self.build_id),
		}
	}

	pub fn problems(&self, kind: MediaKind) -> Vec<String> {
		self.missing(|line| line.needed_for.contains(&kind))
	}

	pub fn universal_problems(&self) -> Vec<String> {
		self.missing(|line| EVERY_KIND.iter().all(|kind| line.needed_for.contains(kind)))
	}

	pub fn has_av1_decoder(&self) -> bool {
		AV1_DECODERS.iter().any(|name| self.decoders.contains(*name))
	}

	pub fn has_libaom(&self) -> bool {
		self.encoders.contains(LIBAOM)
	}

	/// What this ffmpeg lacks for an encoder chosen with `--video-encoder`, known before any file is read.
	pub fn video_encoder_problem(&self, choice: Option<VideoEncoderChoice>) -> Option<&'static str> {
		(choice == Some(VideoEncoderChoice::Aom) && !self.has_libaom())
			.then_some("--video-encoder aom needs ffmpeg with libaom-av1")
	}

	/// The files that fail with this ffmpeg although nothing every run needs is missing.
	pub fn optional_failures(&self) -> Option<String> {
		let mut failures = Vec::new();
		if !self.has_libaom() {
			failures.push(format!("{LIBAOM}: {LIBAOM_FILES}"));
		}
		if !self.has_bluray() {
			failures.push(format!("{BLURAY_PROTOCOL}: {BLURAY_FILES}"));
		}
		(!failures.is_empty()).then(|| failures.join("; "))
	}

	pub fn has_libjxl(&self) -> bool {
		self.encoders.contains(LIBJXL)
	}

	pub fn has_vmaf(&self) -> bool {
		self.vmaf
	}

	pub fn has_bluray(&self) -> bool {
		self.protocols.contains(BLURAY_PROTOCOL)
	}

	fn missing(&self, selected: impl Fn(&ReportLine) -> bool) -> Vec<String> {
		self.report()
			.into_iter()
			.filter(|line| line.status == Status::Missing && selected(line))
			.map(|line| format!("{}: {}; {FULL_BUILD_HINT}", line.label, line.detail))
			.collect()
	}

	#[allow(clippy::too_many_lines)]
	pub fn report(&self) -> Vec<ReportLine> {
		let check = |label: &str, present: bool, missing: Status, needed_for, purpose: &str| ReportLine {
			status: if present { Status::Ok } else { missing },
			label: label.to_owned(),
			detail: if present {
				purpose.to_owned()
			} else {
				format!("{purpose} · not available")
			},
			needed_for,
		};
		let missing_filters: Vec<&str> = VIDEO_FILTERS
			.iter()
			.copied()
			.filter(|name| !self.filters.contains(*name))
			.collect();
		let missing_image_tools: Vec<&str> = IMAGE_FILTERS
			.iter()
			.copied()
			.filter(|name| !self.filters.contains(*name))
			.chain((!self.encoders.contains(PAM)).then_some(PAM))
			.collect();

		vec![
			self.ffmpeg_line(),
			ReportLine {
				status: Status::Ok,
				label: "ffprobe".into(),
				detail: display_path(&self.ffprobe),
				needed_for: EVERY_KIND,
			},
			check(
				"libopus",
				self.encoders.contains("libopus"),
				Status::Missing,
				AUDIO_AND_VIDEO,
				"audio encoder",
			),
			check(
				"soxr",
				self.soxr,
				Status::Missing,
				AUDIO_AND_VIDEO,
				"high-quality resampler",
			),
			check(
				"Opus 5.1",
				self.opus_surround,
				Status::Missing,
				AUDIO_AND_VIDEO,
				"surround audio",
			),
			self.svt_line(),
			check(
				"SVT-AV1 settings",
				self.svt_settings,
				Status::Missing,
				VIDEO_ONLY,
				"tune, film grain, quarter-step CRF, odd sizes",
			),
			check(
				"colour tags",
				self.colour_tags,
				Status::Missing,
				VIDEO_ONLY,
				"tags reach the container and the bitstream",
			),
			check(
				"AV1 decoder",
				self.has_av1_decoder(),
				Status::Missing,
				VIDEO_AND_IMAGES,
				"checks encoded video and images",
			),
			ReportLine {
				status: if missing_image_tools.is_empty() {
					Status::Ok
				} else {
					Status::Missing
				},
				label: "image filters".into(),
				detail: if missing_image_tools.is_empty() {
					"image decoding and checks".to_owned()
				} else {
					format!("image decoding and checks · missing {}", missing_image_tools.join(", "))
				},
				needed_for: IMAGES_ONLY,
			},
			ReportLine {
				status: if missing_filters.is_empty() {
					Status::Ok
				} else {
					Status::Missing
				},
				label: "video filters".into(),
				detail: if missing_filters.is_empty() {
					"deinterlacing and analysis".to_owned()
				} else {
					format!("deinterlacing and analysis · missing {}", missing_filters.join(", "))
				},
				needed_for: VIDEO_ONLY,
			},
			check(
				"libvpx-vp9",
				self.vp9_alpha,
				Status::Warn,
				OPTIONAL,
				"transparent video",
			),
			check(LIBAOM, self.has_libaom(), Status::Warn, OPTIONAL, LIBAOM_FILES),
			check(
				LIBJXL,
				self.has_libjxl(),
				Status::Warn,
				OPTIONAL,
				"large and high-bit-depth images",
			),
			check(
				"libvmaf",
				self.vmaf,
				Status::Warn,
				OPTIONAL,
				"video quality search and checks at MODERATE and SLOW",
			),
			check(
				"Blu-ray discs",
				self.has_bluray(),
				Status::Warn,
				OPTIONAL,
				"reading Blu-ray discs and folders",
			),
		]
	}

	fn ffmpeg_line(&self) -> ReportLine {
		let at = display_path(&self.ffmpeg);
		let (status, detail) = match &self.build {
			FfmpegBuild::Release(version) if *version >= MIN_FFMPEG => (Status::Ok, format!("{version} at {at}")),
			FfmpegBuild::Release(version) => (
				Status::Missing,
				format!("{version} at {at} · {MIN_FFMPEG} or newer required"),
			),
			FfmpegBuild::Git(tag) => (Status::Warn, format!("git build {tag} at {at} (version unknown)")),
		};
		ReportLine {
			status,
			label: "ffmpeg".into(),
			detail,
			needed_for: EVERY_KIND,
		}
	}

	fn svt_line(&self) -> ReportLine {
		let available = self.svt_av1.filter(|_| self.encoders.contains("libsvtav1"));
		let (status, detail) = match available {
			Some(version) if version >= MIN_SVT_AV1 => (Status::Ok, format!("{version} · needed for video")),
			Some(version) => (
				Status::Missing,
				format!("{version} · needed for video, {MIN_SVT_AV1} or newer required"),
			),
			None => (Status::Missing, "not available · needed for video".to_owned()),
		};
		ReportLine {
			status,
			label: "libsvtav1".into(),
			detail,
			needed_for: VIDEO_ONLY,
		}
	}
}

fn parse_ffmpeg_version(line: &str) -> Option<FfmpegBuild> {
	let token = line.strip_prefix("ffmpeg version ")?.split_whitespace().next()?;
	let numeric = token.strip_prefix('n').unwrap_or(token);
	let prefix: String = numeric
		.chars()
		.take_while(|c| c.is_ascii_digit() || *c == '.')
		.collect();

	let mut numbers = prefix.split('.');
	if let (Some(Ok(major)), Some(Ok(minor))) = (numbers.next().map(str::parse), numbers.next().map(str::parse)) {
		return Some(FfmpegBuild::Release(Version { major, minor }));
	}

	(token.starts_with('N') || token.contains("git")).then(|| FfmpegBuild::Git(token.to_owned()))
}

struct SvtVersion {
	version: Version,
	release: Option<String>,
}

fn parse_svt_version(line: &str) -> Option<SvtVersion> {
	const MARKER: &str = "Encoder Lib v";

	let rest = &line[line.find(MARKER)? + MARKER.len()..];
	let mut numbers = rest.split(|c: char| !c.is_ascii_digit());
	let version = Version {
		major: numbers.next()?.parse().ok()?,
		minor: numbers.next()?.parse().ok()?,
	};
	Some(SvtVersion {
		version,
		release: dotted_release(rest),
	})
}

/// libvpx prints its version on a line of its own when it starts encoding: `[libvpx-vp9 @ 0000…] v1.16.0`.
fn parse_libvpx_release(line: &str) -> Option<String> {
	let rest = line.strip_prefix("[libvpx-vp9 @ ")?;
	dotted_release(rest.split_once("] v")?.1)
}

fn dotted_release(text: &str) -> Option<String> {
	let release: String = text.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
	let release = release.trim_end_matches('.');
	(release.contains('.') && release.starts_with(|c: char| c.is_ascii_digit())).then(|| release.to_owned())
}

fn build_id(version_line: &str) -> String {
	version_line
		.strip_prefix("ffmpeg version ")
		.and_then(|rest| rest.split_whitespace().next())
		.unwrap_or("unknown")
		.to_owned()
}

fn parse_codecs(text: &str) -> BTreeSet<String> {
	text.lines()
		.filter_map(|line| {
			let mut parts = line.split_whitespace();
			let flags = parts.next()?;
			let name = parts.next()?;
			let is_row = flags.len() == 6 && flags.chars().all(|c| c.is_ascii_uppercase() || c == '.') && name != "=";
			is_row.then(|| name.to_owned())
		})
		.collect()
}

fn parse_filters(text: &str) -> BTreeSet<String> {
	text.lines()
		.filter_map(|line| {
			let mut parts = line.split_whitespace();
			let flags = parts.next()?;
			let name = parts.next()?;
			let io = parts.next()?;
			let is_row = (2..=3).contains(&flags.len())
				&& flags.chars().all(|c| matches!(c, 'T' | 'S' | 'C' | '.'))
				&& io.contains("->");
			is_row.then(|| name.to_owned())
		})
		.collect()
}

fn parse_demuxers(text: &str) -> BTreeSet<String> {
	text.lines()
		.filter_map(|line| {
			let (flags, name) = (line.get(..5)?, line.get(5..)?);
			let is_row = matches!(flags.as_bytes(), [b' ', b'D', b' ', b' ' | b'd', b' ']);
			is_row.then(|| name.split_whitespace().next().map(str::to_owned))?
		})
		.collect()
}

fn parse_protocols(text: &str) -> BTreeSet<String> {
	let mut listing_inputs = false;
	let mut names = BTreeSet::new();

	for line in text.lines() {
		match line.trim() {
			"Input:" => listing_inputs = true,
			"Output:" => listing_inputs = false,
			name if listing_inputs && !name.is_empty() => {
				names.insert(name.to_owned());
			}
			_ => {}
		}
	}

	names
}

fn locate(configured: Option<&Path>) -> Result<(PathBuf, PathBuf), ToolchainError> {
	let exe = |name: &str| format!("{name}{}", std::env::consts::EXE_SUFFIX);

	let ffmpeg = match configured {
		Some(path) if path.is_dir() => path.join(exe("ffmpeg")),
		Some(path) => path.to_path_buf(),
		None => find_on_path(&exe("ffmpeg")).ok_or(ToolchainError::NotFound("ffmpeg"))?,
	};
	if !ffmpeg.is_file() {
		return Err(ToolchainError::NotFound("ffmpeg"));
	}

	let sibling = ffmpeg.with_file_name(exe("ffprobe"));
	let ffprobe = if sibling.is_file() {
		sibling
	} else {
		find_on_path(&exe("ffprobe")).ok_or(ToolchainError::NotFound("ffprobe"))?
	};
	Ok((ffmpeg, ffprobe))
}

fn find_on_path(file: &str) -> Option<PathBuf> {
	std::env::split_paths(&std::env::var_os("PATH")?)
		.map(|dir| dir.join(file))
		.find(|candidate| candidate.is_file())
}

async fn run_ffmpeg(ffmpeg: &Path, args: &[&str], cancel: &CancellationToken) -> Result<ToolOutput, ToolchainError> {
	let mut cmd = ToolCommand::new(ffmpeg, PROBE_TIMEOUT);
	cmd.args(args);
	run_command(&cmd, cancel).await
}

/// A feature probe that crashes shows that the feature is missing, not that the tool is unusable.
async fn run_command(cmd: &ToolCommand, cancel: &CancellationToken) -> Result<ToolOutput, ToolchainError> {
	match process::run(cmd, StdoutMode::Capture, |_| {}, cancel).await {
		Err(RunError::Crashed {
			status, stderr_tail, ..
		}) => Ok(ToolOutput {
			status,
			stdout: Vec::new(),
			stderr_tail,
			counted_lines: 0,
			last_progress: None,
		}),
		result => result.map_err(|err| ToolchainError::Unusable(cmd.program.clone(), err.to_string())),
	}
}

async fn succeeds(ffmpeg: &Path, args: &[&str], cancel: &CancellationToken) -> Result<bool, ToolchainError> {
	let out = run_ffmpeg(ffmpeg, args, cancel).await?;
	Ok(out.status.success())
}

async fn svt_version(ffmpeg: &Path, cancel: &CancellationToken) -> Result<Option<SvtVersion>, ToolchainError> {
	let out = run_ffmpeg(ffmpeg, SVT_PROBE, cancel).await?;
	Ok(out.stderr_tail.iter().find_map(|line| parse_svt_version(line)))
}

async fn vmaf_models(ffmpeg: &Path, cancel: &CancellationToken) -> Result<bool, ToolchainError> {
	for model in vmaf::MODELS {
		let graph = format!("[0:v][1:v]{}=model=version={model}", vmaf::FILTER);
		let args = [
			"-hide_banner",
			"-nostdin",
			"-f",
			"lavfi",
			"-i",
			VMAF_PROBE_INPUT,
			"-f",
			"lavfi",
			"-i",
			VMAF_PROBE_INPUT,
			"-lavfi",
			graph.as_str(),
			"-f",
			"null",
			"-",
		];
		let out = run_ffmpeg(ffmpeg, &args, cancel).await?;
		if !out.status.success() || !out.stderr_tail.iter().any(|line| line.contains("VMAF score:")) {
			return Ok(false);
		}
	}
	Ok(true)
}

fn stdout_text(out: &ToolOutput) -> String {
	String::from_utf8_lossy(&out.stdout).into_owned()
}

struct VideoProbes {
	svt_settings: bool,
	colour_tags: bool,
	vp9_alpha: bool,
	libvpx_release: Option<String>,
}

async fn video_probes(
	ffmpeg: &Path,
	ffprobe: &Path,
	cancel: &CancellationToken,
) -> Result<VideoProbes, ToolchainError> {
	let Some(scratch) = ScratchDir::create() else {
		return Ok(VideoProbes {
			svt_settings: false,
			colour_tags: false,
			vp9_alpha: false,
			libvpx_release: None,
		});
	};

	let (svt, vp9) = tokio::join!(
		svt_settings(ffmpeg, ffprobe, &scratch.0, cancel),
		vp9_alpha(ffmpeg, &scratch.0, cancel),
	);
	let (svt_settings, colour_tags) = svt?;
	let (vp9_alpha, libvpx_release) = vp9?;
	Ok(VideoProbes {
		svt_settings,
		colour_tags,
		vp9_alpha,
		libvpx_release,
	})
}

async fn svt_settings(
	ffmpeg: &Path,
	ffprobe: &Path,
	dir: &Path,
	cancel: &CancellationToken,
) -> Result<(bool, bool), ToolchainError> {
	let clip = dir.join("svt.mkv");
	let mut encode = ToolCommand::new(ffmpeg, PROBE_TIMEOUT);
	encode.args(SVT_SETTINGS_PROBE).arg(&clip);
	let out = run_command(&encode, cancel).await?;

	let confirmed = |marker: &str| out.stderr_tail.iter().any(|line| line.contains(marker));
	// At the probe's tiny size SVT turns AQ off and reports the rate control as CQP; the value is still the CRF.
	let quarter_step = confirmed("CRF / 30.25") || confirmed("CQP / 30.25");
	if !(out.status.success() && confirmed(": 8 / VQ") && quarter_step) {
		return Ok((false, false));
	}

	let mut check = ToolCommand::new(ffprobe, PROBE_TIMEOUT);
	check
		.args([
			"-v",
			"error",
			"-threads",
			"0",
			"-print_format",
			"json",
			"-show_streams",
			"-show_frames",
			"-read_intervals",
			"%+#1",
			"-i",
		])
		.arg(&clip);
	let out = run_command(&check, cancel).await?;

	let json: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
	let tagged = |entry: &Value| PROBE_TAGS.iter().all(|(key, value)| entry[key] == *value);
	Ok((
		true,
		json["streams"][0]["width"] == 65 && tagged(&json["streams"][0]) && tagged(&json["frames"][0]),
	))
}

async fn vp9_alpha(
	ffmpeg: &Path,
	dir: &Path,
	cancel: &CancellationToken,
) -> Result<(bool, Option<String>), ToolchainError> {
	let clip = dir.join("alpha.webm");
	let mut encode = ToolCommand::new(ffmpeg, PROBE_TIMEOUT);
	encode.args(VP9_ALPHA_PROBE).arg(&clip);
	let encoded = run_command(&encode, cancel).await?;
	let release = encoded.stderr_tail.iter().find_map(|line| parse_libvpx_release(line));
	if !encoded.status.success() {
		return Ok((false, release));
	}

	let mut decode = ToolCommand::new(ffmpeg, PROBE_TIMEOUT);
	decode
		.args(["-hide_banner", "-nostdin", "-c:v", LIBVPX_VP9, "-i"])
		.arg(&clip)
		.args(["-vf", "alphaextract", "-f", "null", "-"]);
	Ok((run_command(&decode, cancel).await?.status.success(), release))
}

struct ScratchDir(PathBuf);

impl ScratchDir {
	fn create() -> Option<Self> {
		let dir = std::env::temp_dir().join(format!("cryarchiver-probe-{}", Ulid::generate()));
		fs::create_dir(&dir).ok()?;
		Some(Self(dir))
	}
}

impl Drop for ScratchDir {
	fn drop(&mut self) {
		let _ = fs::remove_dir_all(&self.0);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_release_and_git_versions() {
		let release = |major, minor| Some(FfmpegBuild::Release(Version { major, minor }));

		assert_eq!(
			parse_ffmpeg_version("ffmpeg version n9.0.1-6-g9d4ca21220-20260821 Copyright (c) 2000-2026"),
			release(9, 0)
		);
		assert_eq!(parse_ffmpeg_version("ffmpeg version 8.1.2 Copyright"), release(8, 1));
		assert_eq!(
			parse_ffmpeg_version("ffmpeg version 7.1-full_build-www.gyan.dev Copyright"),
			release(7, 1)
		);
		assert_eq!(
			parse_ffmpeg_version("ffmpeg version N-118000-gabc1234-20260801 Copyright"),
			Some(FfmpegBuild::Git("N-118000-gabc1234-20260801".into()))
		);
		assert_eq!(
			parse_ffmpeg_version("ffmpeg version 2026-08-01-git-abc1234-full_build-www.gyan.dev"),
			Some(FfmpegBuild::Git(
				"2026-08-01-git-abc1234-full_build-www.gyan.dev".into()
			))
		);
		assert_eq!(parse_ffmpeg_version("something else"), None);
	}

	#[test]
	fn parses_svt_version_line() {
		let found = parse_svt_version("Svt[info]: SVT [version]:\tSVT-AV1 Encoder Lib v4.2.0-73-gfb0ed7e59").unwrap();
		let short = parse_svt_version("Svt[info]: SVT [version]:\tSVT-AV1 Encoder Lib v4.1").unwrap();

		assert_eq!(found.version, Version { major: 4, minor: 2 });
		assert_eq!(found.release.as_deref(), Some("4.2.0"));
		assert_eq!(short.release.as_deref(), Some("4.1"));
		assert!(parse_svt_version("Svt[info]: Level of Parallelism: 6").is_none());
	}

	#[test]
	fn parses_the_libvpx_version_and_the_build_id() {
		assert_eq!(
			parse_libvpx_release("[libvpx-vp9 @ 0000021215f89280] v1.16.0").as_deref(),
			Some("1.16.0")
		);
		assert_eq!(
			parse_libvpx_release("[libvpx-vp9 @ 0000021215f89280] Neither bitrate nor constrained quality specified"),
			None
		);
		assert_eq!(
			build_id("ffmpeg version n9.0.1-6-g9d4ca21220-20260821 Copyright (c) 2000-2026"),
			"n9.0.1-6-g9d4ca21220-20260821"
		);
	}

	#[test]
	fn encoder_ids_carry_the_library_version_or_else_the_ffmpeg_build() {
		let tools = complete_toolchain();
		let unreported = Toolchain {
			svt_av1_release: None,
			libvpx_release: None,
			..complete_toolchain()
		};

		assert_eq!(tools.encoder_id(SVT_AV1), "svt-av1-4.2.0");
		assert_eq!(tools.encoder_id(LIBVPX_VP9), "libvpx-vp9-1.16.0");
		assert_eq!(
			tools.encoder_id(LIBAOM),
			"libaom-av1-ffmpeg-n9.0.1-6-g9d4ca21220-20260821"
		);
		assert_eq!(tools.encoder_id(LIBJXL), "libjxl-ffmpeg-n9.0.1-6-g9d4ca21220-20260821");
		assert_eq!(
			unreported.encoder_id(SVT_AV1),
			"svt-av1-ffmpeg-n9.0.1-6-g9d4ca21220-20260821"
		);
		assert_eq!(
			unreported.encoder_id(LIBVPX_VP9),
			"libvpx-vp9-ffmpeg-n9.0.1-6-g9d4ca21220-20260821"
		);
		assert_ne!(
			Toolchain {
				build_id: "n9.0.2".into(),
				..complete_toolchain()
			}
			.encoder_id(LIBJXL),
			tools.encoder_id(LIBJXL)
		);
	}

	#[test]
	fn parses_encoder_and_filter_lists() {
		let encoders = "Encoders:\n V..... = Video\n A..... = Audio\n ------\n V....D libsvtav1            SVT-AV1 encoder (codec av1)\n A....D libopus              libopus Opus (codec opus)\n";
		let filters = "Filters:\n  T.. = Timeline support\n TS bwdif             V->V       Deinterlace the input image.\n .. channelmap        A->A       Remap audio channels.\n .. abuffer           |->A       Buffer audio frames\n";

		assert_eq!(
			parse_codecs(encoders),
			BTreeSet::from(["libopus".to_owned(), "libsvtav1".to_owned()])
		);
		assert_eq!(
			parse_filters(filters),
			BTreeSet::from(["abuffer".to_owned(), "bwdif".to_owned(), "channelmap".to_owned()])
		);
	}

	#[test]
	fn parses_demuxer_and_protocol_lists() {
		let demuxers = "File formats:
 D. = Demuxing supported
 .E = Muxing supported
 ..d = Is a device
 ---
 D   dvdvideo        DVD-Video
 D   matroska,webm   Matroska / WebM
 D   mpeg            MPEG-PS (MPEG-2 Program Stream)
 D d lavfi           Libavfilter virtual input device
  E  mp4              MP4 (MPEG-4 Part 14)
";
		let protocols = "Supported file protocols:
Input:
  bluray
  concat
  file
  subfile
Output:
  file
  md5
";

		assert_eq!(
			parse_demuxers(demuxers),
			BTreeSet::from([
				"dvdvideo".to_owned(),
				"lavfi".to_owned(),
				"matroska,webm".to_owned(),
				"mpeg".to_owned()
			])
		);
		assert_eq!(
			parse_protocols(protocols),
			BTreeSet::from([
				"bluray".to_owned(),
				"concat".to_owned(),
				"file".to_owned(),
				"subfile".to_owned()
			])
		);
	}

	#[test]
	fn missing_bluray_only_warns() {
		let mut tools = complete_toolchain();
		assert!(tools.has_bluray());
		tools.protocols.remove("bluray");

		let line = tools
			.report()
			.into_iter()
			.find(|line| line.label == "Blu-ray discs")
			.unwrap();

		assert_eq!(line.status, Status::Warn);
		assert!(!tools.has_bluray());
		assert_eq!(
			tools.optional_failures().as_deref(),
			Some("bluray: Blu-ray discs and folders can't be converted without it")
		);
		assert!(tools.problems(MediaKind::Video).is_empty());
		assert!(tools.problems(MediaKind::Audio).is_empty());
		assert!(tools.problems(MediaKind::Image).is_empty());
	}

	#[test]
	fn an_older_cached_toolchain_has_no_bluray_until_probed() {
		let mut value = serde_json::to_value(complete_toolchain()).unwrap();
		let fields = value.as_object_mut().unwrap();
		fields.remove("demuxers");
		fields.remove("protocols");

		let cached: Toolchain = serde_json::from_value(value).unwrap();

		assert!(cached.demuxers.is_empty());
		assert!(!cached.has_bluray());
	}

	fn complete_toolchain() -> Toolchain {
		Toolchain {
			ffmpeg: PathBuf::from("ffmpeg"),
			ffprobe: PathBuf::from("ffprobe"),
			build: FfmpegBuild::Release(Version { major: 9, minor: 0 }),
			build_id: "n9.0.1-6-g9d4ca21220-20260821".into(),
			encoders: ["libopus", "libsvtav1", "libvpx-vp9", "libaom-av1", "libjxl", "pam"]
				.into_iter()
				.map(String::from)
				.collect(),
			decoders: BTreeSet::from(["libdav1d".to_owned()]),
			filters: VIDEO_FILTERS
				.iter()
				.chain(ALPHA_FILTERS)
				.chain(IMAGE_FILTERS)
				.chain(&["libvmaf"])
				.map(|name| (*name).to_owned())
				.collect(),
			svt_av1: Some(Version { major: 4, minor: 2 }),
			svt_av1_release: Some("4.2.0".into()),
			libvpx_release: Some("1.16.0".into()),
			svt_settings: true,
			colour_tags: true,
			vp9_alpha: true,
			soxr: true,
			opus_surround: true,
			vmaf: true,
			demuxers: BTreeSet::from(["dvdvideo".to_owned(), "mpeg".to_owned()]),
			protocols: ["bluray", "concat", "file", "subfile"]
				.into_iter()
				.map(String::from)
				.collect(),
		}
	}

	#[test]
	fn a_complete_toolchain_has_no_problems() {
		let tools = complete_toolchain();

		assert!(
			tools.report().iter().all(|line| line.status == Status::Ok),
			"{:?}",
			tools.report()
		);
		assert!(tools.problems(MediaKind::Audio).is_empty());
		assert!(tools.problems(MediaKind::Video).is_empty());
		assert!(tools.problems(MediaKind::Image).is_empty());
		assert!(tools.has_libaom());
		assert!(tools.has_libjxl());
	}

	#[test]
	fn problems_are_split_by_media_kind() {
		let tools = Toolchain {
			svt_av1: None,
			decoders: BTreeSet::new(),
			vp9_alpha: false,
			..complete_toolchain()
		};

		let video = tools.problems(MediaKind::Video);

		assert!(tools.problems(MediaKind::Audio).is_empty());
		assert!(tools.universal_problems().is_empty());
		assert!(
			video.iter().any(|problem| problem.starts_with("libsvtav1:")),
			"{video:?}"
		);
		assert!(
			video.iter().any(|problem| problem.starts_with("AV1 decoder:")),
			"{video:?}"
		);
		assert!(
			!video.iter().any(|problem| problem.starts_with("libvpx-vp9:")),
			"{video:?}"
		);
	}

	#[test]
	fn libaom_is_looked_up_among_the_encoders() {
		let mut tools = complete_toolchain();
		tools.encoders.remove("libaom-av1");

		assert!(!tools.has_libaom());
	}

	#[test]
	fn missing_libaom_says_which_files_fail() {
		let mut tools = complete_toolchain();
		assert_eq!(tools.optional_failures(), None);
		tools.encoders.remove("libaom-av1");

		let line = tools.report().into_iter().find(|line| line.label == LIBAOM).unwrap();

		assert_eq!(line.status, Status::Warn);
		assert!(
			line.detail
				.contains("animated images, video larger than 16384×8704 and --video-encoder aom fail"),
			"{}",
			line.detail
		);
		assert!(
			tools
				.optional_failures()
				.is_some_and(|text| text.starts_with("libaom-av1: animated images"))
		);
	}

	#[test]
	fn an_encoder_choice_this_ffmpeg_lacks_is_named() {
		let complete = complete_toolchain();
		let mut without_libaom = complete_toolchain();
		without_libaom.encoders.remove("libaom-av1");

		assert_eq!(
			without_libaom.video_encoder_problem(Some(VideoEncoderChoice::Aom)),
			Some("--video-encoder aom needs ffmpeg with libaom-av1")
		);
		assert_eq!(
			without_libaom.video_encoder_problem(Some(VideoEncoderChoice::SvtAv1)),
			None
		);
		assert_eq!(without_libaom.video_encoder_problem(None), None);
		assert_eq!(complete.video_encoder_problem(Some(VideoEncoderChoice::Aom)), None);
	}

	#[test]
	fn an_old_ffmpeg_stops_every_kind() {
		let tools = Toolchain {
			build: FfmpegBuild::Release(Version { major: 7, minor: 1 }),
			encoders: BTreeSet::new(),
			..complete_toolchain()
		};

		let universal = tools.universal_problems();

		assert!(
			universal
				.iter()
				.any(|p| p.starts_with("ffmpeg: 7.1") && p.contains("8.1 or newer required"))
		);
		assert!(!universal.iter().any(|p| p.starts_with("libopus:")));
		assert!(
			tools
				.problems(MediaKind::Audio)
				.iter()
				.any(|p| p.starts_with("libopus:"))
		);
		assert!(
			tools
				.problems(MediaKind::Image)
				.iter()
				.all(|p| !p.starts_with("libopus:"))
		);
		assert!(
			tools
				.problems(MediaKind::Video)
				.iter()
				.any(|p| p.starts_with("libsvtav1:"))
		);
	}

	#[test]
	fn images_need_the_pam_encoder_and_their_filters_but_not_opus() {
		let mut tools = complete_toolchain();
		tools.encoders.remove("pam");
		tools.encoders.remove("libopus");
		tools.filters.remove("alphamerge");

		let problems = tools.problems(MediaKind::Image);

		assert_eq!(problems.len(), 1, "{problems:?}");
		assert!(problems[0].contains("missing alphamerge, pam"), "{problems:?}");
	}

	#[test]
	fn missing_video_filters_are_named() {
		let mut tools = complete_toolchain();
		tools.filters.remove("bwdif");
		tools.filters.remove("idet");

		let line = tools
			.report()
			.into_iter()
			.find(|line| line.label == "video filters")
			.unwrap();

		assert_eq!(line.status, Status::Missing);
		assert!(line.detail.contains("bwdif, idet"), "{}", line.detail);
	}

	#[test]
	fn a_missing_vmaf_model_only_warns() {
		let tools = Toolchain {
			vmaf: false,
			..complete_toolchain()
		};

		let line = tools.report().into_iter().find(|line| line.label == "libvmaf").unwrap();

		assert_eq!(line.status, Status::Warn);
		assert!(line.detail.contains("not available"), "{}", line.detail);
		assert!(tools.problems(MediaKind::Video).is_empty());
		assert!(!tools.has_vmaf());
	}

	#[test]
	fn probes_started_at_the_same_moment_get_their_own_scratch_folders() {
		const PROBES: usize = 32;
		let start = std::sync::Barrier::new(PROBES);

		let scratches: Vec<ScratchDir> = std::thread::scope(|scope| {
			let made: Vec<_> = (0..PROBES)
				.map(|_| {
					scope.spawn(|| {
						start.wait();
						ScratchDir::create().unwrap()
					})
				})
				.collect();
			made.into_iter().map(|thread| thread.join().unwrap()).collect()
		});

		let distinct: BTreeSet<&PathBuf> = scratches.iter().map(|scratch| &scratch.0).collect();
		assert_eq!(distinct.len(), PROBES);
	}
}
