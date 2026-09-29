use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use tokio_util::sync::CancellationToken;

use crate::options::SubtitlePick;
use crate::outcome::{IoSide, JobError};
use crate::probe::{MediaInfo, Stream};
use crate::process::{self, STALL_TIMEOUT, ToolCommand};
use crate::video::run_ffmpeg;
use crate::workspace::Workspace;

pub const SUBTITLE_FILE: &str = "subs.ass";
pub const FONT_DIR: &str = "fonts";

const ASS_SCRIPT_HEIGHT: f64 = 288.0;
const ASS: &[&str] = &["ass", "ssa"];
const BITMAP: &[&str] = &["hdmv_pgs_subtitle", "dvd_subtitle", "dvb_subtitle", "xsub"];
const TEXT_FILTERS: &[&str] = &["subtitles"];
const BITMAP_FILTERS: &[&str] = &["scale2ref", "overlay"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BurnKind {
	Text,
	Ass,
	Bitmap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Burn {
	pub stream: u32,
	pub kind: BurnKind,
}

pub fn pick(info: &MediaInfo, pick: &SubtitlePick) -> Result<Burn, String> {
	let subtitles: Vec<&Stream> = info
		.streams
		.iter()
		.filter(|stream| stream.codec_type == "subtitle")
		.collect();
	let chosen = match pick {
		SubtitlePick::Default => subtitles
			.iter()
			.find(|stream| stream.disposition.get("default") == Some(&1))
			.or(subtitles.first())
			.ok_or_else(|| "there is no subtitle track to burn".to_owned())?,
		SubtitlePick::Track(number) => usize::try_from(*number)
			.ok()
			.and_then(|number| number.checked_sub(1))
			.and_then(|index| subtitles.get(index))
			.ok_or_else(|| format!("there is no subtitle track {number} ({} in the file)", subtitles.len()))?,
		SubtitlePick::Language(code) => subtitles
			.iter()
			.find(|stream| {
				stream
					.tag("language")
					.is_some_and(|language| language.eq_ignore_ascii_case(code))
			})
			.ok_or_else(|| format!("there is no subtitle track in the language {code}"))?,
	};

	let codec = chosen.codec_name.as_str();
	let kind = if ASS.contains(&codec) {
		BurnKind::Ass
	} else if BITMAP.contains(&codec) {
		BurnKind::Bitmap
	} else {
		BurnKind::Text
	};
	Ok(Burn {
		stream: chosen.index,
		kind,
	})
}

pub fn extract_command(
	ffmpeg: &Path,
	source: &Path,
	workspace_dir: &Path,
	info: &MediaInfo,
	burn: Burn,
) -> ToolCommand {
	let mut cmd = ToolCommand::new(ffmpeg, STALL_TIMEOUT);
	cmd.in_dir(workspace_dir);
	cmd.args(["-hide_banner", "-nostdin", "-y"]);

	if burn.kind == BurnKind::Ass {
		let attachments = info.streams.iter().filter(|stream| stream.codec_type == "attachment");
		for (ordinal, stream) in attachments.enumerate() {
			let extension = if stream.codec_name == "otf" { "otf" } else { "ttf" };
			cmd.arg(format!("-dump_attachment:t:{ordinal}"))
				.arg(format!("{FONT_DIR}/attachment-{ordinal}.{extension}"));
		}
	}

	let codec = if burn.kind == BurnKind::Ass { "copy" } else { "ass" };
	cmd.arg("-i")
		.arg(process::tool_path(source))
		.arg("-map")
		.arg(format!("0:{}", burn.stream))
		.args(["-c:s", codec, SUBTITLE_FILE]);
	cmd
}

pub async fn prepare(
	ffmpeg: &Path,
	source: &Path,
	workspace: &Workspace,
	info: &MediaInfo,
	burn: Burn,
	cancel: &CancellationToken,
) -> Result<(), JobError> {
	if burn.kind == BurnKind::Bitmap {
		return Ok(());
	}

	if burn.kind == BurnKind::Ass {
		let fonts = workspace.file(FONT_DIR);
		fs::create_dir_all(&fonts).map_err(|err| JobError::io(IoSide::Destination(&fonts), &err))?;
	}

	run_ffmpeg(
		&extract_command(ffmpeg, source, workspace.dir(), info, burn),
		|_| {},
		cancel,
	)
	.await?;
	Ok(())
}

pub fn missing_filters(kind: BurnKind, filters: &BTreeSet<String>) -> Vec<&'static str> {
	let needed = match kind {
		BurnKind::Text | BurnKind::Ass => TEXT_FILTERS,
		BurnKind::Bitmap => BITMAP_FILTERS,
	};
	needed.iter().copied().filter(|name| !filters.contains(*name)).collect()
}

pub fn subtitle_filter(kind: BurnKind, size_percent: u8) -> Option<String> {
	match kind {
		BurnKind::Bitmap => None,
		BurnKind::Ass => Some(format!("subtitles={SUBTITLE_FILE}:fontsdir={FONT_DIR}")),
		BurnKind::Text => Some(format!(
			"subtitles={SUBTITLE_FILE}:force_style='FontSize={:.1},PrimaryColour=&H00FFFFFF,\
			 OutlineColour=&H00000000,BorderStyle=1,Outline=2,Shadow=0,Alignment=2'",
			f64::from(size_percent) * ASS_SCRIPT_HEIGHT / 100.0
		)),
	}
}

/// The main video's filter chain for a command whose input is seeked to `seek_s` of the source. Seeking restarts
/// the frames' timestamps at 0 while `subs.ass` keeps the source's, so the frames are moved onto the source's
/// timeline for the subtitle filter and back afterwards.
pub fn main_chain(subtitles: Option<&str>, filters: &str, seek_s: Option<f64>) -> String {
	let drawn = match (subtitles, seek_s) {
		(Some(subtitles), Some(start_s)) => format!("setpts=PTS+{start_s:.3}/TB,{subtitles},setpts=PTS-STARTPTS"),
		(Some(subtitles), None) => subtitles.to_owned(),
		(None, _) => String::new(),
	};

	[drawn.as_str(), filters]
		.into_iter()
		.filter(|part| !part.is_empty())
		.collect::<Vec<_>>()
		.join(",")
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::video::joined;

	fn info(json: &str) -> MediaInfo {
		MediaInfo::parse(json.as_bytes()).unwrap()
	}

	const SUBTITLED: &str = r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"},
		{"index":1,"codec_type":"subtitle","codec_name":"subrip","tags":{"language":"eng"}},
		{"index":2,"codec_type":"subtitle","codec_name":"ass","tags":{"language":"jpn"},"disposition":{"default":1}},
		{"index":3,"codec_type":"subtitle","codec_name":"hdmv_pgs_subtitle"},
		{"index":4,"codec_type":"attachment","codec_name":"ttf"},
		{"index":5,"codec_type":"attachment","codec_name":"otf"}]}"#;

	#[test]
	fn the_default_a_number_or_a_language_picks_the_track() {
		let subtitled = info(SUBTITLED);

		assert_eq!(
			pick(&subtitled, &SubtitlePick::Default),
			Ok(Burn {
				stream: 2,
				kind: BurnKind::Ass
			})
		);
		assert_eq!(
			pick(&subtitled, &SubtitlePick::Track(3)),
			Ok(Burn {
				stream: 3,
				kind: BurnKind::Bitmap
			})
		);
		assert_eq!(
			pick(&subtitled, &SubtitlePick::Language("eng".into())),
			Ok(Burn {
				stream: 1,
				kind: BurnKind::Text
			})
		);
		assert!(
			pick(&subtitled, &SubtitlePick::Track(4))
				.unwrap_err()
				.contains("track 4")
		);
		assert!(
			pick(&subtitled, &SubtitlePick::Language("slv".into()))
				.unwrap_err()
				.contains("slv")
		);
		assert!(pick(&info(r#"{"streams":[]}"#), &SubtitlePick::Default).is_err());
	}

	#[test]
	fn text_is_drawn_in_one_style_and_shifted_to_the_seek() {
		let ass = subtitle_filter(BurnKind::Ass, 5).unwrap();

		assert_eq!(
			subtitle_filter(BurnKind::Text, 5).unwrap(),
			"subtitles=subs.ass:force_style='FontSize=14.4,PrimaryColour=&H00FFFFFF,OutlineColour=&H00000000,BorderStyle=1,Outline=2,Shadow=0,Alignment=2'"
		);
		assert_eq!(ass, "subtitles=subs.ass:fontsdir=fonts");
		assert_eq!(subtitle_filter(BurnKind::Bitmap, 5), None);
		assert_eq!(
			main_chain(Some(&ass), "format=yuv420p10le", Some(4.0)),
			"setpts=PTS+4.000/TB,subtitles=subs.ass:fontsdir=fonts,setpts=PTS-STARTPTS,format=yuv420p10le"
		);
		assert_eq!(
			main_chain(Some(&ass), "format=yuv420p10le", None),
			"subtitles=subs.ass:fontsdir=fonts,format=yuv420p10le"
		);
		assert_eq!(main_chain(None, "format=yuv420p10le", Some(4.0)), "format=yuv420p10le");
		assert_eq!(main_chain(None, "", None), "");
	}

	#[test]
	fn each_kind_needs_its_filters() {
		let filters: BTreeSet<String> = ["subtitles", "overlay"].map(String::from).into();

		assert!(missing_filters(BurnKind::Text, &filters).is_empty());
		assert!(missing_filters(BurnKind::Ass, &filters).is_empty());
		assert_eq!(missing_filters(BurnKind::Bitmap, &filters), ["scale2ref"]);
		assert_eq!(missing_filters(BurnKind::Text, &BTreeSet::new()), ["subtitles"]);
	}

	#[test]
	fn ass_extraction_takes_the_fonts_under_fixed_names() {
		let subtitled = info(SUBTITLED);

		let cmd = extract_command(
			Path::new("ffmpeg"),
			Path::new("in.mkv"),
			Path::new("work"),
			&subtitled,
			Burn {
				stream: 2,
				kind: BurnKind::Ass,
			},
		);

		let args = joined(&cmd);
		assert_eq!(cmd.current_dir.as_deref(), Some(Path::new("work")));
		assert!(
			args.contains(
				"-dump_attachment:t:0 fonts/attachment-0.ttf -dump_attachment:t:1 fonts/attachment-1.otf -i "
			),
			"{args}"
		);
		assert!(args.ends_with("-map 0:2 -c:s copy subs.ass"), "{args}");
	}
}
