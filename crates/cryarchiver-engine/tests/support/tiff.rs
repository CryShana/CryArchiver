#[derive(Clone)]
pub struct Entry {
	pub tag: u16,
	pub kind: u16,
	pub count: u32,
	pub value: Vec<u8>,
}

pub fn bytes16(big: bool, value: u16) -> [u8; 2] {
	if big { value.to_be_bytes() } else { value.to_le_bytes() }
}

pub fn bytes32(big: bool, value: u32) -> [u8; 4] {
	if big { value.to_be_bytes() } else { value.to_le_bytes() }
}

pub fn short(big: bool, tag: u16, value: u16) -> Entry {
	Entry {
		tag,
		kind: 3,
		count: 1,
		value: bytes16(big, value).to_vec(),
	}
}

pub fn long(big: bool, tag: u16, value: u32) -> Entry {
	Entry {
		tag,
		kind: 4,
		count: 1,
		value: bytes32(big, value).to_vec(),
	}
}

pub fn ascii(tag: u16, text: &str) -> Entry {
	Entry {
		tag,
		kind: 2,
		count: u32::try_from(text.len() + 1).unwrap(),
		value: [text.as_bytes(), b"\0"].concat(),
	}
}

pub fn undefined(tag: u16, data: &[u8]) -> Entry {
	Entry {
		tag,
		kind: 7,
		count: u32::try_from(data.len()).unwrap(),
		value: data.to_vec(),
	}
}

pub fn ifd(big: bool, at: usize, entries: &[Entry], next: u32) -> Vec<u8> {
	let mut sorted = entries.to_vec();
	sorted.sort_by_key(|entry| entry.tag);
	let data_start = at + 2 + sorted.len() * 12 + 4;
	let mut out = bytes16(big, u16::try_from(sorted.len()).unwrap()).to_vec();
	let mut data = Vec::new();

	for entry in &sorted {
		out.extend_from_slice(&bytes16(big, entry.tag));
		out.extend_from_slice(&bytes16(big, entry.kind));
		out.extend_from_slice(&bytes32(big, entry.count));
		if entry.value.len() <= 4 {
			let mut inline = entry.value.clone();
			inline.resize(4, 0);
			out.extend_from_slice(&inline);
		} else {
			out.extend_from_slice(&bytes32(big, u32::try_from(data_start + data.len()).unwrap()));
			data.extend_from_slice(&entry.value);
		}
	}
	out.extend_from_slice(&bytes32(big, next));
	out.extend(data);
	out
}

pub fn tiff(big: bool, ifd0: &[Entry], exif: &[Entry], interop: &[Entry]) -> Vec<u8> {
	let mut out = if big { b"MM\0*".to_vec() } else { b"II*\0".to_vec() };
	out.extend_from_slice(&bytes32(big, 8));

	let mut first = ifd0.to_vec();
	let mut exif = exif.to_vec();
	if !interop.is_empty() {
		exif.push(long(big, 0xA005, 0));
	}
	if !exif.is_empty() {
		first.push(long(big, 0x8769, 0));
	}

	let exif_at = 8 + ifd(big, 8, &first, 0).len();
	if let Some(pointer) = first.iter_mut().find(|entry| entry.tag == 0x8769) {
		pointer.value = bytes32(big, u32::try_from(exif_at).unwrap()).to_vec();
	}
	let interop_at = exif_at + ifd(big, exif_at, &exif, 0).len();
	if let Some(pointer) = exif.iter_mut().find(|entry| entry.tag == 0xA005) {
		pointer.value = bytes32(big, u32::try_from(interop_at).unwrap()).to_vec();
	}

	out.extend(ifd(big, 8, &first, 0));
	if !exif.is_empty() {
		out.extend(ifd(big, exif_at, &exif, 0));
	}
	if !interop.is_empty() {
		out.extend(ifd(big, interop_at, interop, 0));
	}
	out
}
