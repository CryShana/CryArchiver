use ratatui::style::{Color, Style};

use crate::view::{FileState, IssueKind, LogMarker};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorDepth {
	TrueColor,
	Xterm256,
}

impl ColorDepth {
	pub fn detect() -> Self {
		let colorterm = std::env::var("COLORTERM").ok();
		let windows_terminal = std::env::var_os("WT_SESSION").is_some();
		Self::from_env(colorterm.as_deref(), windows_terminal)
	}

	pub fn from_env(colorterm: Option<&str>, windows_terminal: bool) -> Self {
		let truecolor = colorterm.is_some_and(|value| {
			let value = value.to_ascii_lowercase();
			value == "truecolor" || value == "24bit"
		});

		if truecolor || windows_terminal {
			Self::TrueColor
		} else {
			Self::Xterm256
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
	pub background: Color,
	pub panel: Color,
	pub border: Color,
	pub selection: Color,
	pub text: Color,
	pub dim: Color,
	pub bright: Color,
	pub accent: Color,
	pub done: Color,
	pub warn: Color,
	pub error: Color,
	pub search: Color,
	pub verify: Color,
	pub danger: Color,
}

impl Theme {
	pub fn dark(depth: ColorDepth) -> Self {
		let color = |r: u8, g: u8, b: u8| match depth {
			ColorDepth::TrueColor => Color::Rgb(r, g, b),
			ColorDepth::Xterm256 => Color::Indexed(nearest_xterm(r, g, b)),
		};

		Self {
			background: color(0x0B, 0x0F, 0x13),
			panel: color(0x12, 0x19, 0x20),
			border: color(0x2B, 0x36, 0x42),
			selection: color(0x1B, 0x2B, 0x3B),
			text: color(0xC8, 0xD1, 0xDA),
			dim: color(0x66, 0x72, 0x7D),
			bright: color(0xF0, 0xF4, 0xF7),
			accent: color(0x3C, 0xC4, 0xCE),
			done: color(0x46, 0xB8, 0x66),
			warn: color(0xD9, 0xA4, 0x3C),
			error: color(0xEF, 0x5B, 0x53),
			search: color(0xB9, 0x8C, 0xF0),
			verify: color(0x5C, 0x9F, 0xF0),
			danger: color(0x4A, 0x16, 0x19),
		}
	}

	pub fn base(&self) -> Style {
		Style::new().fg(self.text).bg(self.background)
	}

	pub fn state(&self, state: FileState) -> Color {
		match state {
			FileState::Done => self.done,
			FileState::Encoding => self.accent,
			FileState::QualitySearch | FileState::Duplicate => self.search,
			FileState::Verifying | FileState::OriginalDeleted => self.verify,
			FileState::Queued | FileState::Copied | FileState::SourceGone | FileState::Excluded => self.dim,
			FileState::KeptOriginal | FileState::Waiting | FileState::Warning | FileState::DeletePending => self.warn,
			FileState::SourceDamaged | FileState::Failed => self.error,
		}
	}

	pub fn issue(&self, kind: IssueKind) -> Color {
		match kind {
			IssueKind::SourceDamaged | IssueKind::EncodeFailed | IssueKind::SourceUnreadable => self.error,
			IssueKind::OutputMissing | IssueKind::UnknownFile | IssueKind::DirectoryCycle => self.warn,
		}
	}

	pub fn marker(&self, marker: LogMarker) -> Color {
		match marker {
			LogMarker::State(state) => self.state(state),
			LogMarker::Action => self.accent,
		}
	}
}

pub const fn glyph(state: FileState) -> &'static str {
	match state {
		FileState::SourceGone => "⌀",
		FileState::DeletePending => "⊖",
		FileState::OriginalDeleted => "⊗",
		FileState::Excluded => "−",
		FileState::Done => "✓",
		FileState::Encoding => "●",
		FileState::QualitySearch => "◌",
		FileState::Verifying => "◑",
		FileState::Queued => "·",
		FileState::KeptOriginal => "=",
		FileState::Copied => "→",
		FileState::Duplicate => "↪",
		FileState::Waiting => "◷",
		FileState::SourceDamaged => "!",
		FileState::Failed => "✗",
		FileState::Warning => "⚠",
	}
}

pub const fn issue_glyph(kind: IssueKind) -> &'static str {
	match kind {
		IssueKind::SourceDamaged => "!",
		IssueKind::EncodeFailed => "✗",
		IssueKind::SourceUnreadable => "⊘",
		IssueKind::OutputMissing => "⚠",
		IssueKind::UnknownFile => "?",
		IssueKind::DirectoryCycle => "↻",
	}
}

pub const fn marker_glyph(marker: LogMarker) -> &'static str {
	match marker {
		LogMarker::State(state) => glyph(state),
		LogMarker::Action => "→",
	}
}

pub const fn label(state: FileState) -> &'static str {
	match state {
		FileState::SourceGone => "gone",
		FileState::DeletePending => "deleting",
		FileState::OriginalDeleted => "deleted",
		FileState::Excluded => "excluded",
		FileState::Done => "done",
		FileState::Encoding => "encode",
		FileState::QualitySearch => "search",
		FileState::Verifying => "verify",
		FileState::Queued => "queued",
		FileState::KeptOriginal => "kept",
		FileState::Copied => "copied",
		FileState::Duplicate => "duplicate",
		FileState::Waiting => "waiting",
		FileState::SourceDamaged => "damaged",
		FileState::Failed => "failed",
		FileState::Warning => "warning",
	}
}

fn nearest_xterm(r: u8, g: u8, b: u8) -> u8 {
	const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];

	let nearest_level = |channel: u8| {
		(0u8..6)
			.min_by_key(|&i| channel.abs_diff(CUBE[usize::from(i)]))
			.expect("cube has levels")
	};
	let distance = |(cr, cg, cb): (u8, u8, u8)| {
		let d = |a: u8, b: u8| u32::from(a.abs_diff(b)).pow(2);
		d(r, cr) + d(g, cg) + d(b, cb)
	};

	let (ri, gi, bi) = (nearest_level(r), nearest_level(g), nearest_level(b));
	let cube_index = 16 + 36 * ri + 6 * gi + bi;
	let cube_distance = distance((CUBE[usize::from(ri)], CUBE[usize::from(gi)], CUBE[usize::from(bi)]));

	let (gray_index, gray_distance) = (0u8..24)
		.map(|i| {
			let level = 8 + 10 * i;
			(232 + i, distance((level, level, level)))
		})
		.min_by_key(|&(_, d)| d)
		.expect("ramp has levels");

	if gray_distance < cube_distance {
		gray_index
	} else {
		cube_index
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn truecolor_needs_colorterm_or_windows_terminal() {
		assert_eq!(ColorDepth::from_env(Some("truecolor"), false), ColorDepth::TrueColor);
		assert_eq!(ColorDepth::from_env(Some("24bit"), false), ColorDepth::TrueColor);
		assert_eq!(ColorDepth::from_env(None, true), ColorDepth::TrueColor);
		assert_eq!(ColorDepth::from_env(Some("256"), false), ColorDepth::Xterm256);
		assert_eq!(ColorDepth::from_env(None, false), ColorDepth::Xterm256);
	}

	#[test]
	fn palette_maps_to_nearby_xterm_colours() {
		assert_eq!(nearest_xterm(0x00, 0x00, 0x00), 16);
		assert_eq!(nearest_xterm(0xFF, 0xFF, 0xFF), 231);
		assert_eq!(nearest_xterm(0x0B, 0x0F, 0x13), 233);
		assert_eq!(nearest_xterm(0x3C, 0xC4, 0xCE), 80);
		assert_eq!(nearest_xterm(0xEF, 0x5B, 0x53), 203);
	}

	#[test]
	fn every_state_has_a_distinct_glyph() {
		let states = [
			FileState::Done,
			FileState::Encoding,
			FileState::QualitySearch,
			FileState::Verifying,
			FileState::Queued,
			FileState::KeptOriginal,
			FileState::Copied,
			FileState::Duplicate,
			FileState::Waiting,
			FileState::SourceDamaged,
			FileState::Failed,
			FileState::Warning,
			FileState::SourceGone,
			FileState::DeletePending,
			FileState::OriginalDeleted,
			FileState::Excluded,
		];
		let glyphs: std::collections::HashSet<_> = states.iter().map(|&state| glyph(state)).collect();

		assert_eq!(glyphs.len(), states.len());
	}
}
