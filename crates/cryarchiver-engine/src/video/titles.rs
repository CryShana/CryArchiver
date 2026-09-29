//! Codec and channel words in the title of an audio track that is re-encoded to Opus. Only words that name the
//! track's own source codec, or its own channel layout, are rewritten, and only as whole words.

use crate::audio::plan::EncodePlan;
use crate::probe::Stream;

/// The codec words of each source codec, longer forms first so that `DTS-HD MA` is taken whole rather than as
/// `DTS`. Parts may be joined by one separator or none (`E-AC-3`, `EAC3`). Atmos rides on E-AC-3 and `TrueHD`, and
/// `DTS:X` on DTS: the Opus output carries neither, so they are codec words of those codecs.
const FAMILIES: &[(&[&str], &[&[&str]])] = &[
	(&["aac"], &[&["AAC"]]),
	(&["ac3"], &[&["Dolby", "Digital"], &["AC", "3"], &["DD"]]),
	(
		&["eac3"],
		&[
			&["Dolby", "Digital", "Plus"],
			&["Dolby", "Atmos"],
			&["E", "AC", "3"],
			&["DD+"],
			&["DDP"],
			&["Atmos"],
		],
	),
	(
		&["dts"],
		&[
			&["DTS", "HD", "Master", "Audio"],
			&["DTS", "HD", "MA"],
			&["DTS", "HD"],
			&["DTS:X"],
			&["DTS", "ES"],
			&["DTS"],
		],
	),
	(
		&["truehd", "mlp"],
		&[&["Dolby", "TrueHD"], &["Dolby", "Atmos"], &["True", "HD"], &["Atmos"]],
	),
	(&["flac"], &[&["FLAC"]]),
	(&["mp3"], &[&["MP3"]]),
	(&["vorbis"], &[&["Vorbis"]]),
];
const PCM_WORDS: &[&[&str]] = &[&["LPCM"], &["PCM"]];
const CHANNEL_WORDS: &[&str] = &["2.0", "5.1", "7.1"];
const SEPARATORS: &[char] = &[' ', '.', '-', '_'];

/// What the track was and what it becomes.
#[derive(Debug, Clone, Copy)]
pub struct Encode<'a> {
	/// The source track's codec, as ffprobe names it.
	pub codec: &'a str,
	/// The source's and the output's layouts as title words (`2.0`, `5.1`), when they have one.
	pub source_channels: Option<&'a str>,
	pub output_channels: Option<&'a str>,
}

/// `title` for the Opus track: the first word that names the source codec becomes `Opus`, the others are removed,
/// and a channel word equal to the source layout becomes the output's. `None` when nothing changes.
pub fn retitle(title: &str, encode: Encode<'_>) -> Option<String> {
	let codec_words = codec_words(encode.codec);
	let new_channels = encode
		.output_channels
		.filter(|output| Some(*output) != encode.source_channels);

	let mut words = Vec::new();
	let mut at = 0;
	let mut after_codec = false;
	while let Some(next) = title[at..].chars().next() {
		if let Some(end) = codec_word_end(title, at, codec_words) {
			words.push((at..end, true));
			at = end;
			after_codec = true;
			continue;
		}

		if let Some(end) = channel_word_end(title, at, after_codec) {
			if Some(&title[at..end]) == encode.source_channels && new_channels.is_some() {
				words.push((at..end, false));
			}
			at = end;
		} else {
			at += next.len_utf8();
		}
		after_codec = false;
	}

	let mut retitled = String::new();
	let mut last = 0;
	let mut named = false;
	for (word, is_codec) in words {
		let between = &title[last..word.start];
		last = word.end;
		if is_codec && named {
			retitled.push_str(between.strip_suffix(SEPARATORS).unwrap_or(between));
			continue;
		}

		retitled.push_str(between);
		if is_codec {
			named = true;
			retitled.push_str("Opus");
		} else if let Some(channels) = new_channels {
			retitled.push_str(channels);
		}
	}
	retitled.push_str(&title[last..]);

	(retitled != title).then_some(retitled)
}

/// The title of `stream` once it is encoded as `encoded`; `None` when it stays as it is.
pub fn encoded_title(stream: &Stream, encoded: &EncodePlan) -> Option<String> {
	retitle(
		stream.tag("title")?,
		Encode {
			codec: &stream.codec_name,
			source_channels: channel_words(stream.channel_layout.as_deref(), stream.channels),
			output_channels: channel_words(Some(encoded.layout.target()), Some(encoded.channels)),
		},
	)
}

/// A layout as a title word: `stereo` is `2.0`, `5.1(side)` is `5.1`. `channels` stands in when there is no layout.
pub fn channel_words(layout: Option<&str>, channels: Option<u32>) -> Option<&str> {
	match layout {
		Some("mono") => Some("1.0"),
		Some("stereo" | "downmix") => Some("2.0"),
		Some("quad") => Some("4.0"),
		Some(layout) => {
			let layout = layout.split('(').next()?;
			(layout.starts_with(|c: char| c.is_ascii_digit()) && layout.contains('.')).then_some(layout)
		}
		None => match channels? {
			1 => Some("1.0"),
			2 => Some("2.0"),
			6 => Some("5.1"),
			8 => Some("7.1"),
			_ => None,
		},
	}
}

fn codec_words(codec: &str) -> &'static [&'static [&'static str]] {
	if codec.starts_with("pcm_") {
		return PCM_WORDS;
	}

	FAMILIES
		.iter()
		.find(|(codecs, _)| codecs.contains(&codec))
		.map_or(&[], |(_, words)| words)
}

fn continues_word(byte: u8) -> bool {
	byte.is_ascii_alphanumeric() || byte == b'+'
}

/// Where a codec word from `forms` that starts at `at` ends. It must be a whole word, though channels may follow it
/// directly (`DDP5.1`).
fn codec_word_end(title: &str, at: usize, forms: &[&[&str]]) -> Option<usize> {
	let bytes = title.as_bytes();
	if at > 0 && continues_word(bytes[at - 1]) {
		return None;
	}

	forms.iter().find_map(|parts| {
		let mut end = at;
		for (ordinal, part) in parts.iter().enumerate() {
			if ordinal > 0 && title[end..].starts_with(SEPARATORS) {
				end += 1;
			}
			if !title.get(end..end + part.len())?.eq_ignore_ascii_case(part) {
				return None;
			}
			end += part.len();
		}

		let whole = bytes.get(end).is_none_or(|next| !continues_word(*next));
		(whole || channel_word_end(title, end, true).is_some()).then_some(end)
	})
}

/// Where a channel word that starts at `at` ends. It must be a whole word, unless it directly follows a codec word,
/// and never part of a number or a version such as `v5.1` or `7.1.2`.
fn channel_word_end(title: &str, at: usize, after_codec: bool) -> Option<usize> {
	let bytes = title.as_bytes();
	let word = CHANNEL_WORDS.iter().find(|word| title[at..].starts_with(**word))?;
	if at > 0 && !after_codec && (bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'.') {
		return None;
	}

	let end = at + word.len();
	let digit = |index: usize| bytes.get(index).is_some_and(u8::is_ascii_digit);
	let continues =
		bytes.get(end).is_some_and(u8::is_ascii_alphanumeric) || (bytes.get(end) == Some(&b'.') && digit(end + 1));
	(!continues).then_some(end)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn encode<'a>(codec: &'a str, source: &'a str, output: &'a str) -> Encode<'a> {
		Encode {
			codec,
			source_channels: Some(source),
			output_channels: Some(output),
		}
	}

	#[test]
	fn words_naming_the_source_codec_become_opus() {
		let aac = encode("aac", "2.0", "2.0");

		assert_eq!(retitle("English 2.0 AAC", aac).as_deref(), Some("English 2.0 Opus"));
		assert_eq!(
			retitle("Japanese 2.0 FLAC", encode("flac", "2.0", "2.0")).as_deref(),
			Some("Japanese 2.0 Opus")
		);
		assert_eq!(
			retitle("English 5.1 AC3", encode("ac3", "5.1", "2.0")).as_deref(),
			Some("English 2.0 Opus")
		);
		assert_eq!(
			retitle("Dolby TrueHD 7.1 Atmos", encode("truehd", "7.1", "5.1")).as_deref(),
			Some("Opus 5.1")
		);
		assert_eq!(
			retitle("English DDP5.1", encode("eac3", "5.1", "5.1")).as_deref(),
			Some("English Opus5.1")
		);
		assert_eq!(
			retitle("Commentary (E-AC-3)", encode("eac3", "2.0", "2.0")).as_deref(),
			Some("Commentary (Opus)")
		);
		assert_eq!(
			retitle("DTS-HD Master Audio 5.1", encode("dts", "5.1", "5.1")).as_deref(),
			Some("Opus 5.1")
		);
		assert_eq!(
			retitle("Film.2019.DTS-HD.MA.7.1-GROUP", encode("dts", "7.1", "7.1")).as_deref(),
			Some("Film.2019.Opus.7.1-GROUP")
		);
		assert_eq!(
			retitle("PCM 2.0", encode("pcm_s24le", "2.0", "2.0")).as_deref(),
			Some("Opus 2.0")
		);
	}

	#[test]
	fn dts_x_is_dts_with_objects_the_output_does_not_keep() {
		assert_eq!(
			retitle("English DTS:X 7.1", encode("dts", "7.1", "7.1")).as_deref(),
			Some("English Opus 7.1")
		);
	}

	#[test]
	fn words_of_other_codecs_and_other_text_are_left_alone() {
		let aac = encode("aac", "2.0", "2.0");

		for title in [
			"Atmos Live",
			"Magnum opus",
			"English Opus 2.0",
			"Commentary",
			"Film.2019.DTS-HD.MA.TrueHD.7.1.Atmos",
			"AACS",
			"Paccard",
		] {
			assert_eq!(retitle(title, aac), None, "{title}");
		}
		assert_eq!(
			retitle("Commentary (AAC, AC3)", aac).as_deref(),
			Some("Commentary (Opus, AC3)")
		);
	}

	#[test]
	fn channel_words_change_only_when_they_are_the_source_layout_and_it_changes() {
		let stereo = encode("aac", "2.0", "2.0");
		let downmix = encode("aac", "5.1", "2.0");

		assert_eq!(retitle("5.1 Surround Mix", stereo), None);
		assert_eq!(retitle("Surround 5.1", downmix).as_deref(), Some("Surround 2.0"));
		for title in ["v5.1 mix", "Windows 7.1.2", "Track 15.1", "Version 1.5.1"] {
			assert_eq!(retitle(title, encode("aac", "7.1", "2.0")), None, "{title}");
			assert_eq!(retitle(title, downmix), None, "{title}");
		}
	}

	#[test]
	fn layouts_become_channel_words() {
		assert_eq!(channel_words(Some("stereo"), Some(2)), Some("2.0"));
		assert_eq!(channel_words(Some("5.1(side)"), Some(6)), Some("5.1"));
		assert_eq!(channel_words(Some("7.1"), Some(8)), Some("7.1"));
		assert_eq!(channel_words(None, Some(6)), Some("5.1"));
		assert_eq!(channel_words(Some("hexagonal"), Some(6)), None);
	}
}
