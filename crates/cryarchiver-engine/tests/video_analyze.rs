mod support;

use std::path::{Path, PathBuf};

use cryarchiver_engine::probe::probe;
use cryarchiver_engine::video::analyze::{DEFAULT_SEGMENTS, NoiseClass, ScanType, analyze};
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
#[ignore = "needs the local sample corpus (set CRY_CORPUS, default C:\\media-corpus)"]
async fn corpus_scan_types() {
	let corpus = std::env::var_os("CRY_CORPUS").map_or_else(|| PathBuf::from(r"C:\media-corpus"), PathBuf::from);
	let valid = corpus.join("generated-valid");

	assert_eq!(
		scan_and_noise(&valid.join("interlaced_tff_480i_h264.mkv")).await.0,
		ScanType::Interlaced { top_first: true }
	);
	assert_eq!(
		scan_and_noise(&valid.join("interlaced_bff_576i_mpeg2.mpg")).await.0,
		ScanType::Interlaced { top_first: false }
	);
	assert_eq!(
		scan_and_noise(&valid.join("telecined_480i_mpeg2.mpg")).await.0,
		ScanType::Telecined
	);
	assert_eq!(
		scan_and_noise(&valid.join("hfr_120fps.mp4")).await.0,
		ScanType::Progressive
	);
	assert_eq!(
		scan_and_noise(&valid.join("very_short_0.4s.mp4")).await.0,
		ScanType::Progressive
	);
	assert_eq!(
		scan_and_noise(&corpus.join("concert-MPEG2_1080-interlaced_sample.mkv"))
			.await
			.0,
		ScanType::Interlaced { top_first: true }
	);
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

	let analysis = Box::pin(analyze(&tools, &clip, main, info.format.duration, 8, &cancel))
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
