use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use super::dvd::{Dvd, DvdTitle, Kept, MAX_DVD_MBITS, Selection};
use crate::language;
use crate::outcome::{ErrorKind, IoSide, JobError};
use crate::probe::{self, MediaInfo, ProbeError};
use crate::process::{self, Progress, ToolCommand};
use crate::toolchain::Toolchain;
use crate::video::chapters::{self, Chapter};
use crate::video::run_ffmpeg;
use crate::workspace::Workspace;

const PROBE_TIMEOUT: Duration = Duration::from_secs(120);
/// How far into a title ffmpeg looks for its streams. A PS declares none: a stream is found from its first packet,
/// and a subtitle track's first packet can come late.
pub(super) const PROBE_BYTES: &str = "67108864";
pub(super) const PROBE_MICROSECONDS: &str = "60000000";
/// `concat:` splits on `|` and has no escape for it, and the whole URL must fit a command line: other inputs go
/// through a `concatf:` list.
const MAX_URL: usize = 8192;
pub(super) const OUTPUT: &str = "disc.mkv";
pub(super) const CHAPTERS: &str = "chapters.txt";
const PIECES: &str = "pieces.txt";
const REENCODE_QUALITY: &str = "2";
const KEPT_SUBTITLES: &[&str] = &["dvd_subtitle", "hdmv_pgs_subtitle"];

#[derive(Debug, Clone, PartialEq)]
pub struct Built {
	pub path: PathBuf,
	pub titles: Vec<BuiltTitle>,
	pub reencoded: Vec<u16>,
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

	if titles.iter().all(|title| title.joins(job.main)) {
		job.single(&titles, &on_fraction).await
	} else {
		job.pieces(&titles, &on_fraction).await
	}
}

/// `source` stream-copied into a Matroska file in the workspace on timestamps ffmpeg generates, for a PS or TS whose
/// own timestamps restart. DVD LPCM becomes plain PCM, and data streams and subtitles Matroska can't hold are left out.
pub async fn normalise_timestamps(
	tools: &Toolchain,
	source: &Path,
	info: &MediaInfo,
	workspace: &Workspace,
	on_fraction: impl Fn(f64),
	cancel: &CancellationToken,
) -> Result<PathBuf, JobError> {
	let bytes = fs::metadata(source)
		.map_err(|err| JobError::io(IoSide::Source(source), &err))?
		.len();
	let output = workspace.file(OUTPUT);

	let mut cmd = ToolCommand::new(&tools.ffmpeg, process::scaled_timeout(bytes));
	cmd.args(["-hide_banner", "-nostdin", "-y"])
		.args(["-probesize", PROBE_BYTES, "-analyzeduration", PROBE_MICROSECONDS])
		.args(["-fflags", "+genpts"])
		.input(None, None, source);

	let mut audio = 0;
	for stream in &info.streams {
		let copied = (stream.is_video() && !stream.is_attached_pic())
			|| stream.is_audio()
			|| (stream.is_subtitle() && KEPT_SUBTITLES.contains(&stream.codec_name.as_str()));
		if !copied {
			continue;
		}

		cmd.arg("-map").arg(format!("0:{}", stream.index));
		if stream.is_audio() {
			if stream.codec_name == "pcm_dvd" {
				cmd.arg(format!("-c:a:{audio}")).arg("pcm_s24le");
			}
			audio += 1;
		}
	}

	cmd.args(["-c", "copy", "-f", "matroska"])
		.arg(process::tool_path(&output));
	run_ffmpeg(&cmd, |progress| on_fraction(written(progress, bytes)), cancel).await?;
	Ok(output)
}

struct Title<'a> {
	dvd: &'a DvdTitle,
	false_time: bool,
	ranges: Vec<Range>,
	input: Input,
	bytes: u64,
	layout: Layout,
	length_s: f64,
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
		let ranges = joined(
			title
				.cells
				.iter()
				.flat_map(|cell| dvd.byte_ranges(title.vts, cell.first_sector, cell.last_sector)),
		);
		let bytes = ranges.iter().map(|(_, _, length)| length).sum();
		if bytes == 0 {
			return Err(ErrorKind::SourceDamaged(format!("title {} has no footage", title.number)).into());
		}

		let input = Input::of(&ranges, &workspace.file(&format!("title-{}.txt", title.number)))?;
		let counted = kept.false_time.then_some(bytes);
		let streams = probe_streams(tools, &input, counted, cancel).await?;
		let layout = Layout::of(&streams)
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
		})
	}

	/// Whether the title can be read in one run with the main title: one picture and track layout, and one palette
	/// for the subpictures.
	fn joins(&self, main: &Title<'_>) -> bool {
		self.layout.video.same(&main.layout.video)
			&& self.layout.audio.len() == main.layout.audio.len()
			&& self.layout.audio.iter().zip(&main.layout.audio).all(|(a, b)| a.same(b))
			&& self.layout.subtitles == main.layout.subtitles
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

/// How ffmpeg reads byte ranges of files as one input.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Input {
	url: String,
	whitelist: &'static str,
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
			});
		}

		let lines: String = parts.iter().map(|part| escaped(part) + "\n").collect();
		fs::write(list, lines).map_err(|err| JobError::io(IoSide::Destination(list), &err))?;
		Ok(Self {
			url: format!("concatf:{}", process::tool_path(list).display()),
			whitelist: "file,subfile,concatf",
		})
	}

	fn args(&self, cmd: &mut ToolCommand) {
		cmd.args(["-protocol_whitelist", self.whitelist])
			.args(["-probesize", PROBE_BYTES, "-analyzeduration", PROBE_MICROSECONDS])
			.args(["-fflags", "+genpts", "-f", "mpeg", "-i"])
			.arg(&self.url);
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

#[derive(Debug, Default, Deserialize)]
struct Probed {
	#[serde(default)]
	streams: Vec<PsStream>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PsStream {
	#[serde(default)]
	id: String,
	#[serde(default)]
	codec_type: String,
	#[serde(default)]
	codec_name: String,
	width: Option<u32>,
	height: Option<u32>,
	sample_aspect_ratio: Option<String>,
	r_frame_rate: Option<String>,
	avg_frame_rate: Option<String>,
	field_order: Option<String>,
	channels: Option<u32>,
	channel_layout: Option<String>,
	sample_rate: Option<String>,
	bits_per_raw_sample: Option<String>,
	bit_rate: Option<String>,
	nb_read_packets: Option<String>,
}

impl PsStream {
	fn id(&self) -> Option<u32> {
		u32::from_str_radix(self.id.strip_prefix("0x")?, 16).ok()
	}
}

/// The streams ffmpeg finds in a title's head, or with `counted` (the title's bytes) in all of it, with their packets
/// counted.
async fn probe_streams(
	tools: &Toolchain,
	input: &Input,
	counted: Option<u64>,
	cancel: &CancellationToken,
) -> Result<Vec<PsStream>, JobError> {
	let timeout = counted.map_or(PROBE_TIMEOUT, process::scaled_timeout);
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
fn footage_s(streams: &[PsStream]) -> Option<f64> {
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

/// A title's streams as the PS numbers them: video, then audio and subpictures each in stream-id order.
#[derive(Debug, Clone, PartialEq)]
struct Layout {
	video: Picture,
	audio: Vec<Track>,
	subtitles: Vec<u32>,
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
	bits: Option<u32>,
	bit_rate: Option<u64>,
}

impl Layout {
	fn of(streams: &[PsStream]) -> Option<Self> {
		let mut streams: Vec<&PsStream> = streams.iter().filter(|stream| stream.id().is_some()).collect();
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
					bits: stream.bits_per_raw_sample.as_deref().and_then(|bits| bits.parse().ok()),
					bit_rate: stream.bit_rate.as_deref().and_then(|rate| rate.parse().ok()),
				})
			})
			.collect();
		let subtitles = streams
			.iter()
			.filter(|stream| stream.codec_type == "subtitle")
			.filter_map(|stream| stream.id())
			.collect();

		Some(Self {
			video,
			audio,
			subtitles,
		})
	}
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
		match self.codec.as_str() {
			"pcm_dvd" if self.bits.unwrap_or(16) > 16 => "pcm_s24le",
			"pcm_dvd" => "pcm_s16le",
			_ => "copy",
		}
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
		let input = Input::of(&ranges, &self.workspace.file("disc.txt"))?;
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
		Ok(Built {
			path: output,
			titles: built,
			reencoded: Vec::new(),
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
		let mut start_s = 0.0;

		for title in titles {
			let converted = title.reencodes(self.main);
			if converted {
				reencoded.push(title.dvd.number);
			}

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
		Ok(Built {
			path: output,
			titles: built,
			reencoded,
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
		for id in subtitles {
			cmd.arg("-map").arg(format!("0:i:{id:#x}"));
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
		let ifo = self.main.dvd;
		if ifo.audio.len() == main.audio.len() {
			for (index, attr) in ifo.audio.iter().enumerate() {
				if let Some(code) = &attr.language {
					cmd.arg(format!("-metadata:s:a:{index}"))
						.arg(format!("language={}", language::normalized(code)));
				}
			}
		}
		if ifo.subpictures.len() == main.subtitles.len() {
			for (index, code) in ifo.subpictures.iter().enumerate() {
				if let Some(code) = code {
					cmd.arg(format!("-metadata:s:s:{index}"))
						.arg(format!("language={}", language::normalized(code)));
				}
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
		let streams = [PsStream {
			codec_type: "video".into(),
			avg_frame_rate: Some("25/1".into()),
			nb_read_packets: Some("1500".into()),
			..PsStream::default()
		}];

		assert_eq!(footage_s(&streams), Some(60.0));
	}
}
