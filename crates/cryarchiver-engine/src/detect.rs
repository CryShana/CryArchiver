use std::fs::{self, File, Metadata};
use std::io::{self, Read};
use std::path::Path;

use infer::MatcherType;
use tokio_util::sync::CancellationToken;

use crate::disc::udf::{self, DiscFiles};
use crate::outcome::{ErrorKind, IoSide, JobError};
use crate::probe::{self, MediaInfo, ProbeError};
use crate::toolchain::Toolchain;

const HEADER_BYTES: u64 = 8192;
pub const THREE_D_KEPT: &str = "3D disc: the second view can't be encoded; kept so 3D survives";
const DISC_ROOTS: [&str; 2] = ["VIDEO_TS", "BDMV"];
const TEXT_LIKE_FORMATS: &[&str] = &[
	"lrc",
	"srt",
	"ass",
	"webvtt",
	"tty",
	"jacosub",
	"microdvd",
	"mpl2",
	"pjs",
	"realtext",
	"sami",
	"stl",
	"subviewer",
	"subviewer1",
	"vplayer",
	"aqtitle",
];
const RAW_EXTENSIONS: &[&str] = &[
	"nef", "nrw", "cr2", "cr3", "arw", "srf", "sr2", "dng", "orf", "rw2", "raf", "pef", "srw", "x3f", "3fr", "iiq",
	"erf", "mef", "mos",
];
const LAYERED_EXTENSIONS: &[&str] = &["psd", "psb", "xcf", "kra", "ora"];
const VECTOR_EXTENSIONS: &[&str] = &["svg", "svgz"];
const ICON_EXTENSIONS: &[&str] = &["ico", "cur"];
const ICON_MIME_TYPES: &[&str] = &["image/vnd.microsoft.icon", "image/x-icon"];
const TEXTURE_SIGNATURES: &[&[u8]] = &[b"DDS ", b"\xABKTX 11\xBB", b"\xABKTX 20\xBB"];
const FLOAT_SIGNATURES: &[&[u8]] = &[&[0x76, 0x2F, 0x31, 0x01], b"#?RADIANCE", b"#?RGBE"];
const ANIMATED_FORMATS: &[&str] = &["apng", "webp_anim"];
const ANIMATED_CODECS: &[&str] = &["jpegxl_anim"];
// Any unreadable signature not listed here must stay Damaged: archival mode copies Other files as they are.
const UNSUPPORTED_MIME_TYPES: &[&str] = &[
	"audio/midi",
	"image/vnd.dwg",
	"image/vnd.djvu",
	"image/openraster",
	"image/vnd.ms-photo",
];

#[derive(Debug)]
pub enum MediaClass {
	Video(MediaInfo),
	Audio(MediaInfo),
	StillImage(MediaInfo),
	AnimatedImage(MediaInfo),
	Kept(String),
	Damaged(String),
	Other(String),
	Disc(DiscKind),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscKind {
	Dvd(DiscFiles),
	BluRay { files: DiscFiles, three_d: bool },
}

impl DiscKind {
	pub fn files(&self) -> &DiscFiles {
		match self {
			Self::Dvd(files) | Self::BluRay { files, .. } => files,
		}
	}
}

#[derive(Debug)]
pub struct Detection {
	pub meta: Metadata,
	pub class: MediaClass,
	pub probe_log: Vec<String>,
}

pub async fn detect(tools: &Toolchain, path: &Path, cancel: &CancellationToken) -> Result<Detection, JobError> {
	let (meta, header) = match read_header(path) {
		Ok(read) => read,
		Err(err) => return detect_folder(path).unwrap_or_else(|| Err(JobError::io(IoSide::Source(path), &err))),
	};

	if meta.len() > 0
		&& let Some(reason) = never_transcoded(path, &header)
	{
		return Ok(Detection {
			meta,
			class: MediaClass::Kept(reason),
			probe_log: Vec::new(),
		});
	}

	if meta.len() > 0 && is_iso(path) {
		let files = udf::read_image(path).map_err(|err| JobError::io(IoSide::Source(path), &err))?;
		return Ok(Detection {
			meta,
			class: files.map_or_else(
				|| MediaClass::Other(not_a_disc("image")),
				|files| disc_class(files, "image"),
			),
			probe_log: Vec::new(),
		});
	}

	if let Some(class) = classify_header(meta.len(), &header) {
		return Ok(Detection {
			meta,
			class,
			probe_log: Vec::new(),
		});
	}

	let (info, probe_log) = match probe::probe(tools, path, cancel).await {
		Ok(info) => (Some(info), Vec::new()),
		Err(ProbeError::Failed(log)) => (None, log),
		Err(ProbeError::Run(err)) => return Err(err.into()),
		Err(err @ ProbeError::Json(_)) => return Err(ErrorKind::Toolchain(err.to_string()).into()),
	};
	Ok(Detection {
		class: classify(&header, info),
		meta,
		probe_log,
	})
}

fn is_iso(path: &Path) -> bool {
	path.extension()
		.is_some_and(|extension| extension.eq_ignore_ascii_case("iso"))
}

/// A folder that is, or directly holds, `VIDEO_TS` or `BDMV`, as a disc; `None` for anything else.
fn detect_folder(path: &Path) -> Option<Result<Detection, JobError>> {
	let meta = fs::metadata(path).ok().filter(Metadata::is_dir)?;
	let files = match disc_folder(path) {
		Ok(files) => files?,
		Err(err) => return Some(Err(JobError::io(IoSide::Source(path), &err))),
	};

	Some(Ok(Detection {
		meta,
		class: disc_class(files, "folder"),
		probe_log: Vec::new(),
	}))
}

/// The files of the `VIDEO_TS` or `BDMV` folder that `path` is or holds, with paths from the disc root.
fn disc_folder(path: &Path) -> io::Result<Option<DiscFiles>> {
	for root in DISC_ROOTS {
		let named = path.file_name().is_some_and(|name| name.eq_ignore_ascii_case(root));
		let dir = if named { path.to_path_buf() } else { path.join(root) };
		if !dir.is_dir() {
			continue;
		}

		let mut files = udf::read_folder(&dir)?;
		for file in &mut files.files {
			file.path = format!("{root}/{}", file.path);
		}
		for sub in &mut files.dirs {
			*sub = format!("{root}/{sub}");
		}
		files.dirs.push(root.to_owned());
		return Ok(Some(files));
	}
	Ok(None)
}

fn disc_class(files: DiscFiles, what: &str) -> MediaClass {
	if files.find("VIDEO_TS/VIDEO_TS.IFO").is_some() {
		return MediaClass::Disc(DiscKind::Dvd(files));
	}
	if files.find("BDMV/index.bdmv").is_none() {
		return MediaClass::Other(not_a_disc(what));
	}

	let three_d = files.dir_exists("BDMV/STREAM/SSIF");
	if three_d {
		MediaClass::Kept(THREE_D_KEPT.to_owned())
	} else {
		MediaClass::Disc(DiscKind::BluRay { files, three_d })
	}
}

fn not_a_disc(what: &str) -> String {
	format!("not a DVD or Blu-ray {what}")
}

pub fn read_header(path: &Path) -> io::Result<(Metadata, Vec<u8>)> {
	let file = File::open(path)?;
	let meta = file.metadata()?;

	let mut header = Vec::new();
	file.take(HEADER_BYTES).read_to_end(&mut header)?;
	Ok((meta, header))
}

pub fn never_transcoded(path: &Path, header: &[u8]) -> Option<String> {
	let extension = path
		.extension()
		.map(|extension| extension.to_string_lossy().to_ascii_lowercase())
		.unwrap_or_default();
	let extension = extension.as_str();
	let mime = infer::get(header).map(|kind| kind.mime_type());

	let kind = if RAW_EXTENSIONS.contains(&extension) {
		"RAW camera"
	} else if header.starts_with(b"8BPS") || header.starts_with(b"gimp xcf") || LAYERED_EXTENSIONS.contains(&extension)
	{
		"layered image"
	} else if VECTOR_EXTENSIONS.contains(&extension) {
		"vector image"
	} else if mime.is_some_and(|mime| ICON_MIME_TYPES.contains(&mime))
		|| (ICON_EXTENSIONS.contains(&extension) && matches!(header, [0, 0, 1 | 2, 0, ..]))
	{
		"icon"
	} else if TEXTURE_SIGNATURES.iter().any(|signature| header.starts_with(signature)) {
		"GPU texture"
	} else if FLOAT_SIGNATURES.iter().any(|signature| header.starts_with(signature)) {
		"floating-point HDR image"
	} else {
		return None;
	};
	Some(format!("{kind} files are never converted"))
}

pub fn classify_header(size: u64, header: &[u8]) -> Option<MediaClass> {
	if size == 0 {
		return Some(MediaClass::Other("empty file".into()));
	}

	let kind = infer::get(header)?;
	let media = matches!(
		kind.matcher_type(),
		MatcherType::Audio | MatcherType::Video | MatcherType::Image
	);
	(!media).then(|| MediaClass::Other(format!("not media: {}", kind.mime_type())))
}

pub fn classify(header: &[u8], info: Option<MediaInfo>) -> MediaClass {
	let magic = infer::get(header);
	let Some(info) = info else {
		return match magic {
			Some(kind) if UNSUPPORTED_MIME_TYPES.contains(&kind.mime_type()) => {
				MediaClass::Other(format!("not supported: {}", kind.mime_type()))
			}
			Some(kind) => MediaClass::Damaged(format!("{} signature but the file cannot be read", kind.mime_type())),
			None => MediaClass::Other("not a readable media file".into()),
		};
	};

	let format = info.format.format_name.as_str();
	let has_av = info.streams.iter().any(|stream| stream.is_audio() || stream.is_video());
	if magic.is_none() && (TEXT_LIKE_FORMATS.contains(&format) || !has_av) {
		return MediaClass::Other(format!("not media: detected only as {format}"));
	}

	let image_demuxer = format == "image2" || format.ends_with("_pipe");
	let image_magic = magic.is_some_and(|kind| kind.matcher_type() == MatcherType::Image)
		|| (image_demuxer && unlisted_image_signature(header, &info));
	if image_demuxer && !image_magic {
		return MediaClass::Other("the extension says image, but the content does not match".into());
	}

	if image_magic {
		let animated = ANIMATED_FORMATS.contains(&format)
			|| info
				.streams
				.iter()
				.any(|stream| ANIMATED_CODECS.contains(&stream.codec_name.as_str()))
			|| info.format.duration.is_some_and(|duration| duration > 0.05);
		return if animated {
			MediaClass::AnimatedImage(info)
		} else {
			MediaClass::StillImage(info)
		};
	}

	if info
		.streams
		.iter()
		.any(|stream| stream.is_video() && !stream.is_attached_pic())
	{
		return MediaClass::Video(info);
	}

	if info.audio_streams().next().is_some() {
		return MediaClass::Audio(info);
	}
	MediaClass::Other("no audio or video streams".into())
}

/// QOI, PNM and TGA images, which `infer` doesn't know. TGA has no signature, so its fixed header must describe a
/// plausible image that ffprobe also read as TGA.
fn unlisted_image_signature(header: &[u8], info: &MediaInfo) -> bool {
	match header {
		[b'q', b'o', b'i', b'f', ..] => return true,
		[b'P', b'1'..=b'7', next, ..] if next.is_ascii_whitespace() => return true,
		_ => {}
	}

	let targa = info.streams.first().is_some_and(|stream| stream.codec_name == "targa");
	let Some(&[_, colour_map, kind, _, _, _, _, _, _, _, _, _, w0, w1, h0, h1, depth, _]) = header.get(..18) else {
		return false;
	};
	let mapped = matches!(kind, 1 | 9);

	targa
		&& matches!(kind, 1 | 2 | 3 | 9 | 10 | 11)
		&& (colour_map == 1 || (colour_map == 0 && !mapped))
		&& u16::from_le_bytes([w0, w1]) > 0
		&& u16::from_le_bytes([h0, h1]) > 0
		&& matches!(depth, 8 | 15 | 16 | 24 | 32)
}

#[cfg(test)]
mod tests {
	use super::*;

	const FLAC: &[u8] = b"fLaC\0\0\0\x22\x10\x00\x10\x00";
	const ID3: &[u8] = b"ID3\x03\0\0\0\0\0\x21";
	const MP4: &[u8] = b"\0\0\0\x20ftypisom\0\0\x02\0isomiso2avc1mp41";
	const HEIC: &[u8] = b"\0\0\0\x18ftypheic\0\0\0\0mif1heic";
	const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
	const HTML: &[u8] = b"<html>\r\n<head><title>503 Service Temporarily Unavailable</title></head>";
	const MIDI: &[u8] = b"MThd\0\0\0\x06\0\x01\0\x02\x01\xe0MTrk";

	#[allow(clippy::unnecessary_wraps)]
	fn info(json: &str) -> Option<MediaInfo> {
		Some(MediaInfo::parse(json.as_bytes()).unwrap())
	}

	#[test]
	fn empty_files_and_documents_need_no_probe() {
		assert!(matches!(classify_header(0, b""), Some(MediaClass::Other(reason)) if reason == "empty file"));
		assert!(matches!(classify_header(190, HTML), Some(MediaClass::Other(reason)) if reason.contains("text/html")));
		assert!(classify_header(654_201, MP4).is_none());
		assert!(classify_header(73, b"This is plain text").is_none());
	}

	#[test]
	fn media_signature_without_probe_is_damaged() {
		assert!(matches!(classify(MP4, None), MediaClass::Damaged(_)));
		assert!(matches!(classify(b"This is plain text", None), MediaClass::Other(_)));
	}

	#[test]
	fn formats_ffmpeg_cannot_decode_are_not_supported_but_unreadable_audio_is_damaged() {
		assert!(matches!(classify(MIDI, None), MediaClass::Other(reason) if reason.contains("not supported")));
		assert!(matches!(classify(FLAC, None), MediaClass::Damaged(_)));
	}

	#[test]
	fn cover_art_does_not_make_audio_a_video() {
		let mp3 = info(
			r#"{"streams":[{"index":0,"codec_type":"audio","codec_name":"mp3"},
			{"index":1,"codec_type":"video","codec_name":"mjpeg","disposition":{"attached_pic":1}}],
			"format":{"format_name":"mp3"}}"#,
		);

		assert!(matches!(classify(ID3, mp3), MediaClass::Audio(_)));
	}

	#[test]
	fn content_beats_extension_for_video_named_mp3() {
		let mp4 = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"},
			{"index":1,"codec_type":"audio","codec_name":"aac"}],"format":{"format_name":"mov,mp4,m4a,3gp,3g2,mj2"}}"#,
		);

		assert!(matches!(classify(MP4, mp4), MediaClass::Video(_)));
	}

	#[test]
	fn weak_detections_are_not_trusted() {
		let lrc = info(
			r#"{"streams":[{"index":0,"codec_type":"subtitle","codec_name":"text"}],"format":{"format_name":"lrc"}}"#,
		);
		let fake_jpeg = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"mjpeg"}],"format":{"format_name":"image2"}}"#,
		);

		assert!(matches!(classify(&[0x13, 0x37, 0x42], lrc), MediaClass::Other(_)));
		assert!(matches!(classify(&[0xE0, 0x00, 0x10], fake_jpeg), MediaClass::Other(_)));
	}

	#[test]
	fn images_are_routed_by_content() {
		let png = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"png"}],"format":{"format_name":"png_pipe"}}"#,
		);
		let gif = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"gif"}],"format":{"format_name":"gif","duration":"3.0"}}"#,
		);
		let heic = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"hevc"}],"format":{"format_name":"mov,mp4,m4a,3gp,3g2,mj2"}}"#,
		);

		assert!(matches!(classify(PNG, png), MediaClass::StillImage(_)));
		assert!(matches!(classify(b"GIF89a\x40\x01", gif), MediaClass::AnimatedImage(_)));
		assert!(matches!(classify(HEIC, heic), MediaClass::StillImage(_)));
	}

	#[test]
	fn qoi_pnm_and_tga_are_trusted_by_their_own_headers() {
		let probed = |format: &str, codec: &str| {
			info(&format!(
				r#"{{"streams":[{{"index":0,"codec_type":"video","codec_name":"{codec}"}}],"format":{{"format_name":"{format}"}}}}"#
			))
		};
		let tga = [&[0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 192, 0, 32, 8][..], &[0; 16]].concat();

		assert!(matches!(
			classify(b"qoif\0\0\x01\0\0\0\0\xc0\x04\0", probed("qoi_pipe", "qoi")),
			MediaClass::StillImage(_)
		));
		assert!(matches!(
			classify(b"P6\n256 192\n255\n", probed("ppm_pipe", "ppm")),
			MediaClass::StillImage(_)
		));
		assert!(matches!(
			classify(&tga, probed("image2", "targa")),
			MediaClass::StillImage(_)
		));
		assert!(matches!(
			classify(
				b"<!DOCTYPE html><html><body>503</body></html>",
				probed("image2", "targa")
			),
			MediaClass::Other(_)
		));
		assert!(matches!(
			classify(&tga, probed("image2", "mjpeg")),
			MediaClass::Other(_)
		));
	}

	#[test]
	fn headers_that_only_look_like_pnm_or_tga_are_not_trusted() {
		let probed = |format: &str, codec: &str| {
			info(&format!(
				r#"{{"streams":[{{"index":0,"codec_type":"video","codec_name":"{codec}"}}],"format":{{"format_name":"{format}"}}}}"#
			))
		};
		let tga = |colour_map: u8, kind: u8, width: u16, height: u16, depth: u8| {
			let mut header = vec![0, colour_map, kind, 0, 0, 0, 0, 0, 0, 0, 0, 0];
			header.extend(width.to_le_bytes());
			header.extend(height.to_le_bytes());
			header.extend([depth, 8]);
			header.extend([0; 16]);
			header
		};
		let trusted =
			|header: &[u8], codec: &str| matches!(classify(header, probed("image2", codec)), MediaClass::StillImage(_));

		assert!(trusted(&tga(0, 2, 256, 192, 32), "targa"));
		assert!(trusted(&tga(1, 1, 256, 192, 8), "targa"));
		assert!(!trusted(&tga(0, 2, 0, 192, 32), "targa"), "zero width");
		assert!(!trusted(&tga(0, 2, 256, 0, 32), "targa"), "zero height");
		assert!(!trusted(&tga(0, 2, 256, 192, 12), "targa"), "depth");
		assert!(!trusted(&tga(0, 4, 256, 192, 32), "targa"), "image type");
		assert!(!trusted(&tga(2, 2, 256, 192, 32), "targa"), "colour map type");
		assert!(trusted(&tga(1, 9, 256, 192, 8), "targa"));
		assert!(
			!trusted(&tga(0, 9, 256, 192, 8), "targa"),
			"a mapped image needs its map"
		);
		assert!(!trusted(&tga(0, 2, 256, 192, 32)[..17], "targa"), "a header cut short");
		assert!(!trusted(&tga(0, 2, 256, 192, 32), "mjpeg"), "ffprobe must agree");

		let pnm = |header: &[u8]| matches!(classify(header, probed("image2", "pam")), MediaClass::StillImage(_));
		assert!(pnm(b"P7\nWIDTH 4\nHEIGHT 4\n"));
		assert!(pnm(b"P1 4 4\n"));
		assert!(!pnm(b"P6x256 192\n"), "no whitespace after the magic");
		assert!(!pnm(b"P7WIDTH 4\n"), "no whitespace after P7");
		assert!(!pnm(b"P8\n4 4\n"), "not a PNM magic");
		assert!(!pnm(b"P0 4 4\n"), "not a PNM magic");
		assert!(!pnm(b"P6"), "the magic alone");
	}

	#[test]
	fn apng_and_animated_webp_are_animated_without_a_duration() {
		let apng = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"apng"}],"format":{"format_name":"apng"}}"#,
		);
		let webp = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"webp_anim"}],"format":{"format_name":"webp_anim"}}"#,
		);
		let still_webp = info(
			r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"webp"}],"format":{"format_name":"webp_pipe"}}"#,
		);

		assert!(matches!(classify(PNG, apng), MediaClass::AnimatedImage(_)));
		assert!(matches!(
			classify(b"RIFF\0\0\0\0WEBPVP8X", webp),
			MediaClass::AnimatedImage(_)
		));
		assert!(matches!(
			classify(b"RIFF\0\0\0\0WEBPVP8 ", still_webp),
			MediaClass::StillImage(_)
		));
	}

	#[test]
	fn never_transcoded_images_are_recognised_by_extension_or_signature() {
		let tiff = b"II*\0\x08\0\0\0";
		let never = |name: &str, header: &[u8]| never_transcoded(std::path::Path::new(name), header);

		assert_eq!(
			never("DSC_0001.NEF", tiff).as_deref(),
			Some("RAW camera files are never converted")
		);
		assert!(never("art.png", b"8BPS\0\x01").unwrap().contains("layered"));
		assert!(never("logo.svg", b"<?xml version").unwrap().contains("vector"));
		assert!(
			never("favicon.ico", &[0, 0, 1, 0, 1, 0, 16, 16])
				.unwrap()
				.contains("icon")
		);
		assert!(never("normal.dds", b"DDS \x7c\0\0\0").unwrap().contains("texture"));
		assert!(
			never("sky.exr", &[0x76, 0x2F, 0x31, 0x01, 2, 0, 0, 0])
				.unwrap()
				.contains("floating-point")
		);
		assert_eq!(never("photo.tif", tiff), None);
		assert_eq!(never("photo.png", PNG), None);
	}

	fn folder_class(path: &Path) -> MediaClass {
		disc_class(disc_folder(path).unwrap().expect("a disc folder"), "folder")
	}

	#[test]
	fn a_folder_that_is_or_holds_video_ts_is_a_dvd_with_paths_from_the_disc_root() {
		let dir = tempfile::tempdir().unwrap();
		let video_ts = dir.path().join("Izlet").join("VIDEO_TS");
		std::fs::create_dir_all(&video_ts).unwrap();
		std::fs::write(video_ts.join("VIDEO_TS.IFO"), b"DVDVIDEO-VMG").unwrap();

		for path in [dir.path().join("Izlet"), video_ts] {
			let MediaClass::Disc(DiscKind::Dvd(files)) = folder_class(&path) else {
				panic!("{}", path.display())
			};
			assert!(files.find("VIDEO_TS/VIDEO_TS.IFO").is_some(), "{files:?}");
			assert!(files.dir_exists("VIDEO_TS"));
		}
		assert!(disc_folder(dir.path()).unwrap().is_none());
	}

	#[test]
	fn a_blu_ray_folder_is_kept_when_it_holds_a_second_view() {
		let dir = tempfile::tempdir().unwrap();
		let bdmv = dir.path().join("BDMV");
		std::fs::create_dir_all(bdmv.join("STREAM")).unwrap();
		std::fs::write(bdmv.join("index.bdmv"), b"INDX0200").unwrap();

		assert!(matches!(
			folder_class(dir.path()),
			MediaClass::Disc(DiscKind::BluRay { three_d: false, .. })
		));

		std::fs::create_dir(bdmv.join("STREAM").join("SSIF")).unwrap();
		assert!(matches!(folder_class(dir.path()), MediaClass::Kept(reason) if reason == THREE_D_KEPT));
	}

	#[test]
	fn flac_is_audio() {
		let flac = info(
			r#"{"streams":[{"index":0,"codec_type":"audio","codec_name":"flac"}],"format":{"format_name":"flac"}}"#,
		);

		assert!(matches!(classify(FLAC, flac), MediaClass::Audio(_)));
	}
}
