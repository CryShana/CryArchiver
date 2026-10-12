use std::path::Path;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::read::{self, Built, BuiltTitle, Window};
use super::udf::DiscFiles;
use super::{Reader, be16, be32, byte, get};
use crate::language;
use crate::outcome::{ErrorKind, JobError};
use crate::probe::{self, MediaInfo, ProbeError, Stream};
use crate::process::{self, ToolCommand};
use crate::toolchain::Toolchain;
use crate::video::chapters::{self, Chapter};
use crate::video::run_ffmpeg;
use crate::workspace::Workspace;

const TICKS_PER_SECOND: f64 = 45_000.0;
const ENTRY_MARK: u8 = 1;
const MAIN_PATH: u8 = 1;
const PLAY_ITEM_BYTES: usize = 32;
const MARK_BYTES: usize = 14;
const MULTI_ANGLE: u8 = 0x10;
const MAX_MPLS_BYTES: u64 = 16 << 20;
/// libbluray reads every playlist and clip of the disc before it picks one.
const PROBE_TIMEOUT: Duration = Duration::from_secs(300);
const SELECTED: &str = "selected ";

/// The playlist libbluray picked, from ffmpeg's `selected 00800.mpls` log line.
fn selected_playlist(log: &[String]) -> Option<u32> {
	log.iter().rev().find_map(|line| {
		let (_, picked) = line.split_once(SELECTED)?;
		let number = picked.trim().strip_suffix(".mpls")?;
		if number.len() == 5 && number.bytes().all(|digit| digit.is_ascii_digit()) {
			number.parse().ok()
		} else {
			None
		}
	})
}

/// A playlist's play items, entry marks and the languages of its first play item's streams.
#[derive(Debug, Clone, Default, PartialEq)]
struct Mpls {
	items: Vec<PlayItem>,
	/// `(play item, time)`, the time in 45 kHz ticks of that item's clip.
	marks: Vec<(usize, u32)>,
	/// Languages of the streams in the main path, by PID.
	languages: Vec<(u16, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct PlayItem {
	in_time: u32,
	out_time: u32,
}

impl PlayItem {
	fn ticks(self) -> u64 {
		u64::from(self.out_time.saturating_sub(self.in_time))
	}
}

impl Mpls {
	fn parse(mpls: &[u8]) -> Result<Self, String> {
		if mpls.get(..4) != Some(b"MPLS") {
			return Err("it is not a playlist".to_owned());
		}

		let list = position(be32(mpls, 8)?);
		let marks = position(be32(mpls, 12)?);
		let count = be16(mpls, list + 6)?;
		let mut items = Vec::with_capacity(usize::from(count));
		let mut languages = Vec::new();
		let mut at = list + 10;
		for index in 0..count {
			let length = usize::from(be16(mpls, at)?);
			let item = at + 2;
			if length < PLAY_ITEM_BYTES || item + length > mpls.len() {
				return Err(format!("play item {index} is cut short"));
			}

			items.push(PlayItem {
				in_time: be32(mpls, item + 12)?,
				out_time: be32(mpls, item + 16)?,
			});
			if index == 0 {
				languages = stream_languages(mpls, item).unwrap_or_default();
			}
			at = item + length;
		}

		let count = usize::from(be16(mpls, marks + 4)?);
		let mut entry_marks = Vec::new();
		for index in 0..count {
			let mark = marks + 6 + MARK_BYTES * index;
			get::<MARK_BYTES>(mpls, mark)?;
			let item = usize::from(be16(mpls, mark + 2)?);
			if byte(mpls, mark + 1)? == ENTRY_MARK && item < items.len() {
				entry_marks.push((item, be32(mpls, mark + 4)?));
			}
		}

		Ok(Self {
			items,
			marks: entry_marks,
			languages,
		})
	}

	fn marks_s(&self) -> Vec<f64> {
		self.marks
			.iter()
			.map(|&(item, time)| {
				let before: u64 = self.items[..item].iter().map(|item| item.ticks()).sum();
				seconds(before + u64::from(time.saturating_sub(self.items[item].in_time)))
			})
			.collect()
	}

	fn length_s(&self) -> f64 {
		seconds(self.items.iter().map(|item| item.ticks()).sum())
	}
}

/// The languages of a play item's main-path video, audio and subtitle streams, from its STN table.
fn stream_languages(mpls: &[u8], item: usize) -> Result<Vec<(u16, String)>, String> {
	let mut stn = item + PLAY_ITEM_BYTES;
	if byte(mpls, item + 10)? & MULTI_ANGLE != 0 {
		let angles = usize::from(byte(mpls, stn)?);
		stn += 2 + 10 * angles.saturating_sub(1);
	}

	let [video, audio, subtitles, _, _, _, picture_subtitles, _] = get::<8>(mpls, stn + 4)?;
	let listed = [video, audio, subtitles, picture_subtitles]
		.into_iter()
		.map(usize::from)
		.sum();
	let mut languages = Vec::new();
	let mut at = stn + 16;
	for _ in 0..listed {
		let entry = usize::from(byte(mpls, at)?);
		let path = byte(mpls, at + 1)?;
		let pid = be16(mpls, at + 2)?;
		let attributes = at + 1 + entry;
		let language = match byte(mpls, attributes + 1)? {
			0x03 | 0x04 | 0x80..=0x86 | 0xA1 | 0xA2 | 0x92 => Some(attributes + 3),
			0x90 | 0x91 => Some(attributes + 2),
			_ => None,
		};

		if path == MAIN_PATH
			&& let Some(code) = language
		{
			let code = get::<3>(mpls, code)?;
			if code.iter().all(u8::is_ascii_alphabetic) {
				languages.push((pid, language::normalized(&String::from_utf8_lossy(&code))));
			}
		}
		at = attributes + 1 + usize::from(byte(mpls, attributes)?);
	}
	Ok(languages)
}

fn position(offset: u32) -> usize {
	usize::try_from(offset).unwrap_or(usize::MAX)
}

#[allow(clippy::cast_precision_loss)]
fn seconds(ticks: u64) -> f64 {
	ticks as f64 / TICKS_PER_SECOND
}

/// A chapter from each entry mark to the next, the last to the end of the playlist.
fn chapter_list(marks_s: &[f64], length_s: f64) -> Vec<Chapter> {
	let mut starts: Vec<f64> = marks_s
		.iter()
		.copied()
		.filter(|start| *start < length_s - chapters::EDGE_S)
		.collect();
	starts.sort_by(f64::total_cmp);
	starts.dedup_by(|later, earlier| *later - *earlier < chapters::EDGE_S);

	let named = starts
		.into_iter()
		.enumerate()
		.map(|(index, start_s)| (start_s, format!("Chapter {}", index + 1)))
		.collect();
	chapters::from_starts(named, length_s)
}

/// The main feature libbluray picks on a disc: the longest of its relevant playlists.
#[derive(Debug, Clone, Default)]
pub struct MainFeature {
	pub playlist: Option<u32>,
	/// The bytes ffmpeg reads for it.
	pub bytes: u64,
	/// Why it has no chapters, when it hasn't.
	pub warnings: Vec<String>,
	url: String,
	streams: Vec<Stream>,
	chapters: Vec<Chapter>,
	languages: Vec<(u16, String)>,
}

impl MainFeature {
	fn streams_of<'a>(&'a self, codec_type: &'a str) -> impl Iterator<Item = &'a Stream> {
		self.streams
			.iter()
			.filter(move |stream| stream.codec_type == codec_type)
	}

	fn language(&self, stream: &Stream) -> Option<&str> {
		let pid = u16::try_from(stream.stream_id()?).ok()?;
		self.languages
			.iter()
			.find(|(own, _)| *own == pid)
			.map(|(_, code)| code.as_str())
	}
}

/// Whether ffmpeg's log says libbluray couldn't open a disc for its AACS or BD+ encryption. ffmpeg echoes the disc's
/// path in its errors, so each of `paths` is cut out of a line before it is matched.
fn encrypted(log: &[String], paths: &[&str]) -> bool {
	let mut paths: Vec<String> = paths
		.iter()
		.filter(|path| !path.is_empty())
		.map(|path| slashed(path))
		.collect();
	paths.sort_by_key(|path| std::cmp::Reverse(path.len()));

	log.iter().any(|line| {
		let mut line = slashed(line);
		for path in &paths {
			line = line.replace(path.as_str(), "");
		}
		line.contains("aacs") || line.contains("bd+") || line.contains("encrypted")
	})
}

fn slashed(text: &str) -> String {
	text.replace('\\', "/").to_ascii_lowercase()
}

/// The main feature of the Blu-ray image or folder `source`, whose files are `files`: ffmpeg opens it through
/// libbluray with no playlist, and the one libbluray picks is read from its log. That playlist's entry marks become
/// the chapters. `None` when the disc is encrypted.
pub async fn main_feature(
	tools: &Toolchain,
	source: &Path,
	files: &DiscFiles,
	cancel: &CancellationToken,
) -> Result<Option<MainFeature>, JobError> {
	let url = disc_url(source, files)?;
	let mut cmd = ToolCommand::new(&tools.ffprobe, PROBE_TIMEOUT);
	cmd.args([
		"-v",
		"info",
		"-hide_banner",
		"-print_format",
		"json",
		"-show_streams",
		"-show_format",
	]);
	Window::HEAD.apply(&mut cmd);
	cmd.arg("-i").arg(&url);

	let out = match probe::run_ffprobe(&cmd, cancel).await {
		Ok(out) => out,
		Err(ProbeError::Failed(log)) if encrypted(&log, &echoed_paths(source, &url)) => return Ok(None),
		Err(err) => return Err(read::unreadable(err)),
	};
	let probed = MediaInfo::parse(&out.stdout).map_err(|err| read::unreadable(err.into()))?;

	let playlist = selected_playlist(&out.stderr_tail);
	let mpls = playlist
		.ok_or_else(|| {
			"ffmpeg didn't name the Blu-ray playlist it read, so the main feature has no chapters".to_owned()
		})
		.and_then(|number| {
			read_mpls(files, number).map_err(|reason| {
				format!("playlist {number:05}.mpls can't be read ({reason}), so the main feature has no chapters")
			})
		});
	let warnings = mpls.as_ref().err().cloned().into_iter().collect();

	let bytes = probed.format.size.unwrap_or_else(|| stream_bytes(files));
	let (chapters, languages) = mpls.map_or_else(
		|_| Default::default(),
		|mpls| (chapter_list(&mpls.marks_s(), mpls.length_s()), mpls.languages),
	);

	Ok(Some(MainFeature {
		playlist,
		bytes,
		warnings,
		url,
		streams: probed.streams,
		chapters,
		languages,
	}))
}

/// The spellings of the disc's path ffmpeg may echo in a log line.
fn echoed_paths<'a>(source: &'a Path, url: &'a str) -> Vec<&'a str> {
	[
		url.strip_prefix("bluray:"),
		source.to_str(),
		source.file_name().and_then(|name| name.to_str()),
	]
	.into_iter()
	.flatten()
	.collect()
}

/// The `bluray:` URL of the image, or of the root of the folder that holds `BDMV`.
fn disc_url(source: &Path, files: &DiscFiles) -> Result<String, JobError> {
	let root = if source.is_dir() {
		files
			.find("BDMV/index.bdmv")
			.and_then(|index| index.source.parent()?.parent())
			.ok_or_else(|| ErrorKind::SourceDamaged("the Blu-ray folder has no BDMV/index.bdmv".into()))?
	} else {
		source
	};

	Ok(format!("bluray:{}", process::unicode_tool_path(root)?))
}

fn read_mpls(files: &DiscFiles, number: u32) -> Result<Mpls, String> {
	let name = format!("{number:05}.mpls");
	let mut reasons = Vec::new();
	for folder in ["BDMV/PLAYLIST", "BDMV/BACKUP/PLAYLIST"] {
		let Some(file) = files.find(&format!("{folder}/{name}")) else {
			continue;
		};
		if file.size > MAX_MPLS_BYTES {
			reasons.push("it is too large for a playlist".to_owned());
			continue;
		}

		match file
			.read(&mut Reader::default())
			.map_err(|err| err.to_string())
			.and_then(|bytes| Mpls::parse(&bytes))
		{
			Ok(mpls) => return Ok(mpls),
			Err(reason) => reasons.push(reason),
		}
	}

	if reasons.is_empty() {
		reasons.push("it is missing".to_owned());
	}
	Err(reasons.join("; "))
}

fn stream_bytes(files: &DiscFiles) -> u64 {
	files
		.files
		.iter()
		.filter(|file| file.path.to_ascii_uppercase().starts_with("BDMV/STREAM/"))
		.map(|file| file.size)
		.sum()
}

/// The main feature stream-copied into one Matroska file in the workspace, with its chapters, its stream languages
/// from the playlist and `name` as its title. Blu-ray LPCM becomes plain PCM.
pub async fn build_bluray(
	tools: &Toolchain,
	main: &MainFeature,
	name: &str,
	workspace: &Workspace,
	on_fraction: impl Fn(f64),
	cancel: &CancellationToken,
) -> Result<Built, JobError> {
	let output = workspace.file(read::OUTPUT);
	let chapters = if main.chapters.is_empty() {
		None
	} else {
		let path = workspace.file(read::CHAPTERS);
		chapters::write(&path, &main.chapters)?;
		Some(path)
	};

	let mut cmd = ToolCommand::new(&tools.ffmpeg, process::scaled_timeout(main.bytes));
	cmd.args(["-hide_banner", "-nostdin", "-y"]);
	if let Some(playlist) = main.playlist {
		cmd.arg("-playlist").arg(playlist.to_string());
	}
	Window::HEAD.apply(&mut cmd);
	cmd.arg("-i").arg(&main.url);
	if let Some(chapters) = &chapters {
		cmd.arg("-i").arg(process::tool_path(chapters));
	}

	cmd.args(["-map", "0:v?", "-map", "0:a?", "-map", "0:s?"]);
	read::copy_with(
		&mut cmd,
		main.streams_of("audio")
			.enumerate()
			.filter_map(|(index, stream)| Some((format!("a:{index}"), stream.pcm()?))),
	);
	read::language_tags(
		&mut cmd,
		'a',
		main.streams_of("audio").map(|stream| main.language(stream)),
	);
	read::language_tags(
		&mut cmd,
		's',
		main.streams_of("subtitle").map(|stream| main.language(stream)),
	);

	if chapters.is_some() {
		cmd.args(["-map_chapters", "1"]);
	}
	cmd.arg("-metadata")
		.arg(format!("title={name}"))
		.args(["-f", "matroska"])
		.arg(process::tool_path(&output));

	run_ffmpeg(
		&cmd,
		|progress| on_fraction(read::written(progress, main.bytes)),
		cancel,
	)
	.await?;

	let length_s = probe::probe(tools, &output, cancel)
		.await
		.map_err(read::unreadable)?
		.format
		.duration
		.or_else(|| main.chapters.last().map(|chapter| chapter.end_s))
		.unwrap_or_default();
	Ok(Built {
		path: output,
		titles: vec![BuiltTitle {
			title: 0,
			start_s: 0.0,
			length_s,
		}],
		reencoded: Vec::new(),
		warnings: Vec::new(),
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	const SECOND: u32 = 45_000;

	/// An `.mpls` whose play items are `(in, out)` and whose marks are `(kind, item, time)`. Its first play item's
	/// STN lists a video stream, a Slovenian audio track and an English subtitle track.
	fn mpls(items: &[(u32, u32)], marks: &[(u8, u16, u32)]) -> Vec<u8> {
		let mut stn = vec![0, 0, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0];
		stn.extend([9, 1, 0x10, 0x11, 0, 0, 0, 0, 0, 0, 5, 0x02, 0x31, 0, 0, 0]);
		stn.extend([9, 1, 0x11, 0x00, 0, 0, 0, 0, 0, 0, 5, 0x80, 0x31]);
		stn.extend(b"slv");
		stn.extend([9, 1, 0x12, 0x00, 0, 0, 0, 0, 0, 0, 5, 0x90]);
		stn.extend(b"eng");
		stn.push(0);

		let mut list = vec![0, 0];
		list.extend(u16::try_from(items.len()).unwrap().to_be_bytes());
		list.extend([0, 0]);
		for (index, (in_time, out_time)) in items.iter().enumerate() {
			let mut item = format!("{:05}M2TS", index + 1).into_bytes();
			item.extend([0, 1, 0]);
			item.extend(in_time.to_be_bytes());
			item.extend(out_time.to_be_bytes());
			item.extend([0; 12]);
			item.extend(u16::try_from(stn.len()).unwrap().to_be_bytes());
			item.extend(&stn);
			list.extend(u16::try_from(item.len()).unwrap().to_be_bytes());
			list.extend(item);
		}

		let mut mark = u16::try_from(marks.len()).unwrap().to_be_bytes().to_vec();
		for (kind, item, time) in marks {
			mark.extend([0, *kind]);
			mark.extend(item.to_be_bytes());
			mark.extend(time.to_be_bytes());
			mark.extend([0xFF, 0xFF, 0, 0, 0, 0]);
		}

		let app_info = [0, 0, 0, 14, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
		let list_at = 40 + app_info.len();
		let mark_at = list_at + 4 + list.len();
		let mut bytes = b"MPLS0200".to_vec();
		bytes.extend(u32::try_from(list_at).unwrap().to_be_bytes());
		bytes.extend(u32::try_from(mark_at).unwrap().to_be_bytes());
		bytes.extend([0; 24]);
		bytes.extend(app_info);
		bytes.extend(u32::try_from(list.len()).unwrap().to_be_bytes());
		bytes.extend(list);
		bytes.extend(u32::try_from(mark.len()).unwrap().to_be_bytes());
		bytes.extend(mark);
		bytes
	}

	#[test]
	fn entry_marks_are_seconds_from_the_playlist_start() {
		let bytes = mpls(
			&[(0, 200 * SECOND)],
			&[(1, 0, 0), (1, 0, 60 * SECOND), (1, 0, 125 * SECOND + SECOND / 2)],
		);

		assert_eq!(Mpls::parse(&bytes).unwrap().marks_s(), [0.0, 60.0, 125.5]);
	}

	#[test]
	fn marks_in_later_play_items_count_from_their_clip_in_time() {
		let bytes = mpls(
			&[(90_000, 90_000 + 10 * SECOND), (4500, 4500 + 20 * SECOND)],
			&[(1, 0, 90_000), (2, 1, 4500), (1, 1, 4500 + 5 * SECOND), (1, 7, 0)],
		);

		let parsed = Mpls::parse(&bytes).unwrap();

		assert_eq!(parsed.marks_s(), [0.0, 15.0]);
		assert!((parsed.length_s() - 30.0).abs() < 1e-9);
		assert_eq!(
			parsed.languages,
			[(0x1100, "slv".to_owned()), (0x1200, "eng".to_owned())]
		);
	}

	#[test]
	fn a_truncated_playlist_is_refused() {
		let bytes = mpls(&[(0, 200 * SECOND)], &[(1, 0, 0), (1, 0, 60 * SECOND)]);

		for length in [3, 20, bytes.len() / 2, bytes.len() - 5] {
			assert!(Mpls::parse(&bytes[..length]).is_err(), "{length}");
		}
		assert!(Mpls::parse(b"INDX0200").is_err());
	}

	#[test]
	fn the_picked_playlist_is_read_from_the_log() {
		let log: Vec<String> = [
			"[bluray @ 000001ff4169e980] 2 usable playlists:",
			"[bluray @ 000001ff4169e980] playlist 00800.mpls (0:03:20)",
			"[bluray @ 000001ff4169e980] playlist 00001.mpls (1:52:13)",
			"[bluray @ 000001ff4169e980] selected 00001.mpls",
			"Input #0, mpegts, from 'bluray:C:\\Discs\\Film.iso':",
		]
		.into_iter()
		.map(str::to_owned)
		.collect();

		assert_eq!(selected_playlist(&log), Some(1));
		assert_eq!(selected_playlist(&log[..3]), None);
		assert_eq!(
			selected_playlist(&["[bluray @ 0] selected 00800.mpls\r".to_owned()]),
			Some(800)
		);
	}

	#[test]
	fn chapters_run_from_mark_to_mark_and_to_the_end() {
		let chapters = chapter_list(&[60.0, 0.0, 0.01, 125.5, 200.0], 200.0);

		let listed: Vec<(f64, f64, &str)> = chapters
			.iter()
			.map(|chapter| (chapter.start_s, chapter.end_s, chapter.title.as_deref().unwrap()))
			.collect();
		assert_eq!(
			listed,
			[
				(0.0, 60.0, "Chapter 1"),
				(60.0, 125.5, "Chapter 2"),
				(125.5, 200.0, "Chapter 3")
			]
		);
	}

	#[test]
	fn libbluray_refusing_an_encrypted_disc_is_recognised() {
		let log = |line: &str| vec!["Input #0".to_owned(), line.to_owned()];

		assert!(encrypted(
			&log("[bluray @ 0000020c] Media stream encrypted with AACS, install and configure libaacs"),
			&[]
		));
		assert!(encrypted(
			&log("[bluray @ 0000020c] Your libaacs can't decrypt this media"),
			&[]
		));
		assert!(encrypted(
			&log("[bluray @ 0000020c] Unable to decrypt BD+ encrypted media"),
			&[]
		));
		assert!(!encrypted(&log("[bluray @ 0000020c] bd_open() failed"), &[]));
		assert!(!encrypted(&log("bluray:F:\\Film.iso: I/O error"), &[]));
	}

	#[test]
	fn a_path_that_says_encrypted_does_not_make_a_disc_encrypted() {
		let log = |line: &str| vec!["Input #0".to_owned(), line.to_owned()];
		let source = r"F:\Encrypted Hearts\Disc.iso";
		let url = r"bluray:F:\Encrypted Hearts\Disc.iso";

		assert!(!encrypted(
			&log(&format!("{url}: Invalid data found when processing input")),
			&echoed_paths(Path::new(source), url)
		));
		assert!(!encrypted(
			&log("f:/encrypted hearts/Disc.iso: I/O error"),
			&echoed_paths(Path::new(source), url)
		));
		assert!(encrypted(
			&log(&format!("{url}: Media stream encrypted with AACS")),
			&echoed_paths(Path::new(source), url)
		));
	}
}
