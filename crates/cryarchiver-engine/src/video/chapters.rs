use std::fmt::Write;

use serde_json::Value;

use crate::options::TrimWindow;

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
