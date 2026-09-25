use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::process::{self, StdoutMode, ToolCommand, ToolOutput};
use crate::text::display_path;

const MIN_FFMPEG: Version = Version { major: 8, minor: 1 };
const MIN_SVT_AV1: Version = Version { major: 4, minor: 0 };
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
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
	pub cache_dir: Option<PathBuf>,
	pub refresh: bool,
}

#[allow(clippy::unsafe_derive_deserialize)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Toolchain {
	pub ffmpeg: PathBuf,
	pub ffprobe: PathBuf,
	pub build: FfmpegBuild,
	pub encoders: BTreeSet<String>,
	pub filters: BTreeSet<String>,
	pub svt_av1: Option<Version>,
	pub soxr: bool,
	pub opus_surround: bool,
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
}

#[derive(Debug, thiserror::Error)]
pub enum ToolchainError {
	#[error("{0} was not found: pass --ffmpeg <path> or add it to PATH")]
	NotFound(&'static str),
	#[error("could not run {path}: {message}", path = display_path(.0), message = .1)]
	Unusable(PathBuf, String),
}

#[derive(Serialize, Deserialize)]
struct CacheEntry {
	fingerprint: String,
	toolchain: Toolchain,
}

impl Toolchain {
	pub async fn detect(config: &ToolConfig, cancel: &CancellationToken) -> Result<Self, ToolchainError> {
		let (ffmpeg, ffprobe) = locate(config.ffmpeg.as_deref())?;
		let cache = cache_file(config);
		let fingerprint = fingerprint(&ffmpeg);

		if !config.refresh
			&& let Some(cached) = read_cache(cache.as_deref(), fingerprint.as_deref())
		{
			return Ok(Self { ffprobe, ..cached });
		}

		let toolchain = Box::pin(Self::probe(ffmpeg, ffprobe, cancel)).await?;
		if let (Some(cache), Some(fingerprint)) = (cache, fingerprint) {
			write_cache(&cache, fingerprint, &toolchain);
		}
		Ok(toolchain)
	}

	async fn probe(ffmpeg: PathBuf, ffprobe: PathBuf, cancel: &CancellationToken) -> Result<Self, ToolchainError> {
		let (version, encoders, filters, soxr, opus_surround, svt_av1) = tokio::join!(
			run_ffmpeg(&ffmpeg, &["-hide_banner", "-version"], cancel),
			run_ffmpeg(&ffmpeg, &["-hide_banner", "-encoders"], cancel),
			run_ffmpeg(&ffmpeg, &["-hide_banner", "-filters"], cancel),
			succeeds(&ffmpeg, SOXR_PROBE, cancel),
			succeeds(&ffmpeg, OPUS_SURROUND_PROBE, cancel),
			svt_version(&ffmpeg, cancel),
		);

		let first_line = stdout_text(&version?).lines().next().unwrap_or_default().to_owned();
		let build = parse_ffmpeg_version(&first_line).ok_or_else(|| {
			ToolchainError::Unusable(ffmpeg.clone(), format!("unrecognised version line: {first_line}"))
		})?;
		let encoders = parse_encoders(&stdout_text(&encoders?));
		let filters = parse_filters(&stdout_text(&filters?));

		let soxr = soxr?;
		let opus_surround = opus_surround? && encoders.contains("libopus");
		let svt_av1 = svt_av1?.filter(|_| encoders.contains("libsvtav1"));
		Ok(Self {
			ffmpeg,
			ffprobe,
			build,
			encoders,
			filters,
			svt_av1,
			soxr,
			opus_surround,
		})
	}

	pub fn audio_problems(&self) -> Vec<String> {
		self.report()
			.into_iter()
			.filter(|line| line.status == Status::Missing)
			.map(|line| format!("{}: {}; {FULL_BUILD_HINT}", line.label, line.detail))
			.collect()
	}

	pub fn report(&self) -> Vec<ReportLine> {
		let check = |label: &str, present: bool, missing: Status, purpose: &str| ReportLine {
			status: if present { Status::Ok } else { missing },
			label: label.to_owned(),
			detail: if present {
				purpose.to_owned()
			} else {
				format!("{purpose} · not available")
			},
		};

		vec![
			self.ffmpeg_line(),
			ReportLine {
				status: Status::Ok,
				label: "ffprobe".into(),
				detail: display_path(&self.ffprobe),
			},
			check(
				"libopus",
				self.encoders.contains("libopus"),
				Status::Missing,
				"audio encoder",
			),
			check("soxr", self.soxr, Status::Missing, "high-quality resampler"),
			check("Opus 5.1", self.opus_surround, Status::Missing, "surround audio"),
			self.svt_line(),
			check(
				"libvpx-vp9",
				self.encoders.contains("libvpx-vp9"),
				Status::Warn,
				"transparent video",
			),
			check(
				"libaom-av1",
				self.encoders.contains("libaom-av1"),
				Status::Warn,
				"animated images with alpha",
			),
			check(
				"libjxl",
				self.encoders.contains("libjxl"),
				Status::Warn,
				"large and high-bit-depth images",
			),
			check(
				"libvmaf",
				self.filters.contains("libvmaf"),
				Status::Warn,
				"video quality checks",
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
		}
	}

	fn svt_line(&self) -> ReportLine {
		let (status, detail) = match self.svt_av1 {
			Some(version) if version >= MIN_SVT_AV1 => (Status::Ok, format!("{version} · needed for video")),
			Some(version) => (
				Status::Warn,
				format!("{version} · needed for video, {MIN_SVT_AV1} or newer required"),
			),
			None => (Status::Warn, "not available · needed for video".to_owned()),
		};
		ReportLine {
			status,
			label: "libsvtav1".into(),
			detail,
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

fn parse_svt_version(line: &str) -> Option<Version> {
	const MARKER: &str = "Encoder Lib v";

	let rest = &line[line.find(MARKER)? + MARKER.len()..];
	let mut numbers = rest.split(|c: char| !c.is_ascii_digit());
	Some(Version {
		major: numbers.next()?.parse().ok()?,
		minor: numbers.next()?.parse().ok()?,
	})
}

fn parse_encoders(text: &str) -> BTreeSet<String> {
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

fn cache_file(config: &ToolConfig) -> Option<PathBuf> {
	let dir = match &config.cache_dir {
		Some(dir) => dir.clone(),
		None => directories::ProjectDirs::from("", "", "cryarchiver")?
			.cache_dir()
			.to_path_buf(),
	};
	Some(dir.join("toolchain.json"))
}

fn fingerprint(ffmpeg: &Path) -> Option<String> {
	let meta = fs::metadata(ffmpeg).ok()?;
	let modified = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
	Some(format!(
		"{}|{}|{}|{modified}",
		crate::VERSION,
		ffmpeg.display(),
		meta.len()
	))
}

fn read_cache(cache: Option<&Path>, fingerprint: Option<&str>) -> Option<Toolchain> {
	let entry: CacheEntry = serde_json::from_slice(&fs::read(cache?).ok()?).ok()?;
	(Some(entry.fingerprint.as_str()) == fingerprint).then_some(entry.toolchain)
}

fn write_cache(cache: &Path, fingerprint: String, toolchain: &Toolchain) {
	let entry = CacheEntry {
		fingerprint,
		toolchain: toolchain.clone(),
	};
	let Ok(json) = serde_json::to_vec_pretty(&entry) else {
		return;
	};

	let temp = cache.with_extension("json.tmp");
	let written = cache.parent().is_some_and(|dir| fs::create_dir_all(dir).is_ok()) && fs::write(&temp, json).is_ok();
	if written {
		let _ = fs::rename(&temp, cache);
	}
}

async fn run_ffmpeg(ffmpeg: &Path, args: &[&str], cancel: &CancellationToken) -> Result<ToolOutput, ToolchainError> {
	let mut cmd = ToolCommand::new(ffmpeg, PROBE_TIMEOUT);
	cmd.args(args);

	process::run(&cmd, StdoutMode::Capture, |_| {}, cancel)
		.await
		.map_err(|err| ToolchainError::Unusable(ffmpeg.to_path_buf(), err.to_string()))
}

async fn succeeds(ffmpeg: &Path, args: &[&str], cancel: &CancellationToken) -> Result<bool, ToolchainError> {
	let out = run_ffmpeg(ffmpeg, args, cancel).await?;
	Ok(out.status.success())
}

async fn svt_version(ffmpeg: &Path, cancel: &CancellationToken) -> Result<Option<Version>, ToolchainError> {
	let out = run_ffmpeg(ffmpeg, SVT_PROBE, cancel).await?;
	Ok(out.stderr_tail.iter().find_map(|line| parse_svt_version(line)))
}

fn stdout_text(out: &ToolOutput) -> String {
	String::from_utf8_lossy(&out.stdout).into_owned()
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
		assert_eq!(
			parse_svt_version("Svt[info]: SVT [version]:\tSVT-AV1 Encoder Lib v4.2.0-73-gfb0ed7e59"),
			Some(Version { major: 4, minor: 2 })
		);
		assert_eq!(parse_svt_version("Svt[info]: Level of Parallelism: 6"), None);
	}

	#[test]
	fn parses_encoder_and_filter_lists() {
		let encoders = "Encoders:\n V..... = Video\n A..... = Audio\n ------\n V....D libsvtav1            SVT-AV1 encoder (codec av1)\n A....D libopus              libopus Opus (codec opus)\n";
		let filters = "Filters:\n  T.. = Timeline support\n TS bwdif             V->V       Deinterlace the input image.\n .. channelmap        A->A       Remap audio channels.\n .. abuffer           |->A       Buffer audio frames\n";

		assert_eq!(
			parse_encoders(encoders),
			BTreeSet::from(["libopus".to_owned(), "libsvtav1".to_owned()])
		);
		assert_eq!(
			parse_filters(filters),
			BTreeSet::from(["abuffer".to_owned(), "bwdif".to_owned(), "channelmap".to_owned()])
		);
	}

	#[test]
	fn missing_report_rows_are_the_audio_problems() {
		let tools = Toolchain {
			ffmpeg: PathBuf::from("ffmpeg"),
			ffprobe: PathBuf::from("ffprobe"),
			build: FfmpegBuild::Release(Version { major: 7, minor: 1 }),
			encoders: BTreeSet::new(),
			filters: BTreeSet::new(),
			svt_av1: None,
			soxr: true,
			opus_surround: false,
		};

		let problems = tools.audio_problems();

		assert!(
			problems
				.iter()
				.any(|p| p.starts_with("ffmpeg: 7.1") && p.contains("8.1 or newer required"))
		);
		assert!(problems.iter().any(|p| p.starts_with("libopus:")));
		assert!(!problems.iter().any(|p| p.starts_with("soxr:")));
		assert_eq!(
			tools
				.report()
				.iter()
				.find(|line| line.label == "libsvtav1")
				.map(|line| line.status),
			Some(Status::Warn)
		);
	}

	fn cached_fake_ffmpeg(dir: &Path) -> (ToolConfig, PathBuf) {
		let exe_suffix = std::env::consts::EXE_SUFFIX;
		let ffmpeg = dir.join(format!("ffmpeg{exe_suffix}"));
		let ffprobe = dir.join(format!("ffprobe{exe_suffix}"));
		fs::write(&ffmpeg, b"").unwrap();
		fs::write(&ffprobe, b"").unwrap();

		let stale = Toolchain {
			ffmpeg: ffmpeg.clone(),
			ffprobe: PathBuf::from("/nonexistent/ffprobe"),
			build: FfmpegBuild::Release(Version { major: 9, minor: 0 }),
			encoders: BTreeSet::new(),
			filters: BTreeSet::new(),
			svt_av1: None,
			soxr: true,
			opus_surround: true,
		};
		let entry = CacheEntry {
			fingerprint: fingerprint(&ffmpeg).unwrap(),
			toolchain: stale,
		};
		fs::write(dir.join("toolchain.json"), serde_json::to_vec(&entry).unwrap()).unwrap();

		let config = ToolConfig {
			ffmpeg: Some(ffmpeg),
			cache_dir: Some(dir.to_path_buf()),
			refresh: false,
		};
		(config, ffprobe)
	}

	#[tokio::test]
	async fn cache_hit_returns_the_freshly_located_ffprobe() {
		let dir = tempfile::tempdir().unwrap();
		let (config, ffprobe) = cached_fake_ffmpeg(dir.path());

		let tools = Toolchain::detect(&config, &CancellationToken::new()).await.unwrap();

		assert_eq!(tools.ffprobe, ffprobe);
	}

	#[tokio::test]
	async fn refresh_probes_instead_of_reading_the_cache() {
		let dir = tempfile::tempdir().unwrap();
		let (config, _) = cached_fake_ffmpeg(dir.path());
		let config = ToolConfig {
			refresh: true,
			..config
		};

		let result = Toolchain::detect(&config, &CancellationToken::new()).await;

		assert!(result.is_err(), "{result:?}");
	}
}
