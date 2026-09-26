use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use super::widgets::{Chrome, bold, fg, panel, put, to_u16, verb_hint};
use crate::app::{Hit, Ui};
use crate::format::{fit, fit_middle, width};
use crate::theme::{Theme, issue_glyph};
use crate::view::Issue;

const ROWS_PER_ISSUE: u16 = 3;

pub fn render(buf: &mut Buffer, area: Rect, issues: &[Issue], ui: &mut Ui) {
	let theme = ui.theme;
	ui.issue_selected = ui.issue_selected.min(issues.len().saturating_sub(1));

	let list_width = to_u16(usize::from(area.width) * 46 / 100).max(34).min(area.width);
	let list_area = Rect {
		width: list_width,
		..area
	};
	let log_area = Rect {
		x: area.x + list_width,
		width: area.width - list_width,
		..area
	};

	let count = format!(" {} ", issues.len());
	let chrome = Chrome {
		title: "Issues",
		right: Some(&count),
		focused: true,
	};
	let list = panel(buf, list_area, &theme, &chrome);
	ui.issues_area = Some(list);

	if issues.is_empty() {
		put(
			buf,
			list.x + 1,
			list.y,
			"✓ No open issues",
			fg(theme.done),
			list.right(),
		);
	} else {
		render_list(buf, list, issues, ui);
	}

	let attempt_back = ui.attempt_back;
	render_log(buf, log_area, issues.get(ui.issue_selected), attempt_back, &theme);
}

fn render_list(buf: &mut Buffer, area: Rect, issues: &[Issue], ui: &mut Ui) {
	let visible = usize::from((area.height / ROWS_PER_ISSUE).max(1));
	let first = ui.issue_selected.saturating_sub(visible - 1);

	for (slot, (index, issue)) in issues.iter().enumerate().skip(first).take(visible).enumerate() {
		let y = area.y + to_u16(slot) * ROWS_PER_ISSUE;
		let row_area = Rect::new(area.x, y, area.width, ROWS_PER_ISSUE.min(area.bottom() - y));
		render_issue(buf, row_area, issue, index == ui.issue_selected, &ui.theme);
		ui.hits.push(row_area, Hit::Issue(index));
	}
}

fn render_issue(buf: &mut Buffer, area: Rect, issue: &Issue, selected: bool, theme: &Theme) {
	let background = if selected { theme.selection } else { theme.background };
	buf.set_style(area, Style::new().bg(background));

	let (x, right) = (area.x + 1, area.right().saturating_sub(1));
	let cells = usize::from(right.saturating_sub(x + 2));

	put(
		buf,
		x,
		area.y,
		issue_glyph(issue.kind),
		bold(theme.issue(issue.kind)),
		right,
	);
	let name_width = cells.saturating_sub(width(&issue.location) + 1);
	put(
		buf,
		x + 2,
		area.y,
		&fit_middle(&issue.name, name_width),
		bold(theme.bright),
		right,
	);
	let location_x = right.saturating_sub(to_u16(width(&issue.location)));
	put(buf, location_x, area.y, &issue.location, fg(theme.dim), right);

	if area.height > 1 {
		put(
			buf,
			x + 2,
			area.y + 1,
			&fit(&issue.reason, cells),
			fg(theme.text),
			right,
		);
	}
	if area.height > 2 {
		put(
			buf,
			x + 2,
			area.y + 2,
			&fit(&issue.resolution, cells),
			fg(theme.dim),
			right,
		);
	}
}

/// Shows one attempt's log, the latest unless the user stepped back with ←.
fn render_log(buf: &mut Buffer, area: Rect, issue: Option<&Issue>, attempt_back: usize, theme: &Theme) {
	let attempts = issue.map_or(0, |issue| issue.attempts.len());
	let shown = attempts.checked_sub(1 + attempt_back.min(attempts.saturating_sub(1)));
	let title = match (shown, issue) {
		(Some(index), Some(issue)) if attempts > 1 => {
			format!("ffmpeg log · attempt {} of {attempts}", issue.attempts[index].attempt)
		}
		_ => "ffmpeg log".to_owned(),
	};
	let hint = issue.map(|issue| verb_hint(&issue.verbs));
	let chrome = Chrome {
		title: &title,
		right: hint.as_deref(),
		focused: false,
	};
	let inner = panel(buf, area, theme, &chrome);
	let Some(issue) = issue else { return };

	let (x, right) = (inner.x + 1, inner.right().saturating_sub(1));
	let cells = usize::from(right.saturating_sub(x));
	put(
		buf,
		x,
		inner.y,
		&fit(&format!("$ cryarchiver: {}", issue.name), cells),
		fg(theme.dim),
		right,
	);

	let Some(index) = shown else {
		put(
			buf,
			x,
			inner.y + 2,
			"no log for this kind of issue",
			fg(theme.dim),
			right,
		);
		return;
	};

	for (row, line) in (inner.y + 2..inner.bottom()).zip(&issue.attempts[index].lines) {
		let color = if line.starts_with('[') { theme.warn } else { theme.text };
		put(buf, x, row, &fit(line, cells), fg(color), right);
	}

	if attempts > 1 && inner.height > 1 {
		let hint = "← → other attempts";
		let hint_x = right.saturating_sub(to_u16(width(hint)));
		put(buf, hint_x, inner.bottom() - 1, hint, fg(theme.dim), right);
	}
}
