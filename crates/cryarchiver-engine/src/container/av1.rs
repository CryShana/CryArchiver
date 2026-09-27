const OBU_SEQUENCE_HEADER: u8 = 1;
const PRIMARIES_BT709: u8 = 1;
const TRANSFER_SRGB: u8 = 13;
const MATRIX_IDENTITY: u8 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequenceHeader {
	pub profile: u8,
	pub level: u8,
	pub width: u32,
	pub height: u32,
	pub bit_depth: u8,
	pub monochrome: bool,
	pub subsampling: (bool, bool),
	pub primaries: Option<u8>,
	pub transfer: Option<u8>,
	pub matrix: Option<u8>,
	pub full_range: bool,
	cicp_bit: Option<usize>,
}

struct Bits<'a> {
	data: &'a [u8],
	at: usize,
}

impl Bits<'_> {
	fn read(&mut self, count: usize) -> Option<u64> {
		let mut value = 0u64;
		for _ in 0..count {
			let byte = *self.data.get(self.at / 8)?;
			let bit = (byte >> (7 - self.at % 8)) & 1;
			value = (value << 1) | u64::from(bit);
			self.at += 1;
		}
		Some(value)
	}

	fn flag(&mut self) -> Option<bool> {
		self.read(1).map(|bit| bit == 1)
	}

	fn small(&mut self, count: usize) -> Option<u8> {
		self.read(count).and_then(|value| u8::try_from(value).ok())
	}

	fn skip(&mut self, count: usize) -> Option<()> {
		self.read(count).map(|_| ())
	}

	fn uvlc(&mut self) -> Option<()> {
		let mut leading_zeros = 0;
		while !self.flag()? {
			leading_zeros += 1;
			if leading_zeros >= 32 {
				return None;
			}
		}
		self.skip(leading_zeros)
	}
}

pub fn read_sequence_header(obus: &[u8]) -> Option<SequenceHeader> {
	let (start, end) = find_sequence_header(obus)?;
	let mut bits = Bits {
		data: &obus[..end],
		at: start * 8,
	};

	let profile = bits.small(3)?;
	bits.skip(1)?;
	let reduced = bits.flag()?;
	let level = if reduced {
		bits.small(5)?
	} else {
		operating_points(&mut bits)?
	};

	let width_bits = usize::from(bits.small(4)?) + 1;
	let height_bits = usize::from(bits.small(4)?) + 1;
	let width = u32::try_from(bits.read(width_bits)? + 1).ok()?;
	let height = u32::try_from(bits.read(height_bits)? + 1).ok()?;
	if !reduced && bits.flag()? {
		bits.skip(7)?;
	}
	bits.skip(3)?;
	if !reduced {
		skip_inter_tools(&mut bits)?;
	}
	bits.skip(3)?;

	color_config(&mut bits, profile, level, width, height)
}

fn operating_points(bits: &mut Bits<'_>) -> Option<u8> {
	let mut decoder_model = false;
	let mut buffer_delay_bits = 0;
	if bits.flag()? {
		bits.skip(64)?;
		if bits.flag()? {
			bits.uvlc()?;
		}
		decoder_model = bits.flag()?;
		if decoder_model {
			buffer_delay_bits = usize::from(bits.small(5)?) + 1;
			bits.skip(42)?;
		}
	}

	let initial_display_delay = bits.flag()?;
	let count = bits.small(5)? + 1;
	let mut first_level = 0;
	for point in 0..count {
		bits.skip(12)?;
		let level = bits.small(5)?;
		if point == 0 {
			first_level = level;
		}
		if level > 7 {
			bits.skip(1)?;
		}
		if decoder_model && bits.flag()? {
			bits.skip(buffer_delay_bits * 2 + 1)?;
		}
		if initial_display_delay && bits.flag()? {
			bits.skip(4)?;
		}
	}
	Some(first_level)
}

fn skip_inter_tools(bits: &mut Bits<'_>) -> Option<()> {
	bits.skip(4)?;
	let order_hint = bits.flag()?;
	if order_hint {
		bits.skip(2)?;
	}

	let screen_content_tools = if bits.flag()? { 2 } else { bits.read(1)? };
	if screen_content_tools > 0 && !bits.flag()? {
		bits.skip(1)?;
	}
	if order_hint {
		bits.skip(3)?;
	}
	Some(())
}

fn color_config(bits: &mut Bits<'_>, profile: u8, level: u8, width: u32, height: u32) -> Option<SequenceHeader> {
	let high_bitdepth = bits.flag()?;
	let bit_depth = if !high_bitdepth {
		8
	} else if profile == 2 && bits.flag()? {
		12
	} else {
		10
	};
	let monochrome = profile != 1 && bits.flag()?;

	let described = bits.flag()?;
	let cicp_bit = described.then_some(bits.at);
	let (primaries, transfer, matrix) = if described {
		(bits.small(8)?, bits.small(8)?, bits.small(8)?)
	} else {
		(2, 2, 2)
	};

	let (full_range, subsampling) = if monochrome {
		(bits.flag()?, (true, true))
	} else if primaries == PRIMARIES_BT709 && transfer == TRANSFER_SRGB && matrix == MATRIX_IDENTITY {
		(true, (false, false))
	} else {
		let full_range = bits.flag()?;
		let subsampling = match profile {
			0 => (true, true),
			1 => (false, false),
			_ if bit_depth == 12 => {
				let x = bits.flag()?;
				(x, x && bits.flag()?)
			}
			_ => (true, false),
		};
		(full_range, subsampling)
	};

	Some(SequenceHeader {
		profile,
		level,
		width,
		height,
		bit_depth,
		monochrome,
		subsampling,
		primaries: described.then_some(primaries),
		transfer: described.then_some(transfer),
		matrix: described.then_some(matrix),
		full_range,
		cicp_bit,
	})
}

fn find_sequence_header(obus: &[u8]) -> Option<(usize, usize)> {
	let mut at = 0;
	while at < obus.len() {
		let header = obus[at];
		let kind = (header >> 3) & 0x0F;
		let has_extension = header & 0x04 != 0;
		let has_size = header & 0x02 != 0;
		at += 1 + usize::from(has_extension);

		let (size, start) = if has_size {
			leb128(obus, at)?
		} else {
			(obus.len().checked_sub(at)?, at)
		};
		let end = start.checked_add(size)?;
		if kind == OBU_SEQUENCE_HEADER {
			return (end <= obus.len()).then_some((start, end));
		}
		at = end;
	}
	None
}

fn leb128(data: &[u8], mut at: usize) -> Option<(usize, usize)> {
	let mut value = 0u64;
	for group in 0..8 {
		let byte = *data.get(at)?;
		at += 1;
		value |= u64::from(byte & 0x7F) << (group * 7);
		if byte & 0x80 == 0 {
			return Some((usize::try_from(value).ok()?, at));
		}
	}
	None
}

pub fn patch_cicp(obus: &mut [u8], primaries: u8, transfer: u8) -> bool {
	let Some(header) = read_sequence_header(obus) else {
		return false;
	};

	let (Some(position), Some(old_primaries), Some(old_transfer)) =
		(header.cicp_bit, header.primaries, header.transfer)
	else {
		return false;
	};

	// primaries 1 + transfer 13 + matrix 0 omits the colour_range bit, so moving into or out of it would shift every
	// later bit.
	let omits_range = |primaries, transfer| {
		primaries == PRIMARIES_BT709 && transfer == TRANSFER_SRGB && header.matrix == Some(MATRIX_IDENTITY)
	};
	if omits_range(old_primaries, old_transfer) != omits_range(primaries, transfer) {
		return false;
	}

	let value = (u16::from(primaries) << 8) | u16::from(transfer);
	for offset in 0..16 {
		let bit_at = position + offset;
		let mask = 1u8 << (7 - bit_at % 8);
		if value & (1 << (15 - offset)) != 0 {
			obus[bit_at / 8] |= mask;
		} else {
			obus[bit_at / 8] &= !mask;
		}
	}
	true
}
