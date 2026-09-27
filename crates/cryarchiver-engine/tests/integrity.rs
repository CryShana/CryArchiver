use std::fs;
use std::path::Path;

use cryarchiver_engine::integrity::find_damage;

fn write(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
	let path = dir.join(name);
	fs::write(&path, bytes).unwrap();
	path
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn mp4_box(kind: &[u8; 4], payload_len: usize) -> Vec<u8> {
	let mut bytes = u32::try_from(payload_len + 8).unwrap().to_be_bytes().to_vec();
	bytes.extend_from_slice(kind);
	bytes.resize(payload_len + 8, 0);
	bytes
}

fn ebml_header(doc_type: &str) -> Vec<u8> {
	let mut payload = vec![0x42, 0x82, 0x80 | u8::try_from(doc_type.len()).unwrap()];
	payload.extend_from_slice(doc_type.as_bytes());

	let mut bytes = vec![0x1A, 0x45, 0xDF, 0xA3, 0x80 | u8::try_from(payload.len()).unwrap()];
	bytes.extend_from_slice(&payload);
	bytes
}

fn segment(declared_len: u64, actual_len: usize) -> Vec<u8> {
	let mut bytes = vec![0x18, 0x53, 0x80, 0x67, 0x01];
	bytes.extend_from_slice(&declared_len.to_be_bytes()[1..]);
	bytes.resize(bytes.len() + actual_len, 0);
	bytes
}

#[test]
fn complete_mp4_and_mkv_pass() {
	let dir = tempfile::tempdir().unwrap();
	let mp4 = [mp4_box(b"ftyp", 16), mp4_box(b"moov", 100), mp4_box(b"mdat", 1000)].concat();
	let mkv = [ebml_header("matroska"), segment(64, 64)].concat();

	assert_eq!(find_damage(&write(dir.path(), "a.mp4", &mp4)).unwrap(), None);
	assert_eq!(find_damage(&write(dir.path(), "a.mkv", &mkv)).unwrap(), None);
}

#[test]
fn truncated_boxes_and_missing_moov_are_damage() {
	let dir = tempfile::tempdir().unwrap();
	let mut truncated = [mp4_box(b"ftyp", 16), mp4_box(b"moov", 100), mp4_box(b"mdat", 1000)].concat();
	truncated.truncate(600);
	let no_moov = [mp4_box(b"ftyp", 16), mp4_box(b"mdat", 1000)].concat();

	let reason = find_damage(&write(dir.path(), "cut.mp4", &truncated)).unwrap().unwrap();
	assert!(reason.contains("mdat"), "{reason}");
	assert!(
		find_damage(&write(dir.path(), "no-moov.mp4", &no_moov))
			.unwrap()
			.unwrap()
			.contains("moov")
	);
}

fn ftyp(major: [u8; 4], compatible: &[[u8; 4]]) -> Vec<u8> {
	let mut payload = major.to_vec();
	payload.extend_from_slice(&[0; 4]);
	for brand in compatible {
		payload.extend_from_slice(brand);
	}
	let mut bytes = u32::try_from(payload.len() + 8).unwrap().to_be_bytes().to_vec();
	bytes.extend_from_slice(b"ftyp");
	bytes.extend(payload);
	bytes
}

#[test]
fn still_images_need_meta_and_sequences_need_moov() {
	let dir = tempfile::tempdir().unwrap();
	let avif = [
		ftyp(*b"avif", &[*b"mif1", *b"miaf"]),
		mp4_box(b"meta", 64),
		mp4_box(b"mdat", 100),
	]
	.concat();
	let heic_without_meta = [ftyp(*b"heic", &[*b"mif1"]), mp4_box(b"mdat", 100)].concat();
	let sequence_without_moov = [
		ftyp(*b"avis", &[*b"avif", *b"msf1"]),
		mp4_box(b"meta", 64),
		mp4_box(b"mdat", 100),
	]
	.concat();

	assert_eq!(find_damage(&write(dir.path(), "a.avif", &avif)).unwrap(), None);
	assert_eq!(
		find_damage(&write(dir.path(), "a.heic", &heic_without_meta))
			.unwrap()
			.as_deref(),
		Some("no 'meta' box")
	);
	assert_eq!(
		find_damage(&write(dir.path(), "seq.avif", &sequence_without_moov))
			.unwrap()
			.as_deref(),
		Some("no 'moov' box")
	);
}

#[test]
fn a_last_box_running_to_the_end_of_file_is_fine() {
	let dir = tempfile::tempdir().unwrap();
	let mut to_eof = mp4_box(b"mdat", 0);
	to_eof[..4].copy_from_slice(&0u32.to_be_bytes());
	to_eof.resize(500, 0);
	let file = [mp4_box(b"ftyp", 16), mp4_box(b"moov", 100), to_eof].concat();

	assert_eq!(find_damage(&write(dir.path(), "eof.mp4", &file)).unwrap(), None);
}

#[test]
fn truncated_segments_and_bad_doc_types_are_damage() {
	let dir = tempfile::tempdir().unwrap();
	let cut = [ebml_header("matroska"), segment(1000, 64)].concat();
	let odd_type = [ebml_header("exotic"), segment(64, 64)].concat();
	let mut unknown_size = ebml_header("webm");
	unknown_size.extend_from_slice(&[0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
	unknown_size.resize(unknown_size.len() + 64, 0);

	assert!(
		find_damage(&write(dir.path(), "cut.mkv", &cut))
			.unwrap()
			.unwrap()
			.contains("truncated")
	);
	assert!(find_damage(&write(dir.path(), "odd.mkv", &odd_type)).unwrap().is_some());
	assert_eq!(
		find_damage(&write(dir.path(), "live.webm", &unknown_size)).unwrap(),
		None
	);
}

#[test]
fn other_containers_are_not_checked() {
	let dir = tempfile::tempdir().unwrap();

	assert_eq!(
		find_damage(&write(dir.path(), "a.mpg", &[0, 0, 1, 0xBA, 0x44, 0, 4, 0])).unwrap(),
		None
	);
	assert!(find_damage(&dir.path().join("missing.mp4")).is_err());
}

#[test]
#[ignore = "needs the local sample corpus (set CRY_CORPUS, default C:\\media-corpus)"]
fn corpus_structures() {
	let corpus =
		std::env::var_os("CRY_CORPUS").map_or_else(|| Path::new(r"C:\media-corpus").to_path_buf(), Into::into);

	for healthy in [
		"generated-valid/rotation_90.mp4",
		"generated-valid/subs_mov_text_chapters.mp4",
		"generated-valid/mov_timecode_tmcd.mov",
		"generated-valid/chapters_unicode.mkv",
		"transparent_video.webm",
		"short-film.mp4",
	] {
		assert_eq!(find_damage(&corpus.join(healthy)).unwrap(), None, "{healthy}");
	}
	for damaged in [
		"generated-corrupted/truncated_50pct_faststart.mp4",
		"generated-corrupted/truncated_30pct_moov-at-end.mp4",
		"generated-corrupted/truncated_60pct.mkv",
		"generated-corrupted/truncated_50pct_alpha.webm",
	] {
		assert!(find_damage(&corpus.join(damaged)).unwrap().is_some(), "{damaged}");
	}
}
