mod support;

use std::fs;
use std::path::Path;

use cryarchiver_engine::container::MATROSKA_VIDEO;
use cryarchiver_engine::preset::Speed;
use cryarchiver_engine::probe::probe;
use cryarchiver_engine::video::analyze::{Analysis, NoiseClass, ScanType};
use cryarchiver_engine::video::mux::{Attachment, ChapterSource, MuxInputs, MuxPlan, command, mux};
use cryarchiver_engine::video::plan::{VideoCaps, plan_video, precheck};
use cryarchiver_engine::video::streams::{StreamRules, plan_streams};
use tokio_util::sync::CancellationToken;

fn source_with_everything(dir: &Path) -> std::path::PathBuf {
	let subs = dir.join("subs.srt");
	let chapters = dir.join("chapters.txt");
	let font = dir.join("font.ttf");
	fs::write(&subs, "1\n00:00:00,000 --> 00:00:01,000\nHello\n").unwrap();
	fs::write(
		&chapters,
		";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=1000\ntitle=One\n\
		 [CHAPTER]\nTIMEBASE=1/1000\nSTART=1000\nEND=2000\ntitle=Two\n",
	)
	.unwrap();
	fs::write(&font, b"not really a font").unwrap();

	let source = dir.join("source.mkv");
	support::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		"testsrc2=s=320x240:r=25:d=2",
		"-f",
		"lavfi",
		"-i",
		"sine=duration=2",
		"-f",
		"lavfi",
		"-i",
		"sine=frequency=880:duration=2",
		"-i",
		subs.to_str().unwrap(),
		"-f",
		"ffmetadata",
		"-i",
		chapters.to_str().unwrap(),
		"-map",
		"0:v",
		"-map",
		"1:a",
		"-map",
		"2:a",
		"-map",
		"3:s",
		"-map_chapters",
		"4",
		"-c:v",
		"mpeg4",
		"-c:a:0",
		"aac",
		"-c:a:1",
		"libopus",
		"-b:a:1",
		"32k",
		"-c:s",
		"srt",
		"-metadata:s:a:0",
		"language=eng",
		"-metadata:s:a:1",
		"language=jpn",
		"-metadata:s:a:1",
		"title=Commentary",
		"-metadata:s:s:0",
		"language=slv",
		"-attach",
		font.to_str().unwrap(),
		"-metadata:s:t:0",
		"mimetype=application/x-truetype-font",
		"-metadata:s:t:0",
		"filename=font.ttf",
		source.to_str().unwrap(),
	]);
	source
}

#[tokio::test]
async fn mux_combines_temps_source_streams_and_attachments() {
	let dir = tempfile::tempdir().unwrap();
	let source = source_with_everything(dir.path());
	let tools = support::toolchain().await;
	let cancel = CancellationToken::new();

	let info = probe(&tools, &source, &cancel).await.unwrap();
	let streams = plan_streams(&info, &StreamRules::new(Speed::Fast, false)).unwrap();
	let analysis = Analysis {
		scan: ScanType::Progressive,
		noise: NoiseClass::Clean,
		noise_db: None,
		detail: Vec::new(),
	};
	let caps = VideoCaps {
		vp9_alpha: false,
		libaom: false,
	};
	let main = precheck(&info, &caps).unwrap();
	let video = plan_video(&info, main, &[], &analysis, Speed::Fast);

	let video_temp = dir.path().join("video.a1.mkv");
	let audio_temp = dir.path().join("audio.mka");
	let s = source.to_str().unwrap();
	support::ffmpeg(&["-i", s, "-map", "0:v", "-c:v", "mpeg4", video_temp.to_str().unwrap()]);
	support::ffmpeg(&[
		"-i",
		s,
		"-map",
		"0:a:0",
		"-c:a",
		"libopus",
		"-b:a",
		"48k",
		audio_temp.to_str().unwrap(),
	]);
	let telemetry = dir.path().join("stream-9.bin");
	fs::write(&telemetry, b"\x01\x02\x03").unwrap();
	let attachments = [Attachment {
		path: telemetry,
		file_name: "stream-9-gpmd.bin".into(),
		mime_type: "application/octet-stream".into(),
	}];

	let output = dir.path().join("mux.mkv");
	let cmd = command(
		&tools.ffmpeg,
		&MuxInputs {
			video: &video_temp,
			audio: Some(&audio_temp),
			source: &source,
			source_bytes: fs::metadata(&source).unwrap().len(),
			attachments: &attachments,
		},
		&MuxPlan {
			info: &info,
			video: &video,
			streams: &streams,
			keep_metadata: false,
			container: MATROSKA_VIDEO,
			chapters: ChapterSource::Source,
		},
		&output,
	);
	mux(&cmd, &cancel).await.unwrap();

	let muxed = probe(&tools, &output, &cancel).await.unwrap();
	let kinds: Vec<&str> = muxed.streams.iter().map(|stream| stream.codec_type.as_str()).collect();
	assert_eq!(
		kinds,
		["video", "audio", "audio", "subtitle", "attachment", "attachment"]
	);
	assert_eq!(muxed.streams[1].codec_name, "opus");
	assert_eq!(muxed.streams[1].tag("language"), Some("eng"));
	assert_eq!(muxed.streams[2].tag("language"), Some("jpn"));
	assert_eq!(muxed.streams[2].tag("title"), Some("Commentary"));
	assert_eq!(muxed.streams[3].tag("language"), Some("slv"));
	assert_eq!(muxed.streams[4].tag("filename"), Some("font.ttf"));
	assert_eq!(muxed.streams[5].tag("filename"), Some("stream-9-gpmd.bin"));
	assert_eq!(muxed.chapters.len(), 2);
}
