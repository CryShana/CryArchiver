use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use tokio_util::sync::CancellationToken;

use crate::images::decode::deep_pix_fmt;
use crate::options::SubtitlePick;
use crate::outcome::{ErrorKind, IoSide, JobError};
use crate::probe::{self as ffprobe, MediaInfo, ProbeError, Stream};
use crate::process::{self, STALL_TIMEOUT, ToolCommand};
use crate::toolchain::Toolchain;
use crate::video::run_ffmpeg;
use crate::workspace::Workspace;

pub const SUBTITLE_FILE: &str = "subs.ass";
pub const BITMAP_FILE: &str = "subs.mkv";
pub const FONT_DIR: &str = "fonts";

const ASS_SCRIPT_HEIGHT: f64 = 288.0;
const ASS: &[&str] = &["ass", "ssa"];
const BITMAP: &[&str] = &["hdmv_pgs_subtitle", "dvd_subtitle", "dvb_subtitle", "xsub"];
const UNDRAWABLE: &[&str] = &["dvb_teletext", "arib_caption", "hdmv_text_subtitle"];
const TEXT_FILTERS: &[&str] = &["subtitles"];
const BITMAP_FILTERS: &[&str] = &["scale2ref", "overlay"];
/// A picture from sub2video that shares its timestamp with a video frame reaches `overlay` after that frame, which
/// then still shows the previous picture. Moving every picture this much earlier (under half a frame even at
/// 240 fps, more than Matroska's 1 ms rounding) puts it on the frame it starts on.
const SAME_FRAME_S: &str = "0.002";
/// Moves frames to 1 µs ticks first: with one tick per frame (AVI, or MP4 with timescale = fps), the shifts
/// round onto whole frames and the overlay's 2 ms lead vanishes.
const FINE_TIME_BASE: &str = "settb=AVTB";

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

/// A bitmap track drawn over the main video from `subs.mkv`, read as its own input from its start.
///
/// `subs.mkv` keeps the source's own timestamps, and ffmpeg subtracts its start from them. Every command puts the
/// video on the file's clock (`VideoStarts`) and the graph moves it past its seek, so `-itsoffset` of
/// `offset_s` puts `subs.mkv` on the same clock.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Overlay {
	/// Where `subs.mkv` starts, less the source file's start.
	pub offset_s: f64,
	/// The picture size the cues are drawn on, when `subs.mkv` doesn't carry it (DVD subtitles copied out of MP4).
	pub canvas: Option<(u32, u32)>,
	pub ten_bit: bool,
}

impl Overlay {
	/// The input options for `subs.mkv`, before its `-i`.
	pub fn input_args(&self) -> Vec<String> {
		let mut args = Vec::new();
		if let Some((width, height)) = self.canvas {
			args.extend(["-canvas_size".to_owned(), format!("{width}x{height}")]);
		}
		args.extend(["-itsoffset".to_owned(), format!("{:.6}", self.offset_s)]);
		args
	}
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
	if UNDRAWABLE.contains(&codec) {
		return Err(format!(
			"subtitle stream {} ({codec}) can't be burned: ffmpeg can't turn it into text or pictures to draw",
			chosen.index
		));
	}

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

	// Matroska can't hold xsub. The re-encode snaps its colours to the default DVD palette.
	let (codec, file) = match burn.kind {
		BurnKind::Ass => ("copy", SUBTITLE_FILE),
		BurnKind::Text => ("ass", SUBTITLE_FILE),
		BurnKind::Bitmap if codec_of(info, burn) == "xsub" => ("dvdsub", BITMAP_FILE),
		BurnKind::Bitmap => ("copy", BITMAP_FILE),
	};
	if burn.kind == BurnKind::Bitmap {
		cmd.arg("-copyts");
	}
	cmd.arg("-i")
		.arg(process::tool_path(source))
		.arg("-map")
		.arg(format!("0:{}", burn.stream))
		.args(["-c:s", codec]);
	if burn.kind == BurnKind::Bitmap {
		cmd.args(["-f", "matroska"]);
	}
	cmd.arg(file);
	cmd
}

/// Extracts the burned track into the workspace. For a bitmap track, returns where the extracted file starts.
pub async fn prepare(
	tools: &Toolchain,
	source: &Path,
	workspace: &Workspace,
	info: &MediaInfo,
	burn: Burn,
	cancel: &CancellationToken,
) -> Result<Option<f64>, JobError> {
	if burn.kind == BurnKind::Ass {
		let fonts = workspace.file(FONT_DIR);
		fs::create_dir_all(&fonts).map_err(|err| JobError::io(IoSide::Destination(&fonts), &err))?;
	}

	let cmd = extract_command(&tools.ffmpeg, source, workspace.dir(), info, burn);
	run_ffmpeg(&cmd, |_| {}, cancel).await?;
	if burn.kind != BurnKind::Bitmap {
		return Ok(None);
	}

	let extracted = ffprobe::probe(tools, &workspace.file(BITMAP_FILE), cancel)
		.await
		.map_err(|err| match err {
			ProbeError::Run(err) => err.into(),
			other => JobError::from(ErrorKind::EncodeFailed(format!(
				"the extracted subtitles can't be read: {other}"
			))),
		})?;
	Ok(Some(extracted.format.start_time.unwrap_or(0.0)))
}

/// `main` is the main video stream; `track_start_s` is what `prepare` returned.
pub fn overlay(info: &MediaInfo, main: u32, burn: Burn, track_start_s: f64) -> Overlay {
	let stream = |index: u32| info.streams.iter().find(|stream| stream.index == index);
	let main = stream(main);

	// Copied out of MP4, a DVD track's private data has its palette but no size, which the MP4 keeps in its track.
	let canvas = stream(burn.stream)
		.filter(|track| track.codec_name == "dvd_subtitle" && info.format.format_name.contains("mov"))
		.and_then(|track| {
			Some((
				track.width.filter(|&width| width > 0)?,
				track.height.filter(|&height| height > 0)?,
			))
		});

	Overlay {
		offset_s: track_start_s - info.format.start_time.unwrap_or(0.0),
		canvas,
		ten_bit: main.and_then(|main| main.pix_fmt.as_deref()).is_some_and(deep_pix_fmt),
	}
}

/// What this ffmpeg lacks to burn `burn`, such as "subtitles filter" or "pgssub decoder".
pub fn missing_tools(burn: Burn, info: &MediaInfo, tools: &Toolchain) -> Vec<String> {
	let codec = codec_of(info, burn);
	let (filters, decoders, encoders): (&[&str], Vec<&str>, &[&str]) = match burn.kind {
		BurnKind::Ass => (TEXT_FILTERS, vec!["ass"], &[]),
		BurnKind::Text => (TEXT_FILTERS, vec![decoder(codec), "ass"], &["ass"]),
		BurnKind::Bitmap if codec == "xsub" => (BITMAP_FILTERS, vec!["xsub", "dvdsub"], &["dvdsub"]),
		BurnKind::Bitmap => (BITMAP_FILTERS, vec![decoder(codec)], &[]),
	};

	let absent = |names: &[&str], have: &BTreeSet<String>, what: &str| -> Vec<String> {
		names
			.iter()
			.filter(|name| !have.contains(**name))
			.map(|name| format!("{name} {what}"))
			.collect()
	};
	[
		absent(filters, &tools.filters, "filter"),
		absent(&decoders, &tools.decoders, "decoder"),
		absent(encoders, &tools.encoders, "encoder"),
	]
	.concat()
}

fn codec_of(info: &MediaInfo, burn: Burn) -> &str {
	info.streams
		.iter()
		.find(|stream| stream.index == burn.stream)
		.map_or("", |stream| stream.codec_name.as_str())
}

fn decoder(codec: &str) -> &str {
	match codec {
		"hdmv_pgs_subtitle" => "pgssub",
		"dvd_subtitle" => "dvdsub",
		"dvb_subtitle" => "dvbsub",
		"eia_608" => "cc_dec",
		other => other,
	}
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

/// The main video's filter chain for a command whose input is seeked to `seek_s` of the source, after `cut` (the
/// seek's `trim_filter`, or empty). Seeking restarts the frames' timestamps near 0 while `subs.ass` keeps the
/// source's, so the frames are moved onto the source's timeline for the subtitle filter and back by the same amount
/// afterwards.
pub fn main_chain(cut: &str, subtitles: Option<&str>, filters: &str, seek_s: Option<f64>) -> String {
	let drawn = match (subtitles, seek_s) {
		(Some(subtitles), Some(start_s)) => {
			format!("{FINE_TIME_BASE},setpts=PTS+{start_s:.3}/TB,{subtitles},setpts=PTS-{start_s:.3}/TB")
		}
		(Some(subtitles), None) => subtitles.to_owned(),
		(None, _) => String::new(),
	};

	chained(&[cut, &drawn, filters])
}

/// The graph that draws `subs.mkv` (the `subtitles` input label) over `video` after `cut`, then runs `chain`, into
/// `out`. The bitmap input is read unseeked, so a cue already on screen at a seek is still drawn; the seeked video
/// is moved onto the source's timeline for the overlay, like `main_chain` does.
pub fn overlay_graph(
	(video, cut): (&str, &str),
	subtitles: &str,
	overlay: Overlay,
	seek_s: Option<f64>,
	chain: &str,
	out: &str,
) -> String {
	let (onto_source, back) = match seek_s {
		Some(start_s) => (
			chained(&[cut, FINE_TIME_BASE, &format!("setpts=PTS+{start_s:.3}/TB")]),
			format!("setpts=PTS-{start_s:.3}/TB"),
		),
		None => (chained(&[cut, FINE_TIME_BASE]), String::new()),
	};
	let (pictures, blend) = if overlay.ten_bit {
		("[bss]format=yuva420p10le[bsd];", ":format=yuv420p10")
	} else {
		("[bss]null[bsd];", "")
	};

	format!(
		"{video}{onto_source}[bv];{subtitles}setpts=PTS-{SAME_FRAME_S}/TB[bs];[bs][bv]scale2ref[bss][bvs];{pictures}\
		 [bvs][bsd]overlay=eof_action=pass{blend}{}{out}",
		prefixed(&chained(&[&back, chain]))
	)
}

fn chained(parts: &[&str]) -> String {
	parts
		.iter()
		.filter(|part| !part.is_empty())
		.copied()
		.collect::<Vec<_>>()
		.join(",")
}

fn prefixed(chain: &str) -> String {
	if chain.is_empty() {
		String::new()
	} else {
		format!(",{chain}")
	}
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
			main_chain("", Some(&ass), "format=yuv420p10le", Some(4.0)),
			"settb=AVTB,setpts=PTS+4.000/TB,subtitles=subs.ass:fontsdir=fonts,setpts=PTS-4.000/TB,format=yuv420p10le"
		);
		assert_eq!(
			main_chain("", Some(&ass), "format=yuv420p10le", None),
			"subtitles=subs.ass:fontsdir=fonts,format=yuv420p10le"
		);
		assert_eq!(
			main_chain("", None, "format=yuv420p10le", Some(4.0)),
			"format=yuv420p10le"
		);
		assert_eq!(main_chain("", None, "", None), "");
		assert_eq!(
			main_chain("trim=start=5", Some(&ass), "", Some(4.0)),
			"trim=start=5,settb=AVTB,setpts=PTS+4.000/TB,subtitles=subs.ass:fontsdir=fonts,setpts=PTS-4.000/TB"
		);
	}

	#[test]
	fn bitmaps_are_overlaid_from_their_own_input_on_the_source_timeline() {
		let eight = Overlay {
			offset_s: 0.0,
			canvas: None,
			ten_bit: false,
		};
		let ten = Overlay { ten_bit: true, ..eight };

		assert_eq!(
			overlay_graph(("[0:0]", ""), "[1:0]", eight, None, "format=yuv420p10le", "[burned]"),
			"[0:0]settb=AVTB[bv];[1:0]setpts=PTS-0.002/TB[bs];[bs][bv]scale2ref[bss][bvs];[bss]null[bsd];\
			 [bvs][bsd]overlay=eof_action=pass,format=yuv420p10le[burned]"
		);
		assert_eq!(
			overlay_graph(("[1:0]", "trim=start=5"), "[2:0]", ten, Some(4.0), "", "[r]"),
			"[1:0]trim=start=5,settb=AVTB,setpts=PTS+4.000/TB[bv];[2:0]setpts=PTS-0.002/TB[bs];\
			 [bs][bv]scale2ref[bss][bvs];[bss]format=yuva420p10le[bsd];\
			 [bvs][bsd]overlay=eof_action=pass:format=yuv420p10,setpts=PTS-4.000/TB[r]"
		);
	}

	#[test]
	fn the_overlay_puts_the_track_on_the_file_clock_of_its_source() {
		let mkv = info(
			r#"{"format":{"format_name":"matroska,webm","start_time":"1.000000"},"streams":[
			{"index":0,"codec_type":"video","codec_name":"h264","start_time":"1.200000","pix_fmt":"yuv420p10le"},
			{"index":1,"codec_type":"audio","codec_name":"aac","start_time":"1.000000"},
			{"index":2,"codec_type":"subtitle","codec_name":"hdmv_pgs_subtitle","start_time":"6.000000"}]}"#,
		);
		let m2ts = info(
			r#"{"format":{"format_name":"mpegts","start_time":"1.400000"},"streams":[
			{"index":0,"codec_type":"video","codec_name":"h264","start_time":"1.743000","pix_fmt":"yuv420p"},
			{"index":1,"codec_type":"audio","codec_name":"aac","start_time":"1.400000"},
			{"index":2,"codec_type":"video","codec_name":"h264","start_time":"1.643000"},
			{"index":3,"codec_type":"subtitle","codec_name":"hdmv_pgs_subtitle","start_time":"6.723000"}]}"#,
		);
		let pgs = |stream| Burn {
			stream,
			kind: BurnKind::Bitmap,
		};

		let from_mkv = overlay(&mkv, 0, pgs(2), 6.0);
		let from_m2ts = overlay(&m2ts, 0, pgs(3), 6.723);

		assert!((from_mkv.offset_s - 5.0).abs() < 1e-9, "{from_mkv:?}");
		assert!(from_mkv.ten_bit);
		assert_eq!(from_mkv.canvas, None);
		assert!((from_m2ts.offset_s - 5.323).abs() < 1e-9, "{from_m2ts:?}");
		assert!(!from_m2ts.ten_bit);
	}

	#[test]
	fn dvd_subtitles_from_mp4_are_drawn_on_the_canvas_of_their_track() {
		let json = |format: &str| {
			format!(
				r#"{{"format":{{"format_name":"{format}"}},"streams":[
				{{"index":0,"codec_type":"video","codec_name":"h264","width":1920,"height":1080}},
				{{"index":1,"codec_type":"subtitle","codec_name":"dvd_subtitle","width":720,"height":480}}]}}"#
			)
		};
		let dvd = Burn {
			stream: 1,
			kind: BurnKind::Bitmap,
		};

		let from_mp4 = overlay(&info(&json("mov,mp4,m4a,3gp,3g2,mj2")), 0, dvd, 5.0);
		let from_mkv = overlay(&info(&json("matroska,webm")), 0, dvd, 0.0);

		assert_eq!(from_mp4.canvas, Some((720, 480)));
		assert_eq!(
			from_mp4.input_args(),
			["-canvas_size", "720x480", "-itsoffset", "5.000000"]
		);
		assert_eq!(from_mkv.canvas, None);
		assert_eq!(from_mkv.input_args(), ["-itsoffset", "0.000000"]);
	}

	#[test]
	fn tracks_ffmpeg_can_not_draw_are_refused_by_name() {
		let teletext = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"},
			{"index":1,"codec_type":"subtitle","codec_name":"dvb_teletext"}]}"#,
		);

		let err = pick(&teletext, &SubtitlePick::Default).unwrap_err();

		assert!(err.contains("stream 1 (dvb_teletext) can't be burned"), "{err}");
	}

	#[test]
	fn bitmap_tracks_are_copied_into_matroska_on_the_source_timeline_and_xsub_reencoded() {
		let xsub = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"mpeg4"},
			{"index":1,"codec_type":"subtitle","codec_name":"xsub"},
			{"index":2,"codec_type":"subtitle","codec_name":"dvd_subtitle"}]}"#,
		);
		let bitmap = Burn {
			stream: 3,
			kind: BurnKind::Bitmap,
		};

		let pgs = joined(&extract_command(
			Path::new("ffmpeg"),
			Path::new("in.mkv"),
			Path::new("work"),
			&info(SUBTITLED),
			bitmap,
		));
		let dvd = joined(&extract_command(
			Path::new("ffmpeg"),
			Path::new("in.avi"),
			Path::new("work"),
			&xsub,
			Burn { stream: 1, ..bitmap },
		));
		let vobsub = joined(&extract_command(
			Path::new("ffmpeg"),
			Path::new("in.mp4"),
			Path::new("work"),
			&xsub,
			Burn { stream: 2, ..bitmap },
		));

		assert!(pgs.contains("-copyts -i "), "{pgs}");
		assert!(pgs.ends_with("-map 0:3 -c:s copy -f matroska subs.mkv"), "{pgs}");
		assert!(dvd.ends_with("-map 0:1 -c:s dvdsub -f matroska subs.mkv"), "{dvd}");
		assert!(vobsub.ends_with("-map 0:2 -c:s copy -f matroska subs.mkv"), "{vobsub}");
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
