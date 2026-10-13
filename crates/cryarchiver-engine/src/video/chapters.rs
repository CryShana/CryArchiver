use std::fmt::Write;
use std::path::Path;

use serde_json::Value;

use crate::options::TrimWindow;
use crate::outcome::JobError;
use crate::workspace::write_work_file;

/// How near a chapter may start to its title's edges, or to another chapter, and still be one.
pub const EDGE_S: f64 = 0.05;
/// A chapter shorter than this, as a disc's tiny last cell makes, is merged into its neighbour.
pub const MIN_CHAPTER_S: f64 = 1.0;

#[derive(Debug, Clone, PartialEq)]
pub struct Chapter {
	pub start_s: f64,
	pub end_s: f64,
	pub title: Option<String>,
}

pub fn clip(chapters: &[Value], window: TrimWindow) -> Vec<Chapter> {
	chapters
		.iter()
		.filter_map(|chapter| {
			let (start_s, end_s) = window.clip(seconds(&chapter["start_time"])?, seconds(&chapter["end_time"])?)?;
			Some(Chapter {
				start_s,
				end_s,
				title: chapter["tags"]["title"].as_str().map(str::to_owned),
			})
		})
		.collect()
}

/// A chapter from each of `starts`, named, to the next, the last to `end_s`. A chapter under `MIN_CHAPTER_S` joins
/// the one before it; a first one that short takes in the one after it, keeping its own start and name.
pub fn from_starts(starts: Vec<(f64, String)>, end_s: f64) -> Vec<Chapter> {
	let stops: Vec<f64> = starts
		.iter()
		.skip(1)
		.map(|(start_s, _)| *start_s)
		.chain([end_s])
		.collect();
	let short = |chapter: &Chapter| chapter.end_s - chapter.start_s < MIN_CHAPTER_S;

	let mut chapters: Vec<Chapter> = Vec::with_capacity(starts.len());
	for ((start_s, title), end_s) in starts.into_iter().zip(stops) {
		let chapter = Chapter {
			start_s,
			end_s,
			title: Some(title),
		};
		match chapters.last_mut() {
			Some(last) if short(&chapter) || short(last) => last.end_s = chapter.end_s,
			_ => chapters.push(chapter),
		}
	}
	chapters
}

pub fn write(path: &Path, chapters: &[Chapter]) -> Result<(), JobError> {
	write_work_file(path, ffmetadata(chapters))
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn ffmetadata(chapters: &[Chapter]) -> String {
	let milliseconds = |seconds: f64| (seconds * 1000.0).round() as u64;
	let mut text = String::from(";FFMETADATA1\n");
	for chapter in chapters {
		let _ = write!(
			text,
			"[CHAPTER]\nTIMEBASE=1/1000\nSTART={}\nEND={}\n",
			milliseconds(chapter.start_s),
			milliseconds(chapter.end_s)
		);
		if let Some(title) = &chapter.title {
			let _ = writeln!(text, "title={}", escaped(title));
		}
	}
	text
}

fn seconds(value: &Value) -> Option<f64> {
	value.as_f64().or_else(|| value.as_str()?.parse().ok())
}

fn escaped(text: &str) -> String {
	let mut escaped = String::with_capacity(text.len());
	for c in text.chars() {
		if matches!(c, '=' | ';' | '#' | '\\' | '\n') {
			escaped.push('\\');
		}
		escaped.push(c);
	}
	escaped
}

#[cfg(test)]
mod tests {
	use super::*;

	fn chapter(start: &str, end: &str, title: &str) -> Value {
		serde_json::json!({"start_time": start, "end_time": end, "tags": {"title": title}})
	}

	#[test]
	fn chapters_are_clipped_to_the_window_and_empty_ones_vanish() {
		let chapters = [
			chapter("0.000000", "4.000000", "A"),
			chapter("4.000000", "7.000000", "B"),
			chapter("7.000000", "10.000000", "C"),
		];

		let clipped = clip(
			&chapters,
			TrimWindow {
				start_s: 4.0,
				length_s: 4.0,
			},
		);

		assert_eq!(
			clipped,
			vec![
				Chapter {
					start_s: 0.0,
					end_s: 3.0,
					title: Some("B".into())
				},
				Chapter {
					start_s: 3.0,
					end_s: 4.0,
					title: Some("C".into())
				},
			]
		);
	}

	fn spans(chapters: &[Chapter]) -> Vec<(f64, f64, &str)> {
		chapters
			.iter()
			.map(|chapter| (chapter.start_s, chapter.end_s, chapter.title.as_deref().unwrap()))
			.collect()
	}

	fn named(starts: &[(f64, &str)]) -> Vec<(f64, String)> {
		starts
			.iter()
			.map(|(start_s, title)| (*start_s, (*title).to_owned()))
			.collect()
	}

	#[test]
	fn a_chapter_under_a_second_joins_the_one_before_it() {
		let chapters = from_starts(named(&[(0.0, "Title 1"), (3009.64, "Chapter 2")]), 3010.04);

		assert_eq!(spans(&chapters), [(0.0, 3010.04, "Title 1")]);

		let chapters = from_starts(
			named(&[(0.0, "A"), (10.0, "B"), (10.5, "C"), (10.9, "D"), (30.0, "E")]),
			40.0,
		);

		assert_eq!(
			spans(&chapters),
			[(0.0, 10.9, "A"), (10.9, 30.0, "D"), (30.0, 40.0, "E")]
		);
	}

	#[test]
	fn a_first_chapter_under_a_second_takes_in_the_next() {
		let chapters = from_starts(
			named(&[(0.0, "Title 1"), (0.4, "Chapter 2"), (20.0, "Chapter 3")]),
			30.0,
		);

		assert_eq!(spans(&chapters), [(0.0, 20.0, "Title 1"), (20.0, 30.0, "Chapter 3")]);
		assert_eq!(spans(&from_starts(named(&[(0.0, "Only")]), 0.5)), [(0.0, 0.5, "Only")]);
	}

	#[test]
	fn the_chapter_file_escapes_titles() {
		let text = ffmetadata(&[Chapter {
			start_s: 1.5,
			end_s: 3.0,
			title: Some("a=b; #c\\d\ne".into()),
		}]);

		assert_eq!(
			text,
			";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=1500\nEND=3000\ntitle=a\\=b\\; \\#c\\\\d\\\ne\n"
		);
	}
}
