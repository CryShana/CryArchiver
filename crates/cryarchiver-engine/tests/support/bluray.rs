use std::path::Path;

const TICKS: f64 = 45_000.0;
const ALIGNED_UNIT: usize = 6144;
const SOURCE_PACKET: usize = 192;
const VIDEO_PID: u16 = 0x1011;
const AUDIO_PID: u16 = 0x1100;
const SUBTITLE_PID: u16 = 0x1200;
/// Where the footage's clock starts, as ffmpeg's TS muxer writes it.
const CLIP_START: u32 = 64_800;

/// A playlist over the one clip: its number, the length it declares and its entry marks, in seconds.
pub struct Playlist {
	pub number: u32,
	pub declared_s: u32,
	pub marks_s: &'static [f64],
}

/// The files of a 2D Blu-ray, as paths from the disc root: one clip of `seconds` of 320×240 MPEG-2 with 24-bit LPCM
/// and a PGS track, and `playlists` over it, whose STN names the audio Slovenian and the subtitles English. libbluray
/// passes over playlists under three minutes and reads the clip whole, so a playlist may declare more than the
/// footage holds. The footage is generated in `dir`.
pub fn bdmv(dir: &Path, seconds: u32, playlists: &[Playlist]) -> Vec<(String, Vec<u8>)> {
	let clip = m2ts(dir, seconds);
	let packets = u32::try_from(clip.len() / SOURCE_PACKET).unwrap();

	let mut files = vec![
		("BDMV/index.bdmv".to_owned(), index()),
		("BDMV/MovieObject.bdmv".to_owned(), movie_object()),
		("BDMV/CLIPINF/00001.clpi".to_owned(), clip_info(packets)),
		("BDMV/STREAM/00001.m2ts".to_owned(), clip),
	];
	for playlist in playlists {
		files.push((format!("BDMV/PLAYLIST/{:05}.mpls", playlist.number), mpls(playlist)));
	}
	files
}

fn m2ts(dir: &Path, seconds: u32) -> Vec<u8> {
	let picture = format!("testsrc2=s=320x240:r=25:d={seconds}");
	let tone = format!("sine=frequency=440:sample_rate=48000:duration={seconds}");
	let subtitles = super::pgs_box(dir, 1000, 3000);
	let path = dir.join("00001.m2ts");
	super::ffmpeg(&[
		"-f",
		"lavfi",
		"-i",
		&picture,
		"-f",
		"lavfi",
		"-i",
		&tone,
		"-i",
		subtitles.to_str().unwrap(),
		"-map",
		"0:v",
		"-map",
		"1:a",
		"-map",
		"2:s",
		"-c:v",
		"mpeg2video",
		"-b:v",
		"2M",
		"-c:a",
		"pcm_bluray",
		"-sample_fmt",
		"s32",
		"-ac",
		"2",
		"-c:s",
		"copy",
		"-f",
		"mpegts",
		"-mpegts_m2ts_mode",
		"1",
		path.to_str().unwrap(),
	]);

	let mut bytes = std::fs::read(&path).unwrap();
	while !bytes.len().is_multiple_of(ALIGNED_UNIT) {
		bytes.extend([0, 0, 0, 0, 0x47, 0x1F, 0xFF, 0x10]);
		bytes.extend([0xFF; SOURCE_PACKET - 8]);
	}
	bytes
}

fn index() -> Vec<u8> {
	let hdmv = [0x40, 0, 0, 0, 0x40, 0, 0, 0, 0, 0, 0, 0];
	let mut body = [hdmv, hdmv].concat();
	body.extend(1_u16.to_be_bytes());
	body.extend(hdmv);

	let mut bytes = b"INDX0200".to_vec();
	bytes.extend(78_u32.to_be_bytes());
	bytes.extend([0; 28]);
	bytes.extend(34_u32.to_be_bytes());
	bytes.extend([0; 34]);
	bytes.extend(u32::try_from(body.len()).unwrap().to_be_bytes());
	bytes.extend(body);
	bytes
}

fn movie_object() -> Vec<u8> {
	let data = [0, 0, 0, 0, 0, 1, 0, 0, 0, 0];
	let mut bytes = b"MOBJ0200".to_vec();
	bytes.extend([0; 32]);
	bytes.extend(u32::try_from(data.len()).unwrap().to_be_bytes());
	bytes.extend(data);
	bytes
}

/// A clip's info with no EP map: libbluray then reads the clip from its first packet to its last.
fn clip_info(packets: u32) -> Vec<u8> {
	let mut clip = vec![0, 0, 1, 1, 0, 0, 0, 0];
	clip.extend(6_000_000_u32.to_be_bytes());
	clip.extend(packets.to_be_bytes());
	clip.extend([0; 130]);

	let mut sequence = vec![0, 1, 0, 0, 0, 0, 1, 0];
	sequence.extend(VIDEO_PID.to_be_bytes());
	sequence.extend([0; 12]);
	let program = [0, 1, 0, 0, 0, 0, 0x01, 0x00, 0, 0];

	let sections: [&[u8]; 5] = [&clip, &sequence, &program, &[], &[]];
	let mut starts = Vec::new();
	let mut body = Vec::new();
	for section in sections {
		starts.push(u32::try_from(40 + body.len()).unwrap());
		body.extend(u32::try_from(section.len()).unwrap().to_be_bytes());
		body.extend(section);
	}

	let mut bytes = b"HDMV0200".to_vec();
	for start in &starts[1..] {
		bytes.extend(start.to_be_bytes());
	}
	bytes.resize(40, 0);
	bytes.extend(body);
	bytes
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn mpls(playlist: &Playlist) -> Vec<u8> {
	let ticks = |seconds: f64| CLIP_START + (seconds * TICKS).round() as u32;

	let mut stn = vec![0, 0, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0];
	for (pid, attributes) in [
		(VIDEO_PID, &[5, 0x02, 0x31, 0, 0, 0][..]),
		(AUDIO_PID, &[5, 0x80, 0x31, b's', b'l', b'v']),
		(SUBTITLE_PID, &[5, 0x90, b'e', b'n', b'g', 0]),
	] {
		stn.extend([9, 1]);
		stn.extend(pid.to_be_bytes());
		stn.extend([0; 6]);
		stn.extend(attributes);
	}

	let mut item = b"00001M2TS".to_vec();
	item.extend([0, 1, 0]);
	item.extend(CLIP_START.to_be_bytes());
	item.extend(ticks(f64::from(playlist.declared_s)).to_be_bytes());
	item.extend([0; 12]);
	item.extend(u16::try_from(stn.len()).unwrap().to_be_bytes());
	item.extend(stn);

	let mut list = vec![0, 0, 0, 1, 0, 0];
	list.extend(u16::try_from(item.len()).unwrap().to_be_bytes());
	list.extend(item);

	let mut marks = u16::try_from(playlist.marks_s.len()).unwrap().to_be_bytes().to_vec();
	for mark_s in playlist.marks_s {
		marks.extend([0, 1, 0, 0]);
		marks.extend(ticks(*mark_s).to_be_bytes());
		marks.extend([0xFF, 0xFF, 0, 0, 0, 0]);
	}

	let app_info = [0, 0, 0, 14, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
	let list_at = 40 + app_info.len();
	let marks_at = list_at + 4 + list.len();
	let mut bytes = b"MPLS0200".to_vec();
	bytes.extend(u32::try_from(list_at).unwrap().to_be_bytes());
	bytes.extend(u32::try_from(marks_at).unwrap().to_be_bytes());
	bytes.extend([0; 24]);
	bytes.extend(app_info);
	for section in [list, marks] {
		bytes.extend(u32::try_from(section.len()).unwrap().to_be_bytes());
		bytes.extend(section);
	}
	bytes
}
