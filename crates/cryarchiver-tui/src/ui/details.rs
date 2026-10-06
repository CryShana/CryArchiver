use std::ffi::OsStr;

use cryarchiver_engine::text::display_name;
use cryarchiver_project::view::WARNING_LABEL;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::widgets::{Chrome, bold, fg, gauge, panel, put, put_line, to_u16, verb_spans};
use crate::app::{Hit, HitMap};
use crate::format::{bytes, duration, fit, fit_middle, fps, percent, saving, speed, width};
use crate::theme::{Theme, glyph, issue_glyph, label};
use crate::view::{Duplicates, FileDetails, IssueSummary, PathLink, StageProgress};

const LABEL: usize = 9;
const WARNINGS_SHOWN: usize = 3;

pub fn render(
	buf: &mut Buffer,
	area: Rect,
	name: &str,
	details: Option<&FileDetails>,
	theme: &Theme,
	hits: &mut HitMap,
) {
	let chrome = Chrome {
		title: name,
		right: Some(" ⏎ back · ↑↓ prev/next "),
		focused: true,
	};
	let inner = panel(buf, area, theme, &chrome);

	let Some(details) = details else {
		put(
			buf,
			inner.x + 1,
			inner.y,
			"Select a file to see its details",
			fg(theme.dim),
			inner.right(),
		);
		return;
	};

	let [facts, side] = Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
		.spacing(2)
		.areas(Rect {
			x: inner.x + 1,
			width: inner.width.saturating_sub(2),
			..inner
		});

	render_facts(buf, facts, details, theme);
	render_side(buf, side, details, theme, hits);
}

fn render_facts(buf: &mut Buffer, area: Rect, details: &FileDetails, theme: &Theme) {
	let value_width = usize::from(area.width).saturating_sub(LABEL);
	let state_color = theme.state(details.state);
	let mut lines = vec![
		labelled(
			"Source",
			&fit_middle(&details.source_path, value_width),
			fg(theme.text),
			theme,
		),
		labelled(
			"Output",
			&fit_middle(&details.destination_path, value_width),
			fg(theme.text),
			theme,
		),
		labelled("", &fit(&details.source_info, value_width), fg(theme.dim), theme),
		state_line(details, theme),
	];

	if let Some(live) = &details.live {
		let cells = value_width.saturating_sub(7).max(6);
		let mut spans = vec![Span::raw(" ".repeat(LABEL))];
		spans.extend(gauge(cells, live.fraction, state_color, theme));
		spans.push(Span::styled(
			format!(" {}", percent(live.fraction, false)),
			fg(theme.text),
		));
		lines.push(Line::from(spans));

		let mut facts = Vec::new();
		facts.extend(live.fps.map(fps));
		facts.extend(live.speed.map(speed));
		facts.extend(live.eta.map(|eta| format!("ETA {}", duration(eta))));
		if !facts.is_empty() {
			lines.push(labelled("", &facts.join(" · "), fg(theme.dim), theme));
		}
	}

	lines.push(size_line(details, theme));
	for plan in &details.plan {
		let color = if plan.label == "Note" { state_color } else { theme.text };
		lines.push(labelled(&plan.label, &fit(&plan.value, value_width), fg(color), theme));
	}

	lines.extend(record_lines(details, value_width, theme));

	if let Some(quality) = &details.quality {
		let color = if quality.starts_with("target") {
			theme.dim
		} else {
			theme.done
		};
		lines.push(labelled("Quality", &fit(quality, value_width), fg(color), theme));
	}

	draw_lines(buf, area, &lines);
}

/// The decision record, leaving out the warnings the notes already show and all but the first few of the rest.
fn record_lines<'a>(details: &FileDetails, value_width: usize, theme: &Theme) -> Vec<Line<'a>> {
	let noted = |value: &str| details.notes.iter().any(|note| display_name(OsStr::new(note)) == value);
	let mut lines = Vec::new();
	let mut warnings = 0;

	for decision in &details.decision {
		if decision.label == WARNING_LABEL {
			if noted(&decision.value) {
				continue;
			}

			warnings += 1;
			if warnings > WARNINGS_SHOWN {
				continue;
			}
		}

		let color = if decision.marked { theme.warn } else { theme.text };
		lines.push(labelled(
			&decision.label,
			&fit(&decision.value, value_width),
			fg(color),
			theme,
		));
	}

	if warnings > WARNINGS_SHOWN {
		let more = format!("… {} more", warnings - WARNINGS_SHOWN);
		lines.push(labelled("", &more, fg(theme.dim), theme));
	}

	lines
}

fn labelled<'a>(label: &str, value: &str, style: Style, theme: &Theme) -> Line<'a> {
	Line::from(vec![
		Span::styled(fit(label, LABEL), fg(theme.dim)),
		Span::styled(value.trim_end().to_owned(), style),
	])
}

fn state_line<'a>(details: &FileDetails, theme: &Theme) -> Line<'a> {
	let color = theme.state(details.state);
	let mut spans = vec![
		Span::styled(fit("State", LABEL), fg(theme.dim)),
		Span::styled(format!("{} ", glyph(details.state)), bold(color)),
		Span::styled(format!("{:<9}", label(details.state)), fg(color)),
	];

	let restated = matches!(details.stages.as_slice(), [only] if restates(&only.name, label(details.state)));
	let stages = if restated { &[][..] } else { details.stages.as_slice() };

	for stage in stages {
		let (symbol, stage_color) = match stage.progress {
			StageProgress::Done => ("✓", theme.done),
			StageProgress::Current => ("●", color),
			StageProgress::Pending => ("○", theme.border),
		};
		spans.push(Span::styled(symbol, fg(stage_color)));
	}

	if let Some(current) = stages.iter().find(|stage| stage.progress == StageProgress::Current) {
		spans.push(Span::styled(format!(" {}", current.name), fg(color)));
	}
	Line::from(spans)
}

/// A lone stage such as "encoding" next to the state "encode" says nothing more.
fn restates(stage: &str, label: &str) -> bool {
	let shared = stage.chars().zip(label.chars()).take_while(|(a, b)| a == b).count();
	shared + 1 >= label.chars().count()
}

fn size_line<'a>(details: &FileDetails, theme: &Theme) -> Line<'a> {
	let (text, color) = match (details.bytes_out, details.estimated_out) {
		(Some(out), _) => (
			format!(
				"{} → {} ({})",
				bytes(details.bytes_in),
				bytes(out),
				saving(details.bytes_in, out).replace('%', " %")
			),
			theme.done,
		),
		(None, Some(estimate)) => (
			format!("{} → est. {}", bytes(details.bytes_in), bytes(estimate)),
			theme.text,
		),
		(None, None) => (format!("{} → …", bytes(details.bytes_in)), theme.text),
	};
	labelled("Size", &text, fg(color), theme)
}

fn render_side(buf: &mut Buffer, area: Rect, details: &FileDetails, theme: &Theme, hits: &mut HitMap) {
	let mut y = area.y;
	let bottom = area.bottom();
	let right = area.right();

	for (heading, lines) in [("ffmpeg (live tail)", &details.log_tail), ("Notes", &details.notes)] {
		if lines.is_empty() {
			continue;
		}

		put(buf, area.x, y, heading, bold(theme.dim), right);
		y += 1;
		for line in lines {
			if y >= bottom {
				return;
			}
			let color = if line.starts_with('[') { theme.warn } else { theme.text };
			put(buf, area.x, y, &fit(line, usize::from(area.width)), fg(color), right);
			y += 1;
		}
		y += 1;
	}

	y = render_duplicates(buf, Rect { y, ..area }, &details.duplicates, theme, hits);
	if let Some(issue) = &details.issue {
		render_issue(buf, Rect { y: y + 1, ..area }, issue, theme);
	}
}

fn render_duplicates(buf: &mut Buffer, area: Rect, duplicates: &Duplicates, theme: &Theme, hits: &mut HitMap) -> u16 {
	let (links, heading): (Vec<&PathLink>, String) = match duplicates {
		Duplicates::None => return area.y.saturating_sub(1),
		Duplicates::PlaceholderOf(link) => (vec![link], "Duplicate of".to_owned()),
		Duplicates::CanonicalOf(links) => (links.iter().collect(), format!("Duplicates ({})", links.len())),
	};

	let mut y = area.y;
	if y >= area.bottom() {
		return y;
	}
	put(buf, area.x, y, &heading, bold(theme.dim), area.right());
	put(
		buf,
		area.x + to_u16(width(&heading)) + 2,
		y,
		"g jump",
		fg(theme.dim),
		area.right(),
	);
	y += 1;

	let link_style = fg(theme.accent).add_modifier(Modifier::UNDERLINED);
	for link in links {
		if y >= area.bottom() {
			break;
		}
		let text = fit_middle(&link.path, usize::from(area.width).saturating_sub(2));
		let line = Line::from(vec![
			Span::styled("↪ ", fg(theme.search)),
			Span::styled(text.trim_end().to_owned(), link_style),
		]);
		let end = put_line(buf, area.x, y, &line, area.right());
		hits.push(
			Rect::new(area.x, y, end - area.x, 1),
			Hit::Link {
				file: link.file,
				folder: link.folder,
			},
		);
		y += 1;
	}
	y
}

fn render_issue(buf: &mut Buffer, area: Rect, issue: &IssueSummary, theme: &Theme) {
	let color = theme.issue(issue.kind);
	let lines = vec![
		Line::styled("Issue", bold(theme.dim)),
		Line::from(vec![
			Span::styled(format!("{} ", issue_glyph(issue.kind)), bold(color)),
			Span::styled(issue.reason.clone(), fg(theme.text)),
		]),
		Line::styled(issue.resolution.clone(), fg(theme.dim)),
		Line::from(verb_spans(&issue.verbs, theme)),
	];
	draw_lines(buf, area, &lines);
}

fn draw_lines(buf: &mut Buffer, area: Rect, lines: &[Line]) {
	for (row, line) in (area.y..area.bottom()).zip(lines) {
		put_line(buf, area.x, row, line, area.right());
	}
}

#[cfg(test)]
mod tests {
	use cryarchiver_engine::decision::{CrfChoice, Decision, Measured, Reencode, Scan, StageTime, VideoDecision, Vmaf};
	use cryarchiver_project::view::decision_lines;

	use super::*;
	use crate::theme::ColorDepth;
	use crate::view::{DecisionLine, FileState, PlanLine, StageMark};

	fn reencoded() -> Decision {
		Decision {
			video: Some(VideoDecision {
				encoder: "SVT-AV1 preset 5".into(),
				crf: Some(36.75),
				chosen: Some(CrfChoice::Searched),
				target: Some(Vmaf { mean: 91.0, p5: 86.5 }),
				grain_allowance: Some(Vmaf { mean: 2.0, p5: 2.5 }),
				probes: vec![Measured::default(); 5],
				checks: vec![
					Measured {
						crf: 38.0,
						mean: 90.4,
						p5: 85.9,
						..Measured::default()
					},
					Measured {
						crf: 36.75,
						mean: 93.1,
						p5: 87.2,
						..Measured::default()
					},
				],
				reencode: Some(Reencode {
					reason: "the full check measured VMAF 90.4 / 85.9, below the target 91 / 86.5".into(),
					from_crf: 38.0,
					to_crf: 36.75,
				}),
				grain: Some(6),
				scan: Some(Scan::Interlaced),
				deinterlaced: true,
				..VideoDecision::default()
			}),
			source_bytes: Some(1_750_000_000),
			output_bytes: Some(152_000_000),
			stages: vec![
				StageTime {
					stage: "analyse".into(),
					seconds: 41.0,
				},
				StageTime {
					stage: "sample".into(),
					seconds: 312.0,
				},
				StageTime {
					stage: "encode".into(),
					seconds: 2410.0,
				},
				StageTime {
					stage: "verify".into(),
					seconds: 380.0,
				},
			],
			attempts: 2,
			..Decision::default()
		}
	}

	fn text(buffer: &Buffer) -> String {
		let area = buffer.area;
		(area.top()..area.bottom())
			.map(|y| {
				let row: String = (area.left()..area.right()).map(|x| buffer[(x, y)].symbol()).collect();
				row.trim_end().to_owned()
			})
			.collect::<Vec<_>>()
			.join("\n")
	}

	fn shown(decision: &Decision) -> Vec<DecisionLine> {
		decision_lines(decision)
			.into_iter()
			.map(|line| DecisionLine {
				label: line.label,
				value: line.value,
				marked: line.marked,
			})
			.collect()
	}

	fn drawn(details: &FileDetails) -> Buffer {
		let area = Rect::new(0, 0, 120, 16);
		let mut buffer = Buffer::empty(area);
		render(
			&mut buffer,
			area,
			"concert.mkv",
			Some(details),
			&Theme::dark(ColorDepth::TrueColor),
			&mut HitMap::default(),
		);
		buffer
	}

	#[test]
	fn a_lone_stage_is_left_out_when_it_only_restates_the_state() {
		let stage = |name: &str| StageMark {
			name: name.into(),
			progress: StageProgress::Current,
		};
		let working = |name: &str| FileDetails {
			state: FileState::Encoding,
			stages: vec![stage(name)],
			..finished(Vec::new())
		};

		let same = text(&drawn(&working("encoding")));
		let finer = text(&drawn(&working("muxing")));

		assert!(
			same.contains("encode") && !same.contains("encoding") && same.matches('●').count() == 1,
			"{same}"
		);
		assert!(finer.contains("● muxing"), "{finer}");
	}

	#[test]
	fn warnings_the_notes_show_are_left_out_and_only_the_first_few_of_the_rest_shown() {
		let decision = Decision {
			warnings: ["seen live", "first", "second", "third", "fourth", "fifth"]
				.map(str::to_owned)
				.into(),
			..Decision::default()
		};
		let details = FileDetails {
			decision: shown(&decision),
			notes: vec!["seen live".into()],
			..finished(Vec::new())
		};

		let text = text(&drawn(&details));

		assert!(!text.contains("Warning  seen live"), "{text}");
		for warning in ["first", "second", "third"] {
			assert!(text.contains(&format!("Warning  {warning}")), "{text}");
		}
		assert!(!text.contains("fourth"), "{text}");
		assert!(text.contains("         … 2 more"), "{text}");
	}

	fn finished(decision: Vec<DecisionLine>) -> FileDetails {
		FileDetails {
			source_path: r"D:\Videos\concert.mkv".into(),
			destination_path: r"E:\Archive\concert.mkv".into(),
			source_info: "1.75 GB".into(),
			state: FileState::Done,
			stages: Vec::new(),
			live: None,
			bytes_in: 1_750_000_000,
			bytes_out: Some(152_000_000),
			estimated_out: None,
			plan: vec![PlanLine {
				label: "Kind".into(),
				value: "media".into(),
			}],
			decision,
			quality: None,
			log_tail: Vec::new(),
			notes: Vec::new(),
			duplicates: Duplicates::None,
			issue: None,
		}
	}

	#[test]
	fn a_finished_video_shows_its_decision_record() {
		let buffer = drawn(&finished(shown(&reencoded())));

		let again = (0..buffer.area.height)
			.find(|&y| {
				text(&buffer)
					.lines()
					.nth(usize::from(y))
					.is_some_and(|row| row.contains("Again"))
			})
			.unwrap();
		assert_eq!(
			buffer[(11, again)].fg,
			Theme::dark(ColorDepth::TrueColor).warn,
			"the encode again stands out"
		);
		insta::assert_snapshot!(text(&buffer));
	}
}
