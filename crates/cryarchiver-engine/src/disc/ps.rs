//! The MPEG program stream packs of a DVD's VOBs, read sector by sector.

use super::array;

const PACK_START: [u8; 4] = [0, 0, 1, 0xBA];
const PRIVATE_1: u8 = 0xBD;
const NAV: [u8; 4] = [0, 0, 1, 0xBF];
const NAV_AT: usize = 38;
const DSI_AT: usize = 1024;
const DSI_SUBSTREAM: u8 = 1;
/// Where the DSI's own data starts in a nav pack.
const DSI_DATA: usize = 1031;

/// A PES packet of a pack: its stream as ffmpeg numbers it (a private stream by its substream byte, others by
/// `0x100` plus their start code), and whether its payload is scrambled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet {
	pub id: u32,
	pub scrambled: bool,
}

/// The audio, video and private-stream packets of the pack `sector`; none when it isn't an MPEG-2 pack.
pub fn packets(sector: &[u8]) -> Vec<Packet> {
	if sector.get(..4) != Some(&PACK_START) || sector.get(4).is_none_or(|marker| marker >> 6 != 1) {
		return Vec::new();
	}

	let mut packets = Vec::new();
	let mut at = 14 + usize::from(sector.get(13).map_or(0, |stuffing| stuffing & 7));
	while let Some(&[0, 0, 1, code, high, low]) = sector.get(at..at + 6) {
		let body = at + 6;
		at = body + usize::from(u16::from_be_bytes([high, low]));

		let Some(&[flags, _, header]) = sector.get(body..body + 3) else {
			break;
		};
		if flags >> 6 != 2 {
			continue;
		}
		let scrambled = flags & 0x30 != 0;
		let id = match code {
			PRIVATE_1 => match sector.get(body + 3 + usize::from(header)) {
				Some(&substream) => u32::from(substream),
				None => continue,
			},
			0xC0..=0xEF => 0x100 | u32::from(code),
			_ => continue,
		};
		packets.push(Packet { id, scrambled });
	}
	packets
}

/// The seamless-playback fields of a nav pack's DSI, when `sector` is the nav pack numbered `lbn`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interleave {
	/// The pack is in an interleaved unit.
	pub in_unit: bool,
	/// The unit's last sector, relative to this pack.
	pub unit_end: u32,
	/// The start of the next unit of the same angle or branch, relative to this pack.
	pub next_unit: u32,
}

pub fn interleave(sector: &[u8], lbn: u32) -> Option<Interleave> {
	let be32 = |at: usize| array(sector, at).map(u32::from_be_bytes);

	if sector.get(NAV_AT..NAV_AT + 4) != Some(&NAV)
		|| sector.get(DSI_AT..DSI_AT + 4) != Some(&NAV)
		|| sector.get(DSI_AT + 6) != Some(&DSI_SUBSTREAM)
		|| be32(DSI_DATA + 4)? != lbn
	{
		return None;
	}

	let category = array(sector, DSI_DATA + 32).map(u16::from_be_bytes)?;
	Some(Interleave {
		in_unit: category & 0x4000 != 0,
		unit_end: be32(DSI_DATA + 34)?,
		next_unit: be32(DSI_DATA + 38)?,
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::disc::SECTOR_LEN;

	fn pack(packets: &[(u8, u8, Option<u8>)]) -> Vec<u8> {
		let mut sector = vec![0, 0, 1, 0xBA, 0x44, 0, 4, 0, 4, 1, 1, 0x89, 0xC3, 0xF8];
		for &(code, flags, substream) in packets {
			let mut body = vec![flags, 0x80, 5, 0x21, 0, 1, 0, 1];
			body.extend(substream);
			body.extend([0xAA; 10]);
			sector.extend([0, 0, 1, code]);
			sector.extend(u16::try_from(body.len()).unwrap().to_be_bytes());
			sector.extend(body);
		}
		sector.resize(SECTOR_LEN, 0xFF);
		sector
	}

	#[test]
	fn packets_are_named_like_ffmpeg_names_their_streams() {
		let sector = pack(&[
			(0xE0, 0x81, None),
			(0xBD, 0x81, Some(0x80)),
			(0xBD, 0x81, Some(0x21)),
			(0xC0, 0x80, None),
		]);

		let ids: Vec<u32> = packets(&sector).iter().map(|packet| packet.id).collect();

		assert_eq!(ids, [0x1E0, 0x80, 0x21, 0x1C0]);
		assert!(packets(&sector).iter().all(|packet| !packet.scrambled));
		assert!(packets(&[0; SECTOR_LEN]).is_empty());
	}

	#[test]
	fn scrambled_payloads_are_flagged() {
		let sector = pack(&[(0xBF, 0x00, None), (0xE0, 0x81 | 0x10, None), (0xBD, 0xA1, Some(0x80))]);

		let flags: Vec<(u32, bool)> = packets(&sector)
			.iter()
			.map(|packet| (packet.id, packet.scrambled))
			.collect();

		assert_eq!(flags, [(0x1E0, true), (0x80, true)]);
	}

	#[test]
	fn a_nav_pack_gives_its_interleaved_unit() {
		let mut sector = vec![0; SECTOR_LEN];
		sector[NAV_AT..NAV_AT + 4].copy_from_slice(&NAV);
		sector[DSI_AT..DSI_AT + 4].copy_from_slice(&NAV);
		sector[DSI_AT + 6] = DSI_SUBSTREAM;
		sector[DSI_DATA + 4..DSI_DATA + 8].copy_from_slice(&700_u32.to_be_bytes());
		sector[DSI_DATA + 32..DSI_DATA + 34].copy_from_slice(&0x6000_u16.to_be_bytes());
		sector[DSI_DATA + 34..DSI_DATA + 38].copy_from_slice(&40_u32.to_be_bytes());
		sector[DSI_DATA + 38..DSI_DATA + 42].copy_from_slice(&90_u32.to_be_bytes());

		assert_eq!(
			interleave(&sector, 700),
			Some(Interleave {
				in_unit: true,
				unit_end: 40,
				next_unit: 90,
			})
		);
		assert_eq!(interleave(&sector, 701), None);
		assert_eq!(interleave(&sector[..1000], 700), None);
	}
}
