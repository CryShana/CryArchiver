use crate::metadata::SourceMetadata;

const EXIF_PREFIX: &[u8] = b"Exif\0\0";
const FIRST_CHUNK: usize = 12;

pub(super) fn extract(bytes: &[u8]) -> SourceMetadata {
	let mut metadata = SourceMetadata::default();
	let mut at = FIRST_CHUNK;

	while at + 8 <= bytes.len() {
		let kind = &bytes[at..at + 4];
		let len = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().expect("4 bytes"));
		let Some(body) = usize::try_from(len)
			.ok()
			.and_then(|len| bytes.get(at + 8..(at + 8).checked_add(len)?))
		else {
			metadata
				.warnings
				.push(format!("the {} chunk is cut off", String::from_utf8_lossy(kind)));
			break;
		};

		match kind {
			b"EXIF" => metadata.exif = Some(body.strip_prefix(EXIF_PREFIX).unwrap_or(body).to_vec()),
			b"XMP " => metadata.xmp = Some(body.to_vec()),
			b"ICCP" => metadata.icc = Some(body.to_vec()),
			_ => {}
		}
		at += 8 + body.len() + body.len() % 2;
	}
	metadata
}
