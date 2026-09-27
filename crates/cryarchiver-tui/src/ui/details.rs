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

	for stage in &details.stages {
		let (symbol, stage_color) = match stage.progress {
			StageProgress::Done => ("✓", theme.done),
			StageProgress::Current => ("●", color),
			StageProgress::Pending => ("○", theme.border),
		};
		spans.push(Span::styled(symbol, fg(stage_color)));
	}

	if let Some(current) = details
		.stages
		.iter()
		.find(|stage| stage.progress == StageProgress::Current)
	{
		spans.push(Span::styled(format!(" {}", current.name), fg(color)));
	}
	Line::from(spans)
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
