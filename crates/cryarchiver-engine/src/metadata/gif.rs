use crate::metadata::SourceMetadata;

const HEADER_LEN: usize = 13;
const EXTENSION: u8 = 0x21;
const COMMENT_LABEL: u8 = 0xFE;
const IMAGE: u8 = 0x2C;

pub(super) fn extract(bytes: &[u8]) -> SourceMetadata {
	let mut metadata = SourceMetadata::default();
	let Some(&flags) = bytes.get(10) else {
		return metadata;
	};
	let mut at = HEADER_LEN + colour_table_len(flags);

	loop {
		match bytes.get(at) {
			Some(&EXTENSION) => {
				let label = bytes.get(at + 1).copied();
				let Some((data, next)) = sub_blocks(bytes, at + 2) else {
					break;
				};
				if label == Some(COMMENT_LABEL) {
					metadata.comments.push(String::from_utf8_lossy(&data).into_owned());
				}
				at = next;
			}
			Some(&IMAGE) => {
				let Some(&local_flags) = bytes.get(at + 9) else {
					break;
				};
				let Some((_, next)) = sub_blocks(bytes, at + 10 + colour_table_len(local_flags) + 1) else {
					break;
				};
				at = next;
			}
			_ => break,
		}
	}
	metadata
}

fn colour_table_len(flags: u8) -> usize {
	if flags & 0x80 == 0 {
		0
	} else {
		3 << ((flags & 0x07) + 1)
	}
}

fn sub_blocks(bytes: &[u8], mut at: usize) -> Option<(Vec<u8>, usize)> {
	let mut data = Vec::new();
	loop {
		let len = usize::from(*bytes.get(at)?);
		if len == 0 {
			return Some((data, at + 1));
		}
		data.extend_from_slice(bytes.get(at + 1..at + 1 + len)?);
		at += 1 + len;
	}
}
