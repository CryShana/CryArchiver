use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use super::dvd::{Dvd, DvdTitle, Kept, MAX_DVD_MBITS, Selection};
use super::ps::{self, Reader};
use crate::language;
use crate::outcome::{ErrorKind, IoSide, JobError};
use crate::probe::{self, MediaInfo, ProbeError};
use crate::process::{self, Progress, ToolCommand};
use crate::toolchain::Toolchain;
use crate::video::chapters::{self, Chapter};
use crate::video::run_ffmpeg;
use crate::video::streams::matroska_subtitle;
use crate::workspace::Workspace;

const PROBE_TIMEOUT: Duration = Duration::from_secs(120);
/// ffmpeg holds what it probes in memory: a stream that starts later in its title than this isn't looked for.
const MAX_PROBE_BYTES: u64 = 1 << 30;
const PROBE_MARGIN: u64 = 4 << 20;
const DEEP_PROBE_MICROSECONDS: u64 = 36_000_000_000;
/// `concat:` splits on `|` and has no escape for it, and the whole URL must fit a command line: other inputs go
/// through a `concatf:` list.
const MAX_URL: usize = 8192;
pub(super) const OUTPUT: &str = "disc.mkv";
pub(super) const CHAPTERS: &str = "chapters.txt";
const PIECES: &str = "pieces.txt";
const REENCODE_QUALITY: &str = "2";
/// How far the joined titles may run from the disc's times, per join, before the joins are suspect.
const JOIN_SLACK_S: f64 = 0.5;
pub const ANGLES_LEFT_OUT: &str = "angles beyond the first were left out";

#[derive(Debug, Clone, PartialEq)]
pub struct Built {
	pub path: PathBuf,
	pub titles: Vec<BuiltTitle>,
	pub reencoded: Vec<u16>,
	/// What reading the disc fell short of.
	pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BuiltTitle {
	pub title: u16,
	pub start_s: f64,
	pub length_s: f64,
}

/// The kept titles joined into one Matroska file in the workspace: stream-copied where every title has the main
/// (longest) title's layout, otherwise title by title with the odd ones re-encoded to it. DVD LPCM becomes plain PCM,
/// subpictures carry their IFO palette, and the chapters are a chapter per title with the disc's own inside it.
pub async fn build_dvd(
	tools: &Toolchain,
	dvd: &Dvd,
	selection: &Selection,
	name: &str,
	workspace: &Workspace,
	on_fraction: impl Fn(f64),
	cancel: &CancellationToken,
) -> Result<Built, JobError> {
	let mut titles = Vec::with_capacity(selection.kept.len());
	for kept in &selection.kept {
		titles.push(Title::read(tools, dvd, kept, workspace, cancel).await?);
	}

	let main = longest(&titles).ok_or_else(|| ErrorKind::SourceDamaged("the disc has no title to read".into()))?;
	let total: u64 = titles.iter().map(|title| title.bytes).sum();
	let job = Job {
		tools,
		workspace,
		main: &titles[main],
		name,
		timeout: process::scaled_timeout(total),
		cancel,
	};

	let mut built = if titles.iter().all(|title| title.joins(job.main)) {
		job.single(&titles, &on_fraction).await?
	} else {
		job.pieces(&titles, &on_fraction).await?
	};

	let mut warnings: Vec<String> = titles.iter().flat_map(|title| title.warnings.iter().cloned()).collect();
	if titles.iter().any(|title| title.dvd.angles) {
		warnings.push(ANGLES_LEFT_OUT.to_owned());
	}
	warnings.append(&mut built.warnings);
	built.warnings = warnings;
	Ok(built)
}

/// `source` stream-copied into a Matroska file in the workspace on timestamps ffmpeg generates, for a PS or TS whose
/// own timestamps restart. DVD LPCM becomes plain PCM, text subtitles become SRT, and data streams are left out;
/// the warnings name the subtitle tracks Matroska can't hold, which are left out too.
pub async fn normalise_timestamps(
	tools: &Toolchain,
	source: &Path,
	info: &MediaInfo,
	workspace: &Workspace,
	on_fraction: impl Fn(f64),
	cancel: &CancellationToken,
) -> Result<(PathBuf, Vec<String>), JobError> {
	let bytes = fs::metadata(source)
		.map_err(|err| JobError::io(IoSide::Source(source), &err))?
		.len();
	let output = workspace.file(OUTPUT);

	let mut cmd = ToolCommand::new(&tools.ffmpeg, process::scaled_timeout(bytes));
	cmd.args(["-hide_banner", "-nostdin", "-y"])
		.args(["-fflags", "+genpts"]);
	Window::HEAD.apply(&mut cmd);
	cmd.input(None, None, source);

	let mut overrides = Vec::new();
	let mut warnings = Vec::new();
	let (mut audio, mut subtitles, mut seen) = (0, 0, 0);
	for stream in &info.streams {
		if stream.is_video() && !stream.is_attached_pic() {
			cmd.arg("-map").arg(format!("0:{}", stream.index));
		} else if stream.is_audio() {
			cmd.arg("-map").arg(format!("0:{}", stream.index));
			let pcm = pcm_target(
				&stream.codec_name,
				stream.bits_per_raw_sample,
				stream.sample_fmt.as_deref(),
			);
			overrides.extend(pcm.map(|codec| (format!("a:{audio}"), codec)));
			audio += 1;
		} else if stream.is_subtitle() {
			seen += 1;
			let Some(written) = matroska_subtitle(&stream.codec_name) else {
				warnings.push(format!(
					"subtitle track {seen} ({}) was left out: MKV can't hold it",
					stream.codec_name
				));
				continue;
			};

			cmd.arg("-map").arg(format!("0:{}", stream.index));
			if written != "copy" {
				overrides.push((format!("s:{subtitles}"), written));
			}
			subtitles += 1;
		}
	}

	copy_with(&mut cmd, overrides);
	cmd.args(["-f", "matroska"]).arg(process::tool_path(&output));
	run_ffmpeg(&cmd, |progress| on_fraction(written(progress, bytes)), cancel).await?;
	Ok((output, warnings))
}

/// The PCM Matroska holds for DVD or Blu-ray LPCM, of its depth; `None` for any other codec.
pub(super) fn pcm_target(codec: &str, bits: Option<u32>, sample_fmt: Option<&str>) -> Option<&'static str> {
	if !matches!(codec, "pcm_dvd" | "pcm_bluray") {
		return None;
	}

	let deep = sample_fmt == Some("s32") || bits.is_some_and(|bits| bits > 16);
	Some(if deep { "pcm_s24le" } else { "pcm_s16le" })
}

/// `-c copy`, then each `(stream specifier, codec)` of `overrides`: ffmpeg takes the last option that matches a
/// stream, so the general copy must come first.
pub(super) fn copy_with(cmd: &mut ToolCommand, overrides: impl IntoIterator<Item = (String, &'static str)>) {
	cmd.args(["-c", "copy"]);
	for (specifier, codec) in overrides {
		cmd.arg(format!("-c:{specifier}")).arg(codec);
	}
}

struct Title<'a> {
	dvd: &'a DvdTitle,
	false_time: bool,
	ranges: Vec<Range>,
	input: Input,
	bytes: u64,
	layout: Layout,
	length_s: f64,
	warnings: Vec<String>,
}

type Range = (PathBuf, u64, u64);

impl<'a> Title<'a> {
	async fn read(
		tools: &Toolchain,
		dvd: &'a Dvd,
		kept: &Kept,
		workspace: &Workspace,
		cancel: &CancellationToken,
	) -> Result<Self, JobError> {
		let title = dvd
			.titles
			.iter()
			.find(|title| title.number == kept.title)
			.ok_or_else(|| ErrorKind::SourceDamaged(format!("the disc has no title {}", kept.title)))?;
		let ranges = joined(title.cells.iter().flat_map(|cell| dvd.cell_ranges(title.vts, cell)));
		let bytes = ranges.iter().map(|(_, _, length)| length).sum();
		if bytes == 0 {
			return Err(ErrorKind::SourceDamaged(format!("title {} has no footage", title.number)).into());
		}

		let mut input = Input::of(&ranges, &workspace.file(&format!("title-{}.txt", title.number)))?;
		let counted = kept.false_time.then_some(bytes);
		let mut streams = probe_streams(tools, &input, counted, cancel).await?;
		let mut warnings = Vec::new();
		let lacking = missing(&streams, title);
		if !lacking.is_empty() {
			let ids: Vec<u32> = lacking.iter().map(|(id, _)| *id).collect();
			let firsts = first_packets(&ranges, ids, cancel).await?;
			let reach = firsts
				.iter()
				.map(|(_, at)| *at)
				.filter(|at| *at < MAX_PROBE_BYTES)
				.max();
			if let Some(reach) = reach {
				input.window = Window {
					bytes: (reach + PROBE_MARGIN).max(Window::HEAD.bytes),
					microseconds: DEEP_PROBE_MICROSECONDS,
				};
				streams = probe_streams(tools, &input, counted, cancel).await?;
			}

			for (id, name) in missing(&streams, title) {
				let why = match firsts.iter().find(|(first, _)| *first == id) {
					None => "has no packets",
					Some((_, at)) if *at >= MAX_PROBE_BYTES => "starts too late in the title to be read",
					Some(_) => "can't be read",
				};
				warnings.push(format!("Title {}: {name} {why}", title.number));
			}
		}

		let layout = Layout::of(&streams, title)
			.ok_or_else(|| ErrorKind::SourceDamaged(format!("title {} has no video", title.number)))?;

		#[allow(clippy::cast_precision_loss)]
		let length_s = if kept.false_time {
			footage_s(&streams).unwrap_or(bytes as f64 * 8.0 / (MAX_DVD_MBITS * 1e6))
		} else {
			title.cells.iter().map(|cell| cell.time_s).sum()
		};

		Ok(Self {
			dvd: title,
			false_time: kept.false_time,
			ranges,
			input,
			bytes,
			layout,
			length_s,
			warnings,
		})
	}

	/// Whether the title can be read in one run with the main title: one picture and track layout, and one palette
	/// for the subpictures.
	fn joins(&self, main: &Title<'_>) -> bool {
		self.layout.video.same(&main.layout.video)
			&& self.layout.audio.len() == main.layout.audio.len()
			&& self.layout.audio.iter().zip(&main.layout.audio).all(|(a, b)| a.same(b))
			&& self.layout.subtitle_ids() == main.layout.subtitle_ids()
			&& (main.layout.subtitles.is_empty() || self.dvd.palette == main.dvd.palette)
	}

	/// Whether the title's picture or audio must be re-encoded to join the main title.
	fn reencodes(&self, main: &Title<'_>) -> bool {
		!self.layout.video.same(&main.layout.video)
			|| main
				.layout
				.audio
				.iter()
				.enumerate()
				.any(|(index, track)| self.layout.audio.get(index).is_none_or(|own| !own.same(track)))
	}

	/// The title's tracks beyond the main title's, which its piece leaves out.
	fn left_out(&self, main: &Title<'_>) -> Vec<String> {
		let audio =
			(main.layout.audio.len()..self.layout.audio.len()).map(|index| format!("audio track {}", index + 1));
		let subtitles = (main.layout.subtitles.len()..self.layout.subtitles.len())
			.map(|index| format!("subtitle track {}", index + 1));
		audio
			.chain(subtitles)
			.map(|track| {
				format!(
					"Title {}: {track} was left out to match the main title",
					self.dvd.number
				)
			})
			.collect()
	}
}

/// The streams the IFO says `title` plays that `streams` lack, as `(PS id, what the track is called)`.
fn missing(streams: &[ProbedStream], title: &DvdTitle) -> Vec<(u32, String)> {
	let found: Vec<u32> = streams.iter().filter_map(ProbedStream::id).collect();
	let audio = title
		.audio
		.iter()
		.enumerate()
		.filter_map(|(index, attr)| Some((attr.id?, format!("audio track {}", index + 1))));
	let subtitles = title
		.subpictures
		.iter()
		.enumerate()
		.filter_map(|(index, subpicture)| Some((subpicture.id?, format!("subtitle track {}", index + 1))));
	audio.chain(subtitles).filter(|(id, _)| !found.contains(id)).collect()
}

/// Where in `ranges` each of `ids` has its first packet, for those that have one.
async fn first_packets(
	ranges: &[Range],
	ids: Vec<u32>,
	cancel: &CancellationToken,
) -> Result<Vec<(u32, u64)>, JobError> {
	let ranges = ranges.to_vec();
	let stop = cancel.clone();
	let scan = tokio::task::spawn_blocking(move || {
		let mut left = ids;
		let mut found = Vec::new();
		let read = Reader::default().each_sector(&ranges, usize::MAX, |position, sector| {
			for packet in ps::packets(sector) {
				if let Some(index) = left.iter().position(|id| *id == packet.id) {
					found.push((left.swap_remove(index), position));
				}
			}
			!left.is_empty() && !stop.is_cancelled()
		});
		read.map(|()| found)
			.map_err(|err| (ranges.first().map(|(path, _, _)| path.clone()), err))
	});

	let found = scan
		.await
		.map_err(|err| ErrorKind::Toolchain(format!("the disc scan stopped: {err}")))?
		.map_err(|(path, err)| JobError::io(IoSide::Source(path.as_deref().unwrap_or(Path::new(""))), &err))?;
	if cancel.is_cancelled() {
		return Err(ErrorKind::Cancelled.into());
	}
	Ok(found)
}

/// A warning when the joined titles' real length is off the disc's times by more than `JOIN_SLACK_S` per join: the
/// timestamp correction at a join may have gone wrong.
fn join_gap(actual_s: f64, expected_s: f64, joins: usize) -> Option<String> {
	#[allow(clippy::cast_precision_loss)]
	let slack = JOIN_SLACK_S * joins as f64;
	(joins > 0 && (actual_s - expected_s).abs() > slack).then(|| {
		format!(
			"the joined titles last {actual_s:.1} s, not the {expected_s:.1} s the disc gives them: chapters and sync \
			 after a title join may be off"
		)
	})
}

/// The first of the longest titles.
fn longest(titles: &[Title<'_>]) -> Option<usize> {
	let mut best: Option<usize> = None;
	for (index, title) in titles.iter().enumerate() {
		if best.is_none_or(|best| title.length_s > titles[best].length_s) {
			best = Some(index);
		}
	}
	best
}

/// Ranges with each one that continues the one before it in the same file merged into it.
fn joined(ranges: impl IntoIterator<Item = Range>) -> Vec<Range> {
	let mut joined: Vec<Range> = Vec::new();
	for (path, offset, length) in ranges {
		match joined.last_mut() {
			Some((last, at, len)) if *last == path && *at + *len == offset => *len += length,
			_ => joined.push((path, offset, length)),
		}
	}
	joined
}

/// How much of an input ffmpeg reads to find its streams. A PS declares none: a stream is found from its first
/// packet, and a subtitle track's first packet can come late.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Window {
	bytes: u64,
	microseconds: u64,
}

impl Window {
	pub(super) const HEAD: Self = Self {
		bytes: 64 << 20,
		microseconds: 60_000_000,
	};

	pub(super) fn apply(self, cmd: &mut ToolCommand) {
		cmd.arg("-probesize")
			.arg(self.bytes.to_string())
			.arg("-analyzeduration")
			.arg(self.microseconds.to_string());
	}
}

/// How ffmpeg reads byte ranges of files as one input.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Input {
	url: String,
	whitelist: &'static str,
	window: Window,
}

impl Input {
	/// A `concat:` URL of `subfile:` ranges, or a `concatf:` URL of `list`, written here, when `concat:` can't hold
	/// them.
	fn of(ranges: &[Range], list: &Path) -> Result<Self, JobError> {
		let parts = ranges
			.iter()
			.map(|(path, offset, length)| {
				let path = process::tool_path(path);
				let path = path
					.to_str()
					.ok_or_else(|| ErrorKind::SourceUnreadable(format!("{} is not a Unicode path", path.display())))?;
				Ok(format!("subfile,,start,{offset},end,{},,:{path}", offset + length))
			})
			.collect::<Result<Vec<_>, JobError>>()?;

		let url = format!("concat:{}", parts.join("|"));
		if url.len() <= MAX_URL && !parts.iter().any(|part| part.contains('|')) {
			return Ok(Self {
				url,
				whitelist: "file,subfile,concat",
				window: Window::HEAD,
			});
		}

		let lines: String = parts.iter().map(|part| escaped(part) + "\n").collect();
		fs::write(list, lines).map_err(|err| JobError::io(IoSide::Destination(list), &err))?;
		Ok(Self {
			url: format!("concatf:{}", process::tool_path(list).display()),
			whitelist: "file,subfile,concatf",
			window: Window::HEAD,
		})
	}

	fn args(&self, cmd: &mut ToolCommand) {
		cmd.args(["-protocol_whitelist", self.whitelist]);
		self.window.apply(cmd);
		cmd.args(["-fflags", "+genpts", "-f", "mpeg", "-i"]).arg(&self.url);
	}
}

/// A `concatf:` line: ffmpeg reads each as a token, where a backslash escapes the next character.
fn escaped(line: &str) -> String {
	let mut escaped = String::with_capacity(line.len());
	for c in line.chars() {
		if matches!(c, '\\' | '\'' | '"' | ' ' | '\t') {
			escaped.push('\\');
		}
		escaped.push(c);
	}
	escaped
}

/// What ffprobe reports of a disc's footage, a DVD title's PS or a Blu-ray playlist's TS.
#[derive(Debug, Default, Deserialize)]
pub(super) struct Probed {
	#[serde(default)]
	pub(super) streams: Vec<ProbedStream>,
	#[serde(default)]
	pub(super) format: ProbedFormat,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct ProbedFormat {
	pub(super) size: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub(super) struct ProbedStream {
	#[serde(default)]
	id: String,
	#[serde(default)]
	pub(super) codec_type: String,
	#[serde(default)]
	pub(super) codec_name: String,
	width: Option<u32>,
	height: Option<u32>,
	sample_aspect_ratio: Option<String>,
	r_frame_rate: Option<String>,
	avg_frame_rate: Option<String>,
	field_order: Option<String>,
	channels: Option<u32>,
	channel_layout: Option<String>,
	sample_rate: Option<String>,
	pub(super) sample_fmt: Option<String>,
	bits_per_raw_sample: Option<String>,
	bit_rate: Option<String>,
	nb_read_packets: Option<String>,
}

impl ProbedStream {
	/// The PS stream id, or the TS PID.
	pub(super) fn id(&self) -> Option<u32> {
		u32::from_str_radix(self.id.strip_prefix("0x")?, 16).ok()
	}

	pub(super) fn bits(&self) -> Option<u32> {
		self.bits_per_raw_sample.as_deref()?.parse().ok()
	}

	/// The PCM Matroska holds for the track, when it is DVD or Blu-ray LPCM.
	pub(super) fn pcm(&self) -> Option<&'static str> {
		pcm_target(&self.codec_name, self.bits(), self.sample_fmt.as_deref())
	}
}

/// The streams ffmpeg finds in a title's probe window, or with `counted` (the title's bytes) in all of it, with their
/// packets counted.
async fn probe_streams(
	tools: &Toolchain,
	input: &Input,
	counted: Option<u64>,
	cancel: &CancellationToken,
) -> Result<Vec<ProbedStream>, JobError> {
	let timeout = process::scaled_timeout(counted.unwrap_or(input.window.bytes)).max(PROBE_TIMEOUT);
	let mut cmd = ToolCommand::new(&tools.ffprobe, timeout);
	cmd.args(["-v", "error", "-hide_banner", "-print_format", "json", "-show_streams"]);
	if counted.is_some() {
		cmd.arg("-count_packets");
	}
	input.args(&mut cmd);

	let out = probe::run_ffprobe(&cmd, cancel).await.map_err(unreadable)?;
	let probed: Probed = serde_json::from_slice(&out.stdout)
		.map_err(|err| JobError::from(ErrorKind::Toolchain(format!("ffprobe returned unreadable JSON: {err}"))))?;
	Ok(probed.streams)
}

pub(super) fn unreadable(err: ProbeError) -> JobError {
	match err {
		ProbeError::Run(err) => err.into(),
		ProbeError::Failed(log) => JobError::with_log(
			ErrorKind::SourceDamaged("ffprobe could not read the disc's footage".into()),
			log,
		),
		ProbeError::Json(err) => ErrorKind::Toolchain(err.to_string()).into(),
	}
}

/// How long the video's counted packets last at its frame rate.
#[allow(clippy::cast_precision_loss)]
fn footage_s(streams: &[ProbedStream]) -> Option<f64> {
	let video = streams.iter().find(|stream| stream.codec_type == "video")?;
	let packets: u64 = video.nb_read_packets.as_deref()?.parse().ok()?;
	let fps = video
		.avg_frame_rate
		.as_deref()
		.and_then(ratio)
		.or_else(|| video.r_frame_rate.as_deref().and_then(ratio))?;
	(packets > 0).then(|| packets as f64 / fps)
}

fn ratio(text: &str) -> Option<f64> {
	let (numerator, denominator) = text.split_once(['/', ':'])?;
	let (numerator, denominator): (f64, f64) = (numerator.parse().ok()?, denominator.parse().ok()?);
	(numerator > 0.0 && denominator > 0.0).then(|| numerator / denominator)
}

/// A title's streams: its video, and its audio and subpictures each in the disc's logical order with their IFO
/// languages, then any the IFO doesn't name in stream-id order.
#[derive(Debug, Clone, PartialEq)]
struct Layout {
	video: Picture,
	audio: Vec<Track>,
	subtitles: Vec<Subtitle>,
}

#[derive(Debug, Clone, PartialEq)]
struct Picture {
	id: u32,
	codec: String,
	width: u32,
	height: u32,
	sar: Option<String>,
	rate: Option<String>,
	fields: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq)]
struct Track {
	id: u32,
	codec: String,
	channels: Option<u32>,
	channel_layout: Option<String>,
	sample_rate: Option<u32>,
	sample_fmt: Option<String>,
	bits: Option<u32>,
	bit_rate: Option<u64>,
	language: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct Subtitle {
	id: u32,
	language: Option<String>,
}

impl Layout {
	fn of(streams: &[ProbedStream], title: &DvdTitle) -> Option<Self> {
		let mut streams: Vec<&ProbedStream> = streams.iter().filter(|stream| stream.id().is_some()).collect();
		streams.sort_by_key(|stream| stream.id());

		let video = streams.iter().find(|stream| stream.codec_type == "video")?;
		let video = Picture {
			id: video.id()?,
			codec: video.codec_name.clone(),
			width: video.width?,
			height: video.height?,
			sar: video.sample_aspect_ratio.clone().filter(|sar| ratio(sar).is_some()),
			rate: video.r_frame_rate.clone().filter(|rate| ratio(rate).is_some()),
			fields: video.field_order.as_deref().and_then(fields),
		};
		let audio = streams
			.iter()
			.filter(|stream| stream.codec_type == "audio")
			.filter_map(|stream| {
				Some(Track {
					id: stream.id()?,
					codec: stream.codec_name.clone(),
					channels: stream.channels,
					channel_layout: stream.channel_layout.clone(),
					sample_rate: stream.sample_rate.as_deref().and_then(|rate| rate.parse().ok()),
					sample_fmt: stream.sample_fmt.clone(),
					bits: stream.bits(),
					bit_rate: stream.bit_rate.as_deref().and_then(|rate| rate.parse().ok()),
					language: None,
				})
			})
			.collect();
		let subtitles = streams
			.iter()
			.filter(|stream| stream.codec_type == "subtitle")
			.filter_map(|stream| stream.id())
			.map(|id| Subtitle { id, language: None })
			.collect();

		let audio_listed = title.audio.iter().map(|attr| (attr.id, &attr.language));
		let subtitles_listed = title
			.subpictures
			.iter()
			.map(|subpicture| (subpicture.id, &subpicture.language));
		Some(Self {
			video,
			audio: in_disc_order(
				audio,
				audio_listed,
				|track| track.id,
				|track, language| track.language = language,
			),
			subtitles: in_disc_order(
				subtitles,
				subtitles_listed,
				|subtitle| subtitle.id,
				|subtitle, language| {
					subtitle.language = language;
				},
			),
		})
	}

	fn subtitle_ids(&self) -> Vec<u32> {
		self.subtitles.iter().map(|subtitle| subtitle.id).collect()
	}
}

/// `found` in the order of `listed`, `(PS id, language)` pairs, each given its language; those `listed` doesn't name
/// follow in their own order.
fn in_disc_order<'a, T>(
	found: Vec<T>,
	listed: impl IntoIterator<Item = (Option<u32>, &'a Option<String>)>,
	id: impl Fn(&T) -> u32,
	name: impl Fn(&mut T, Option<String>),
) -> Vec<T> {
	let mut left: Vec<Option<T>> = found.into_iter().map(Some).collect();
	let mut ordered = Vec::with_capacity(left.len());
	for (listed_id, language) in listed {
		let Some(listed_id) = listed_id else {
			continue;
		};
		let slot = left
			.iter_mut()
			.find(|slot| slot.as_ref().is_some_and(|found| id(found) == listed_id));
		if let Some(mut found) = slot.and_then(Option::take) {
			name(&mut found, language.clone());
			ordered.push(found);
		}
	}

	ordered.extend(left.into_iter().flatten());
	ordered
}

fn fields(order: &str) -> Option<&'static str> {
	match order {
		"tt" | "tb" => Some("tff"),
		"bb" | "bt" => Some("bff"),
		_ => None,
	}
}

impl Picture {
	fn same(&self, other: &Self) -> bool {
		self.id == other.id
			&& self.codec == other.codec
			&& (self.width, self.height) == (other.width, other.height)
			&& self.sar == other.sar
			&& self.rate == other.rate
			&& self.fields == other.fields
	}

	/// The filters that make another title's picture this one: its field order, size (by field where both are
	/// interlaced), frame rate and pixel shape.
	fn filters_from(&self, other: &Self) -> String {
		let mut filters = Vec::new();
		if let (Some(from), Some(to)) = (other.fields, self.fields)
			&& from != to
		{
			filters.push(format!("fieldorder={to}"));
		}

		let by_field = if other.fields.is_some() && self.fields.is_some() {
			":interl=1"
		} else {
			""
		};
		filters.push(format!("scale={}:{}{by_field}", self.width, self.height));

		if let Some(rate) = &self.rate
			&& other.rate.as_ref() != Some(rate)
		{
			filters.push(format!("fps={rate}"));
		}
		if let Some(sar) = &self.sar {
			filters.push(format!("setsar={}", sar.replace(':', "/")));
		}
		filters.push(format!("setfield={}", self.fields.unwrap_or("prog")));
		filters.join(",")
	}
}

impl Track {
	fn same(&self, other: &Self) -> bool {
		self.id == other.id
			&& self.codec == other.codec
			&& self.channels == other.channels
			&& self.sample_rate == other.sample_rate
			&& self.bits == other.bits
	}

	/// How the track is written: Matroska can't hold DVD LPCM, which becomes plain PCM of its depth.
	fn copied_as(&self) -> &'static str {
		pcm_target(&self.codec, self.bits, self.sample_fmt.as_deref()).unwrap_or("copy")
	}

	fn encoder(&self) -> &str {
		match self.codec.as_str() {
			"pcm_dvd" => self.copied_as(),
			"mp1" | "mp2" => "mp2",
			"mp3" => "libmp3lame",
			"dts" => "dca",
			codec => codec,
		}
	}

	fn layout(&self) -> String {
		self.channel_layout
			.clone()
			.unwrap_or_else(|| match self.channels.unwrap_or(2) {
				1 => "mono".to_owned(),
				6 => "5.1".to_owned(),
				channels => format!("{channels}c"),
			})
	}
}

/// The IFO's `0x00YYCrCb` palette as the `RRGGBB` list ffmpeg's DVD subtitle codecs take, by BT.601.
fn rgb_palette(palette: &[u32; 16]) -> String {
	palette
		.iter()
		.map(|&entry| {
			let [_, y, cr, cb] = entry.to_be_bytes();
			let [r, g, b] = rgb(y, cr, cb);
			format!("{r:02x}{g:02x}{b:02x}")
		})
		.collect::<Vec<_>>()
		.join(",")
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn rgb(y: u8, cr: u8, cb: u8) -> [u8; 3] {
	let y = 1.164 * (f64::from(y) - 16.0);
	let (cr, cb) = (f64::from(cr) - 128.0, f64::from(cb) - 128.0);
	let channel = |value: f64| value.round().clamp(0.0, 255.0) as u8;
	[
		channel(y + 1.596 * cr),
		channel(y - 0.813 * cr - 0.391 * cb),
		channel(y + 2.018 * cb),
	]
}

/// A chapter per title, named after its disc number, with the disc's own chapters after its first inside it. A title
/// whose IFO time is false gets only its title chapter.
fn chapter_list(titles: &[(&DvdTitle, bool, BuiltTitle)]) -> Vec<Chapter> {
	const EDGE_S: f64 = 0.05;

	let mut chapters = Vec::new();
	for (dvd, false_time, built) in titles {
		let end_s = built.start_s + built.length_s;
		let mut starts = vec![(built.start_s, format!("Title {}", dvd.number))];
		if !false_time {
			starts.extend(
				dvd.chapter_starts_s
					.iter()
					.enumerate()
					.filter(|(_, start)| **start > EDGE_S && **start < built.length_s - EDGE_S)
					.map(|(index, start)| (built.start_s + start, format!("Chapter {}", index + 1))),
			);
		}

		for (index, (start_s, title)) in starts.iter().enumerate() {
			chapters.push(Chapter {
				start_s: *start_s,
				end_s: starts.get(index + 1).map_or(end_s, |(next, _)| *next),
				title: Some(title.clone()),
			});
		}
	}
	chapters
}

/// How each of the main layout's audio tracks is fed from a title.
enum Feed<'a> {
	Copy(&'a Track),
	Convert(&'a Track),
	Silence,
}

struct Job<'a> {
	tools: &'a Toolchain,
	workspace: &'a Workspace,
	main: &'a Title<'a>,
	name: &'a str,
	timeout: Duration,
	cancel: &'a CancellationToken,
}

impl Job<'_> {
	async fn single(&self, titles: &[Title<'_>], on_fraction: &impl Fn(f64)) -> Result<Built, JobError> {
		let mut built = Vec::with_capacity(titles.len());
		let mut start_s = 0.0;
		for title in titles {
			built.push(BuiltTitle {
				title: title.dvd.number,
				start_s,
				length_s: title.length_s,
			});
			start_s += title.length_s;
		}

		let ranges = joined(titles.iter().flat_map(|title| title.ranges.iter().cloned()));
		let total: u64 = titles.iter().map(|title| title.bytes).sum();
		let mut input = Input::of(&ranges, &self.workspace.file("disc.txt"))?;
		input.window = titles.first().map_or(Window::HEAD, |first| first.input.window);
		let chapters = self.chapters(titles, &built)?;
		let output = self.workspace.file(OUTPUT);

		let mut cmd = ToolCommand::new(&self.tools.ffmpeg, self.timeout);
		cmd.args(["-hide_banner", "-nostdin", "-y"]);
		if !self.main.layout.subtitles.is_empty() {
			cmd.arg("-palette").arg(rgb_palette(&self.main.dvd.palette));
		}
		input.args(&mut cmd);
		cmd.arg("-i").arg(process::tool_path(&chapters));
		self.map(&mut cmd, self.main, &[]);
		self.finish(&mut cmd, &output);

		run_ffmpeg(&cmd, |progress| on_fraction(written(progress, total)), self.cancel).await?;
		let read_s = probe::probe(self.tools, &output, self.cancel)
			.await
			.map_err(unreadable)?
			.format
			.duration;
		let warnings = read_s
			.and_then(|read_s| join_gap(read_s, start_s, titles.len().saturating_sub(1)))
			.into_iter()
			.collect();
		Ok(Built {
			path: output,
			titles: built,
			reencoded: Vec::new(),
			warnings,
		})
	}

	async fn pieces(&self, titles: &[Title<'_>], on_fraction: &impl Fn(f64)) -> Result<Built, JobError> {
		let total: u64 = titles.iter().map(|title| title.bytes).sum();
		#[allow(clippy::cast_precision_loss)]
		let work = 2.0 * total as f64;
		let mut done = 0;
		let mut pieces = Vec::with_capacity(titles.len());
		let mut built = Vec::with_capacity(titles.len());
		let mut reencoded = Vec::new();
		let mut warnings = Vec::new();
		let mut start_s = 0.0;

		for title in titles {
			let converted = title.reencodes(self.main);
			if converted {
				reencoded.push(title.dvd.number);
			}
			warnings.extend(title.left_out(self.main));

			let piece = self.workspace.file(&format!("title-{}.mkv", title.dvd.number));
			let cmd = self.piece(title, &piece);
			#[allow(clippy::cast_precision_loss)]
			let report = |progress: &Progress| {
				let fraction = if converted {
					progress
						.out_time
						.map_or(0.0, |time| (time.as_secs_f64() / title.length_s.max(1.0)).min(1.0))
				} else {
					written(progress, title.bytes)
				};
				on_fraction((done as f64 + fraction * title.bytes as f64) / work);
			};
			run_ffmpeg(&cmd, report, self.cancel).await?;
			done += title.bytes;

			let length_s = probe::probe(self.tools, &piece, self.cancel)
				.await
				.map_err(unreadable)?
				.format
				.duration
				.unwrap_or(title.length_s);
			built.push(BuiltTitle {
				title: title.dvd.number,
				start_s,
				length_s,
			});
			start_s += length_s;
			pieces.push(piece);
		}

		let list = self.workspace.file(PIECES);
		let mut lines = String::new();
		for piece in &pieces {
			let path = process::tool_path(piece).display().to_string();
			let _ = writeln!(lines, "file '{}'", path.replace('\'', r"'\''"));
		}
		fs::write(&list, lines).map_err(|err| JobError::io(IoSide::Destination(&list), &err))?;
		let chapters = self.chapters(titles, &built)?;
		let output = self.workspace.file(OUTPUT);
		let piece_bytes: u64 = pieces
			.iter()
			.map(|piece| fs::metadata(piece).map_or(0, |meta| meta.len()))
			.sum();

		let mut cmd = ToolCommand::new(&self.tools.ffmpeg, self.timeout);
		cmd.args(["-hide_banner", "-nostdin", "-y", "-f", "concat", "-safe", "0", "-i"])
			.arg(process::tool_path(&list))
			.arg("-i")
			.arg(process::tool_path(&chapters))
			.args(["-map", "0", "-c", "copy"]);
		self.finish(&mut cmd, &output);

		#[allow(clippy::cast_precision_loss)]
		let report = |progress: &Progress| {
			on_fraction((total as f64 + written(progress, piece_bytes) * total as f64) / work);
		};
		run_ffmpeg(&cmd, report, self.cancel).await?;
		for piece in &pieces {
			let _ = fs::remove_file(piece);
		}

		Ok(Built {
			path: output,
			titles: built,
			reencoded,
			warnings,
		})
	}

	/// The command that writes `title` alone in the main layout to `piece`.
	fn piece(&self, title: &Title<'_>, piece: &Path) -> ToolCommand {
		let mut cmd = ToolCommand::new(&self.tools.ffmpeg, self.timeout);
		cmd.args(["-hide_banner", "-nostdin", "-y"]);
		if !title.layout.subtitles.is_empty() && !self.main.layout.subtitles.is_empty() {
			cmd.arg("-palette").arg(rgb_palette(&title.dvd.palette));
		}
		title.input.args(&mut cmd);

		let feeds = self.feeds(title);
		for (index, feed) in feeds.iter().enumerate() {
			if matches!(feed, Feed::Silence) {
				let track = &self.main.layout.audio[index];
				let source = format!(
					"anullsrc=r={}:cl={}",
					track.sample_rate.unwrap_or(48_000),
					track.layout()
				);
				cmd.arg("-t")
					.arg(format!("{:.3}", title.length_s))
					.args(["-f", "lavfi", "-i"])
					.arg(source);
			}
		}

		self.map(&mut cmd, title, &feeds);
		cmd.args(["-f", "matroska"]).arg(process::tool_path(piece));
		cmd
	}

	fn feeds<'t>(&self, title: &'t Title<'_>) -> Vec<Feed<'t>> {
		self.main
			.layout
			.audio
			.iter()
			.enumerate()
			.map(|(index, main)| match title.layout.audio.get(index) {
				Some(own) if own.same(main) => Feed::Copy(own),
				Some(own) => Feed::Convert(own),
				None => Feed::Silence,
			})
			.collect()
	}

	/// The streams of input 0, which holds `title`, mapped to the main layout. `feeds` are from `feeds`, or empty
	/// for a title of the main layout.
	fn map(&self, cmd: &mut ToolCommand, title: &Title<'_>, feeds: &[Feed<'_>]) {
		let main = &self.main.layout;
		cmd.arg("-map").arg(format!("0:i:{:#x}", title.layout.video.id));
		if main.video.same(&title.layout.video) {
			cmd.args(["-c:v", "copy"]);
		} else {
			cmd.args([
				"-c:v",
				"mpeg2video",
				"-q:v",
				REENCODE_QUALITY,
				"-pix_fmt",
				"yuv420p",
				"-filter:v",
			])
			.arg(main.video.filters_from(&title.layout.video));
			if main.video.fields.is_some() {
				cmd.args(["-flags:v", "+ilme+ildct"]);
			}
		}

		let mut silence = 0;
		for (index, track) in main.audio.iter().enumerate() {
			let codec = format!("-c:a:{index}");
			match feeds.get(index).unwrap_or(&Feed::Copy(track)) {
				Feed::Copy(own) => {
					cmd.arg("-map").arg(format!("0:i:{:#x}", own.id));
					cmd.arg(codec).arg(own.copied_as());
					continue;
				}
				Feed::Convert(own) => {
					cmd.arg("-map").arg(format!("0:i:{:#x}", own.id));
				}
				Feed::Silence => {
					silence += 1;
					cmd.arg("-map").arg(format!("{silence}:a:0"));
				}
			}

			cmd.arg(codec).arg(track.encoder());
			if track.encoder() == "dca" {
				cmd.args(["-strict", "-2"]);
			}
			if let Some(rate) = track.sample_rate {
				cmd.arg(format!("-ar:a:{index}")).arg(rate.to_string());
			}
			if let Some(channels) = track.channels {
				cmd.arg(format!("-ac:a:{index}")).arg(channels.to_string());
			}
			if let Some(bit_rate) = track.bit_rate.filter(|_| track.copied_as() == "copy") {
				cmd.arg(format!("-b:a:{index}")).arg(bit_rate.to_string());
			}
		}

		let subtitles = &title.layout.subtitles[..title.layout.subtitles.len().min(main.subtitles.len())];
		for subtitle in subtitles {
			cmd.arg("-map").arg(format!("0:i:{:#x}", subtitle.id));
		}
		if !subtitles.is_empty() {
			cmd.args(["-c:s", "dvdsub", "-palette"])
				.arg(rgb_palette(&self.main.dvd.palette));
		}
	}

	/// The chapters, title, languages and output of the command that writes the intermediate.
	fn finish(&self, cmd: &mut ToolCommand, output: &Path) {
		cmd.args(["-map_chapters", "1", "-metadata"])
			.arg(format!("title={}", self.name));

		let main = &self.main.layout;
		for (index, track) in main.audio.iter().enumerate() {
			if let Some(code) = &track.language {
				cmd.arg(format!("-metadata:s:a:{index}"))
					.arg(format!("language={}", language::normalized(code)));
			}
		}
		for (index, subtitle) in main.subtitles.iter().enumerate() {
			if let Some(code) = &subtitle.language {
				cmd.arg(format!("-metadata:s:s:{index}"))
					.arg(format!("language={}", language::normalized(code)));
			}
		}

		cmd.args(["-f", "matroska"]).arg(process::tool_path(output));
	}

	fn chapters(&self, titles: &[Title<'_>], built: &[BuiltTitle]) -> Result<PathBuf, JobError> {
		let listed: Vec<(&DvdTitle, bool, BuiltTitle)> = titles
			.iter()
			.zip(built)
			.map(|(title, built)| (title.dvd, title.false_time, *built))
			.collect();
		let path = self.workspace.file(CHAPTERS);
		fs::write(&path, chapters::ffmetadata(&chapter_list(&listed)))
			.map_err(|err| JobError::io(IoSide::Destination(&path), &err))?;
		Ok(path)
	}
}

/// The share of `bytes` ffmpeg has written: a stream copy writes about what it reads.
#[allow(clippy::cast_precision_loss)]
pub(super) fn written(progress: &Progress, bytes: u64) -> f64 {
	progress
		.total_size
		.map_or(0.0, |size| (size as f64 / bytes.max(1) as f64).min(1.0))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::disc::dvd::{AudioAttr, AudioCoding, Subpicture};

	#[test]
	fn the_ifo_palette_becomes_rgb() {
		let mut palette = [0x0010_8080; 16];
		palette[1] = 0x00EB_8080;
		palette[2] = 0x0051_5AF0;

		let rgb = rgb_palette(&palette);

		assert!(rgb.starts_with("000000,ffffff,0f3fff,000000,"), "{rgb}");
		assert_eq!(rgb.split(',').count(), 16);
	}

	#[test]
	fn ranges_that_continue_each_other_are_merged() {
		let a = PathBuf::from("a.vob");
		let b = PathBuf::from("b.vob");

		let merged = joined([
			(a.clone(), 0, 2048),
			(a.clone(), 2048, 4096),
			(a.clone(), 10_240, 2048),
			(b.clone(), 12_288, 2048),
		]);

		assert_eq!(merged, [(a.clone(), 0, 6144), (a, 10_240, 2048), (b, 12_288, 2048)]);
	}

	#[test]
	fn byte_ranges_become_a_concat_url_with_exclusive_ends() {
		let dir = tempfile::tempdir().unwrap();
		let vob = dir.path().join("VTS_01_1.VOB");

		let input = Input::of(
			&[(vob.clone(), 4096, 2048), (vob.clone(), 0, 1024)],
			&dir.path().join("list.txt"),
		)
		.unwrap();

		let path = vob.display();
		assert_eq!(
			input.url,
			format!("concat:subfile,,start,4096,end,6144,,:{path}|subfile,,start,0,end,1024,,:{path}")
		);
		assert_eq!(input.whitelist, "file,subfile,concat");
	}

	#[test]
	fn ranges_that_concat_cannot_hold_go_into_a_list() {
		let dir = tempfile::tempdir().unwrap();
		let odd = dir.path().join("a b'c|d.vob");
		let list = dir.path().join("list.txt");

		let input = Input::of(&[(odd.clone(), 0, 2048)], &list).unwrap();

		assert_eq!(input.url, format!("concatf:{}", list.display()));
		assert_eq!(input.whitelist, "file,subfile,concatf");
		let line = fs::read_to_string(&list).unwrap();
		assert!(line.ends_with("a\\ b\\'c|d.vob\n"), "{line}");

		let long: Vec<Range> = (0..400)
			.map(|index| (dir.path().join("VTS_01_1.VOB"), index * 4096, 2048))
			.collect();
		assert!(Input::of(&long, &list).unwrap().url.starts_with("concatf:"));
	}

	#[test]
	fn titles_get_a_chapter_each_with_the_disc_chapters_inside() {
		let title = |number: u16, chapter_starts_s: Vec<f64>| DvdTitle {
			number,
			chapter_starts_s,
			..DvdTitle::default()
		};
		let first = title(1, vec![0.0, 20.0, 50.0]);
		let second = title(4, vec![0.0, 3.0]);
		let third = title(5, vec![0.0, 99.0]);
		let built = |start_s, length_s| BuiltTitle {
			title: 0,
			start_s,
			length_s,
		};

		let chapters = chapter_list(&[
			(&first, false, built(0.0, 60.0)),
			(&second, true, built(60.0, 10.0)),
			(&third, false, built(70.0, 30.0)),
		]);

		let listed: Vec<(f64, f64, &str)> = chapters
			.iter()
			.map(|chapter| (chapter.start_s, chapter.end_s, chapter.title.as_deref().unwrap()))
			.collect();
		assert_eq!(
			listed,
			[
				(0.0, 20.0, "Title 1"),
				(20.0, 50.0, "Chapter 2"),
				(50.0, 60.0, "Chapter 3"),
				(60.0, 70.0, "Title 4"),
				(70.0, 100.0, "Title 5"),
			]
		);
	}

	#[test]
	fn another_shape_is_scaled_by_field_and_marked_like_the_main_title() {
		let main = Picture {
			id: 0x1e0,
			codec: "mpeg2video".into(),
			width: 720,
			height: 576,
			sar: Some("64:45".into()),
			rate: Some("25/1".into()),
			fields: Some("tff"),
		};
		let small = Picture {
			width: 352,
			height: 288,
			sar: Some("16:11".into()),
			fields: Some("bff"),
			..main.clone()
		};

		assert_eq!(
			main.filters_from(&small),
			"fieldorder=tff,scale=720:576:interl=1,setsar=64/45,setfield=tff"
		);
		assert!(!main.same(&small));
		assert!(main.same(&main.clone()));
	}

	#[test]
	fn footage_time_comes_from_the_counted_video_packets() {
		let streams = [ProbedStream {
			codec_type: "video".into(),
			avg_frame_rate: Some("25/1".into()),
			nb_read_packets: Some("1500".into()),
			..ProbedStream::default()
		}];

		assert_eq!(footage_s(&streams), Some(60.0));
	}

	fn stream(id: &str, codec_type: &str, codec_name: &str) -> ProbedStream {
		ProbedStream {
			id: id.into(),
			codec_type: codec_type.into(),
			codec_name: codec_name.into(),
			width: Some(720),
			height: Some(576),
			..ProbedStream::default()
		}
	}

	fn audio(coding: AudioCoding, language: &str, id: u32) -> AudioAttr {
		AudioAttr {
			coding,
			channels: 2,
			language: Some(language.into()),
			id: Some(id),
		}
	}

	#[test]
	fn mixed_codings_keep_the_disc_order_and_languages() {
		let title = DvdTitle {
			audio: vec![
				audio(AudioCoding::Ac3, "en", 0x80),
				audio(AudioCoding::Mpeg, "de", 0x1C0),
				audio(AudioCoding::Ac3, "fr", 0x81),
			],
			subpictures: vec![
				Subpicture {
					language: Some("sl".into()),
					id: Some(0x21),
				},
				Subpicture {
					language: Some("hr".into()),
					id: Some(0x20),
				},
			],
			..DvdTitle::default()
		};
		let streams = [
			stream("0x1e0", "video", "mpeg2video"),
			stream("0x80", "audio", "ac3"),
			stream("0x81", "audio", "ac3"),
			stream("0x1c0", "audio", "mp2"),
			stream("0x89", "audio", "dts"),
			stream("0x20", "subtitle", "dvd_subtitle"),
			stream("0x21", "subtitle", "dvd_subtitle"),
		];

		let layout = Layout::of(&streams, &title).unwrap();

		let audio: Vec<(u32, Option<&str>)> = layout
			.audio
			.iter()
			.map(|track| (track.id, track.language.as_deref()))
			.collect();
		assert_eq!(
			audio,
			[
				(0x80, Some("en")),
				(0x1C0, Some("de")),
				(0x81, Some("fr")),
				(0x89, None)
			]
		);
		let subtitles: Vec<(u32, Option<&str>)> = layout
			.subtitles
			.iter()
			.map(|subtitle| (subtitle.id, subtitle.language.as_deref()))
			.collect();
		assert_eq!(subtitles, [(0x21, Some("sl")), (0x20, Some("hr"))]);
	}

	#[test]
	fn the_streams_a_title_should_play_but_lacks_are_named() {
		let title = DvdTitle {
			audio: vec![audio(AudioCoding::Ac3, "en", 0x80), audio(AudioCoding::Ac3, "de", 0x81)],
			subpictures: vec![
				Subpicture {
					language: None,
					id: Some(0x20),
				},
				Subpicture::default(),
			],
			..DvdTitle::default()
		};
		let streams = [stream("0x1e0", "video", "mpeg2video"), stream("0x80", "audio", "ac3")];

		assert_eq!(
			missing(&streams, &title),
			[
				(0x81, "audio track 2".to_owned()),
				(0x20, "subtitle track 1".to_owned())
			]
		);
	}

	#[test]
	fn lpcm_becomes_pcm_of_its_depth_whichever_disc_it_is_from() {
		assert_eq!(pcm_target("pcm_dvd", Some(16), Some("s16")), Some("pcm_s16le"));
		assert_eq!(pcm_target("pcm_dvd", Some(20), Some("s32")), Some("pcm_s24le"));
		assert_eq!(pcm_target("pcm_dvd", None, None), Some("pcm_s16le"));
		assert_eq!(pcm_target("pcm_bluray", None, Some("s32")), Some("pcm_s24le"));
		assert_eq!(pcm_target("ac3", Some(24), None), None);
	}

	#[test]
	fn the_general_copy_comes_before_each_streams_own_codec() {
		let mut cmd = ToolCommand::new(Path::new("ffmpeg"), Duration::from_secs(1));

		copy_with(&mut cmd, [("a:1".to_owned(), "pcm_s24le"), ("s:0".to_owned(), "srt")]);

		assert_eq!(cmd.args, ["-c", "copy", "-c:a:1", "pcm_s24le", "-c:s:0", "srt"]);
	}

	#[test]
	fn joins_that_drift_from_the_disc_times_are_warned_about() {
		assert_eq!(join_gap(23.3, 23.0, 1), None);
		assert_eq!(join_gap(30.0, 23.0, 0), None);
		let warning = join_gap(24.2, 23.0, 2).unwrap();
		assert!(
			warning.starts_with("the joined titles last 24.2 s, not the 23.0 s"),
			"{warning}"
		);
	}
}
