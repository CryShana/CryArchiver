mod support;

use std::path::{Path, PathBuf};

use cryarchiver_engine::probe::probe;
use cryarchiver_engine::video::analyze::{DEFAULT_SEGMENTS, NoiseClass, ScanType, analyze};
use cryarchiver_engine::video::plan::grain_level;
use tokio_util::sync::CancellationToken;

fn encode(dir: &Path, name: &str, source: &str, args: &[&str]) -> PathBuf {
	let path = dir.join(name);
	let mut all = vec!["-f", "lavfi", "-i", source];
	all.extend_from_slice(args);
	all.push(path.to_str().unwrap());
	support::ffmpeg(&all);
	path
}

async fn scan_and_noise(path: &Path) -> (ScanType, NoiseClass) {
	let tools = support::toolchain().await;
	let cancel = CancellationToken::new();
	let info = probe(&tools, path, &cancel).await.unwrap();
	let main = info.streams.iter().find(|stream| stream.is_real_video()).unwrap();

	let analysis = Box::pin(analyze(
		&tools,
		path,
		main,
		info.format.duration,
		DEFAULT_SEGMENTS,
		None,
		&cancel,
	))
	.await
	.unwrap();
	(analysis.scan, analysis.noise)
}

const INTERLACED_MPEG2: &[&str] = &["-c:v", "mpeg2video", "-b:v", "12M", "-flags", "+ilme+ildct"];

#[tokio::test]
async fn detects_interlaced_tff_and_bff() {
	let dir = tempfile::tempdir().unwrap();
	let tff = encode(
		dir.path(),
		"tff.mpg",
		"testsrc2=s=720x576:r=50:d=4,interlace=scan=tff,setfield=tff",
		INTERLACED_MPEG2,
	);
	let bff = encode(
		dir.path(),
		"bff.mpg",
		"testsrc2=s=720x576:r=50:d=4,interlace=scan=bff,setfield=bff",
		INTERLACED_MPEG2,
	);

	assert_eq!(scan_and_noise(&tff).await.0, ScanType::Interlaced { top_first: true });
	assert_eq!(scan_and_noise(&bff).await.0, ScanType::Interlaced { top_first: false });
}

#[tokio::test]
async fn detects_telecine_and_progressive() {
	let dir = tempfile::tempdir().unwrap();
	let telecined = encode(
		dir.path(),
		"telecined.mpg",
		"testsrc2=s=720x480:r=24000/1001:d=6,telecine=first_field=top:pattern=23,setfield=tff",
		&["-c:v", "mpeg2video", "-b:v", "12M", "-flags", "+ilme+ildct"],
	);
	let progressive = encode(
		dir.path(),
		"progressive.mp4",
		"testsrc2=s=640x360:r=30:d=14",
		&["-c:v", "mpeg4", "-q:v", "2"],
	);

	assert_eq!(scan_and_noise(&telecined).await.0, ScanType::Telecined);
	assert_eq!(
		scan_and_noise(&progressive).await,
		(ScanType::Progressive, NoiseClass::Clean)
	);
}

#[tokio::test]
async fn interlaced_h264_high_frame_rates_and_very_short_clips_are_detected() {
	let dir = tempfile::tempdir().unwrap();
	let h264 = encode(
		dir.path(),
		"tff_480i.mkv",
		"testsrc2=s=720x480:r=60000/1001:d=4,interlace=scan=tff,setfield=tff",
		&[
			"-c:v",
			"libx264",
			"-crf",
			"18",
			"-flags",
			"+ilme+ildct",
			"-x264opts",
			"tff=1",
		],
	);
	let high_rate = encode(
		dir.path(),
		"hfr.mp4",
		"testsrc2=s=320x240:r=120:d=3",
		&["-c:v", "mpeg4", "-q:v", "2"],
	);
	let very_short = encode(
		dir.path(),
		"short.mp4",
		"testsrc2=s=320x240:r=25:d=0.4",
		&["-c:v", "mpeg4", "-q:v", "2"],
	);

	assert_eq!(scan_and_noise(&h264).await.0, ScanType::Interlaced { top_first: true });
	assert_eq!(scan_and_noise(&high_rate).await.0, ScanType::Progressive);
	assert_eq!(scan_and_noise(&very_short).await.0, ScanType::Progressive);
}

#[tokio::test]
#[ignore = "needs a grainy real clip (set CRY_GRAIN_CLIP to its path)"]
async fn a_strongly_grainy_real_clip_gets_the_strongest_grain() {
	let Some(clip) = std::env::var_os("CRY_GRAIN_CLIP").map(PathBuf::from) else {
		return;
	};
	let tools = support::toolchain().await;
	let cancel = CancellationToken::new();
	let info = probe(&tools, &clip, &cancel).await.unwrap();
	let main = info.streams.iter().find(|stream| stream.is_real_video()).unwrap();

	let analysis = Box::pin(analyze(
		&tools,
		&clip,
		main,
		info.format.duration,
		DEFAULT_SEGMENTS,
		None,
		&cancel,
	))
	.await
	.unwrap();

	assert_eq!(analysis.noise, NoiseClass::Light, "{:?}", analysis.noise_db);
	assert_eq!(grain_level(analysis.noise_db), Some(20), "{:?}", analysis.noise_db);
}

#[tokio::test]
async fn detail_is_measured_for_every_segment() {
	let dir = tempfile::tempdir().unwrap();
	let clip = encode(
		dir.path(),
		"detailed.mp4",
		"testsrc2=s=640x360:r=30:d=14",
		&["-c:v", "mpeg4", "-q:v", "2"],
	);
	let tools = support::toolchain().await;
	let cancel = CancellationToken::new();
	let info = probe(&tools, &clip, &cancel).await.unwrap();
	let main = info.streams.iter().find(|stream| stream.is_real_video()).unwrap();

	let analysis = Box::pin(analyze(&tools, &clip, main, info.format.duration, 8, None, &cancel))
		.await
		.unwrap();

	assert_eq!(analysis.detail.len(), 8);
	assert!(
		analysis
			.detail
			.iter()
			.all(|detail| detail.usable && detail.si.is_some_and(|si| si > 15.0)),
		"{:?}",
		analysis.detail
	);
}
