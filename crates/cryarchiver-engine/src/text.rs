use std::ffi::OsStr;
use std::fmt::Write;
use std::path::Path;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub fn width(s: &str) -> usize {
	s.graphemes(true).map(grapheme_width).sum()
}

pub fn fit(s: &str, max: usize) -> String {
	if width(s) <= max {
		return pad(s, max);
	}

	if max == 0 {
		return String::new();
	}

	pad(&format!("{}…", take_front(s, max - 1)), max)
}

pub fn fit_middle(s: &str, max: usize) -> String {
	if width(s) <= max {
		return pad(s, max);
	}

	if max < 8 {
		return fit(s, max);
	}

	let tail = take_back(s, (extension_width(s) + 4).min(max / 2));
	let head = take_front(s, max - width(&tail) - 1);
	pad(&format!("{head}…{tail}"), max)
}

pub fn display_name(name: &OsStr) -> String {
	let mut out = String::new();
	for piece in decode(name) {
		match piece {
			Ok(c) => push_escaped(&mut out, c),
			Err(escape) => out.push_str(&escape),
		}
	}
	out
}

pub fn display_path(path: &Path) -> String {
	display_name(path.as_os_str())
}

fn grapheme_width(g: &str) -> usize {
	UnicodeWidthStr::width(g).clamp(1, 2)
}

fn take_front(s: &str, budget: usize) -> String {
	let mut out = String::new();
	let mut used = 0;
	for g in s.graphemes(true) {
		let w = grapheme_width(g);
		if used + w > budget {
			break;
		}
		out.push_str(g);
		used += w;
	}
	out
}

fn take_back(s: &str, budget: usize) -> String {
	let mut parts = Vec::new();
	let mut used = 0;
	for g in s.graphemes(true).rev() {
		let w = grapheme_width(g);
		if used + w > budget {
			break;
		}
		parts.push(g);
		used += w;
	}
	parts.reverse();
	parts.concat()
}

fn extension_width(s: &str) -> usize {
	match s.rfind('.') {
		Some(i) if i > 0 && s.len() - i <= 7 => width(&s[i..]),
		_ => 0,
	}
}

fn pad(s: &str, max: usize) -> String {
	let mut out = s.to_owned();
	out.extend(std::iter::repeat_n(' ', max.saturating_sub(width(s))));
	out
}

fn push_escaped(out: &mut String, c: char) {
	match c {
		'\n' => out.push_str("\\n"),
		'\r' => out.push_str("\\r"),
		'\t' => out.push_str("\\t"),
		c if c.is_control() => {
			let _ = write!(out, "\\x{:02x}", u32::from(c));
		}
		c => out.push(c),
	}
}

#[cfg(unix)]
fn decode(name: &OsStr) -> Vec<Result<char, String>> {
	use std::os::unix::ffi::OsStrExt;

	let mut out = Vec::new();
	for chunk in name.as_bytes().utf8_chunks() {
		out.extend(chunk.valid().chars().map(Ok));
		out.extend(chunk.invalid().iter().map(|b| Err(format!("\\x{b:02x}"))));
	}
	out
}

#[cfg(windows)]
fn decode(name: &OsStr) -> Vec<Result<char, String>> {
	use std::os::windows::ffi::OsStrExt;

	char::decode_utf16(name.encode_wide())
		.map(|r| r.map_err(|e| format!("\\u{{{:04x}}}", e.unpaired_surrogate())))
		.collect()
}

#[cfg(test)]
mod tests {
	use std::ffi::OsStr;
	use std::path::Path;

	use super::*;

	#[test]
	fn width_counts_cells() {
		assert_eq!(width("abc"), 3);
		assert_eq!(width("日本語"), 6);
		assert_eq!(width("cafe\u{301}"), 4);
		assert_eq!(width("🎬"), 2);
	}

	#[test]
	fn fit_pads_and_truncates() {
		assert_eq!(fit("hello", 8), "hello   ");
		assert_eq!(fit("hello world", 8), "hello w…");
		assert_eq!(fit("日本語ファイル名.jpg", 9), "日本語フ…");
	}

	#[test]
	fn fit_middle_keeps_extension() {
		assert_eq!(
			fit_middle("2018 - mountain trail - GoPro.mkv", 24),
			"2018 - mountain…oPro.mkv"
		);
	}

	#[test]
	fn fit_middle_cjk_and_emoji() {
		let shown = fit_middle("日本語のとても長いファイル名.mkv", 20);

		assert_eq!(shown, "日本語のと…ル名.mkv ");
		assert_eq!(width(&shown), 20);
		assert_eq!(fit("🎬 clip.mp4", 6), "🎬 cl…");
	}

	#[test]
	fn display_name_escapes_controls() {
		assert_eq!(display_name(OsStr::new("a\nb\u{1b}c\td")), "a\\nb\\x1bc\\td");
		assert_eq!(display_name(OsStr::new("02. 夜明け.mp3")), "02. 夜明け.mp3");
		assert_eq!(display_path(Path::new("dir/a\u{7}.flac")), "dir/a\\x07.flac");
	}

	#[cfg(unix)]
	#[test]
	fn display_name_shows_invalid_utf8() {
		use std::os::unix::ffi::OsStrExt;

		assert_eq!(display_name(OsStr::from_bytes(b"bad\xffname")), "bad\\xffname");
	}

	#[cfg(windows)]
	#[test]
	fn display_name_shows_unpaired_surrogate() {
		use std::ffi::OsString;
		use std::os::windows::ffi::OsStringExt;

		let name = OsString::from_wide(&[u16::from(b'a'), 0xD800, u16::from(b'b')]);

		assert_eq!(display_name(&name), "a\\u{d800}b");
	}
}
