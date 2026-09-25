use std::collections::VecDeque;

#[derive(Default)]
pub struct LineSplitter {
	pending: Vec<u8>,
}

impl LineSplitter {
	pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
		let mut lines = Vec::new();
		for &byte in bytes {
			if byte == b'\n' || byte == b'\r' {
				if let Some(line) = self.finish() {
					lines.push(line);
				}
			} else {
				self.pending.push(byte);
			}
		}
		lines
	}

	pub fn finish(&mut self) -> Option<String> {
		if self.pending.is_empty() {
			return None;
		}

		let line = String::from_utf8_lossy(&self.pending).into_owned();
		self.pending.clear();
		Some(line)
	}
}

pub struct RingBuffer {
	lines: VecDeque<String>,
	capacity: usize,
}

impl RingBuffer {
	pub fn new(capacity: usize) -> Self {
		Self {
			lines: VecDeque::with_capacity(capacity),
			capacity,
		}
	}

	pub fn push(&mut self, line: String) {
		if self.lines.len() == self.capacity {
			self.lines.pop_front();
		}
		self.lines.push_back(line);
	}

	pub fn into_vec(self) -> Vec<String> {
		self.lines.into()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn splits_on_carriage_return_and_newline_across_chunks() {
		let mut splitter = LineSplitter::default();

		assert_eq!(splitter.push(b"frame=1\rfra"), vec!["frame=1"]);
		assert_eq!(splitter.push(b"me=2\r\nlast"), vec!["frame=2"]);
		assert_eq!(splitter.finish(), Some("last".to_owned()));
		assert_eq!(splitter.finish(), None);
	}

	#[test]
	fn ring_buffer_keeps_the_newest_lines() {
		let mut ring = RingBuffer::new(2);
		for line in ["a", "b", "c"] {
			ring.push(line.to_owned());
		}

		assert_eq!(ring.into_vec(), vec!["b", "c"]);
	}
}
