use std::collections::BTreeMap;
use std::path::Path;

use cryarchiver_engine::disc::SECTOR_LEN as SECTOR;

const PARTITION_START: u32 = 272;
const MAIN_VDS: u32 = 32;
const RESERVE_VDS: u32 = 48;
const ANCHOR: u32 = 256;
const CHUNK: usize = 1 << 20;
const EMBEDDED: usize = 1024;
const METADATA_PIECE: usize = 8 * SECTOR;
/// 2003-07-14 20:30:05 local time at UTC+2, as an ECMA-167 timestamp.
const MODIFIED: [u8; 12] = [0x78, 0x10, 0xD3, 0x07, 7, 14, 20, 30, 5, 0, 0, 0];

/// The modification time every entry an `Image` writes records, in UTC.
pub const RECORDED: &str = "2003-07-14T18:30:05Z";
pub const RECORDED_UNIX_S: u64 = 1_058_207_405;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UdfRevision {
	V102,
	V250,
}

/// A minimal UDF image: `V102` is one physical partition with File Entries and short allocation descriptors, as on a
/// DVD; `V250` adds a metadata partition with Extended File Entries and long allocation descriptors, as on a Blu-ray.
/// Files over 1 MiB are split into 1 MiB extents with a free block between them, files up to 1 KiB are embedded in
/// their entry, and the metadata file is split into 8-block pieces the same way (`write_mastered` keeps it whole, as
/// libudfread needs). Every entry records `RECORDED` as its modification time.
pub struct Image {
	pub files: Vec<(String, Vec<u8>)>,
	pub dirs: Vec<String>,
	pub revision: UdfRevision,
}

enum Node {
	Dir(BTreeMap<String, Node>),
	File(Vec<u8>),
}

impl Image {
	pub fn write(&self, path: &Path) {
		self.write_in(path, METADATA_PIECE);
	}

	pub fn write_mastered(&self, path: &Path) {
		self.write_in(path, usize::MAX);
	}

	fn write_in(&self, path: &Path, metadata_piece: usize) {
		let mut root = BTreeMap::new();
		for dir in &self.dirs {
			folder(&mut root, dir);
		}
		for (file, bytes) in &self.files {
			let (parent, name) = file.rsplit_once('/').unwrap_or(("", file));
			folder(&mut root, parent).insert(name.to_owned(), Node::File(bytes.clone()));
		}

		let mut writer = Writer {
			revision: self.revision,
			tree: Space::default(),
			data: Space::default(),
			metadata_piece,
		};
		if self.revision == UdfRevision::V250 {
			writer.data.reserve(2);
		}

		let fsd = writer.tree.reserve(1);
		let root_block = writer.tree.reserve(1);
		writer.directory(&root, root_block, root_block);
		let set = writer.file_set(fsd, root_block);
		writer.tree.put(fsd, &set);

		std::fs::write(path, writer.finish()).unwrap();
	}
}

fn folder<'a>(root: &'a mut BTreeMap<String, Node>, path: &str) -> &'a mut BTreeMap<String, Node> {
	let mut current = root;
	for name in path.split('/').filter(|name| !name.is_empty()) {
		let node = current
			.entry(name.to_owned())
			.or_insert_with(|| Node::Dir(BTreeMap::new()));
		current = match node {
			Node::Dir(children) => children,
			Node::File(_) => panic!("{name} is a file"),
		};
	}
	current
}

#[derive(Default)]
struct Space {
	bytes: Vec<u8>,
}

impl Space {
	fn blocks(&self) -> u32 {
		u32::try_from(self.bytes.len() / SECTOR).unwrap()
	}

	fn reserve(&mut self, count: usize) -> u32 {
		let at = self.blocks();
		self.bytes.resize(self.bytes.len() + count * SECTOR, 0);
		at
	}

	fn put(&mut self, block: u32, bytes: &[u8]) {
		let at = block as usize * SECTOR;
		self.bytes[at..at + bytes.len()].copy_from_slice(bytes);
	}

	fn add(&mut self, bytes: &[u8]) -> u32 {
		let at = self.reserve(bytes.len().div_ceil(SECTOR).max(1));
		self.put(at, bytes);
		at
	}
}

struct Writer {
	revision: UdfRevision,
	tree: Space,
	data: Space,
	metadata_piece: usize,
}

impl Writer {
	fn extended(&self) -> bool {
		self.revision == UdfRevision::V250
	}

	fn tree_part(&self) -> u16 {
		u16::from(self.extended())
	}

	fn version(&self) -> u16 {
		if self.extended() { 3 } else { 2 }
	}

	fn data_space(&mut self) -> &mut Space {
		if self.extended() {
			&mut self.data
		} else {
			&mut self.tree
		}
	}

	fn directory(&mut self, children: &BTreeMap<String, Node>, block: u32, parent: u32) {
		let mut fids = vec![(Vec::new(), parent, 0x0A)];
		for (name, node) in children {
			let child = self.tree.reserve(1);
			match node {
				Node::Dir(grandchildren) => {
					self.directory(grandchildren, child, block);
					fids.push((encode(name), child, 0x02));
				}
				Node::File(bytes) => {
					let entry = self.file(child, bytes);
					self.tree.put(child, &entry);
					fids.push((encode(name), child, 0x00));
				}
			}
		}

		let mut data = Vec::new();
		let mut tags = Vec::new();
		for (name, icb, characteristics) in fids {
			tags.push(data.len());
			data.extend(self.fid(&name, icb, characteristics));
		}
		let at = self.tree.add(&data);
		for offset in tags {
			let location = at + u32::try_from(offset / SECTOR).unwrap();
			let version = self.version();
			seal(&mut data[offset..], 257, location, version);
		}
		self.tree.put(at, &data);

		let descriptors = short_ad(data.len(), at);
		let entry = self.entry(block, 4, data.len(), 0, &descriptors);
		self.tree.put(block, &entry);
	}

	fn fid(&self, name: &[u8], icb: u32, characteristics: u8) -> Vec<u8> {
		let mut fid = vec![0; 38];
		fid[16..18].copy_from_slice(&1u16.to_le_bytes());
		fid[18] = characteristics;
		fid[19] = u8::try_from(name.len()).unwrap();
		fid[20..36].copy_from_slice(&long_ad(SECTOR, icb, self.tree_part()));
		fid.extend_from_slice(name);
		fid.resize(fid.len().next_multiple_of(4), 0);
		fid
	}

	fn file(&mut self, block: u32, bytes: &[u8]) -> Vec<u8> {
		if bytes.len() <= EMBEDDED {
			return self.entry(block, 5, bytes.len(), 3, bytes);
		}

		let long = self.extended();
		let mut descriptors = Vec::new();
		for chunk in bytes.chunks(CHUNK) {
			let space = self.data_space();
			let at = space.add(chunk);
			space.reserve(1);
			descriptors.extend(if long {
				long_ad(chunk.len(), at, 0).to_vec()
			} else {
				short_ad(chunk.len(), at)
			});
		}
		self.entry(block, 5, bytes.len(), u16::from(long), &descriptors)
	}

	fn entry(&self, block: u32, file_type: u8, length: usize, kind: u16, descriptors: &[u8]) -> Vec<u8> {
		let header = if self.extended() { 216 } else { 176 };
		let mut entry = vec![0; header];
		let length = length as u64;
		let blocks = length.div_ceil(SECTOR as u64);

		entry[20..22].copy_from_slice(&4u16.to_le_bytes());
		entry[24..26].copy_from_slice(&1u16.to_le_bytes());
		entry[27] = file_type;
		entry[34..36].copy_from_slice(&kind.to_le_bytes());
		entry[36..44].fill(0xFF);
		entry[48..50].copy_from_slice(&1u16.to_le_bytes());
		entry[56..64].copy_from_slice(&length.to_le_bytes());
		let ad_length = u32::try_from(descriptors.len()).unwrap().to_le_bytes();
		if self.extended() {
			entry[64..72].copy_from_slice(&length.to_le_bytes());
			entry[72..80].copy_from_slice(&blocks.to_le_bytes());
			entry[92..104].copy_from_slice(&MODIFIED);
			entry[212..216].copy_from_slice(&ad_length);
		} else {
			entry[64..72].copy_from_slice(&blocks.to_le_bytes());
			entry[84..96].copy_from_slice(&MODIFIED);
			entry[172..176].copy_from_slice(&ad_length);
		}
		entry.extend_from_slice(descriptors);
		assert!(entry.len() <= SECTOR, "an entry fits its block");

		seal(
			&mut entry,
			if self.extended() { 266 } else { 261 },
			block,
			self.version(),
		);
		entry
	}

	fn file_set(&self, block: u32, root: u32) -> Vec<u8> {
		let mut set = vec![0; 512];
		set[400..416].copy_from_slice(&long_ad(SECTOR, root, self.tree_part()));
		seal(&mut set, 256, block, self.version());
		set
	}

	fn finish(mut self) -> Vec<u8> {
		if self.extended() {
			let tree = std::mem::take(&mut self.tree.bytes);
			let mut descriptors = Vec::new();
			for piece in tree.chunks(self.metadata_piece) {
				self.data.reserve(1);
				descriptors.extend(short_ad(piece.len(), self.data.add(piece)));
			}
			for (block, file_type) in [(0, 250), (1, 251)] {
				let entry = self.entry(block, file_type, tree.len(), 0, &descriptors);
				self.data.put(block, &entry);
			}
		}
		let extended = self.extended();
		let partition = std::mem::take(if extended {
			&mut self.data.bytes
		} else {
			&mut self.tree.bytes
		});
		let partition_blocks = u32::try_from(partition.len() / SECTOR).unwrap();

		let mut image = vec![0; PARTITION_START as usize * SECTOR];
		let nsr: &[u8; 5] = if self.revision == UdfRevision::V250 {
			b"NSR03"
		} else {
			b"NSR02"
		};
		for (sector, id) in [(16, b"BEA01"), (17, nsr), (18, b"TEA01")] {
			let at = sector * SECTOR;
			image[at + 1..at + 6].copy_from_slice(id);
			image[at + 6] = 1;
		}

		let version = if self.revision == UdfRevision::V250 { 3 } else { 2 };
		for start in [MAIN_VDS, RESERVE_VDS] {
			let descriptors = [
				primary(start, version),
				self.partition(start + 1, partition_blocks, version),
				self.logical_volume(start + 2, version),
				terminator(start + 3, version),
			];
			for (index, descriptor) in descriptors.iter().enumerate() {
				let at = (start as usize + index) * SECTOR;
				image[at..at + descriptor.len()].copy_from_slice(descriptor);
			}
		}

		let mut anchor = vec![0; 512];
		anchor[16..20].copy_from_slice(&u32::try_from(16 * SECTOR).unwrap().to_le_bytes());
		anchor[20..24].copy_from_slice(&MAIN_VDS.to_le_bytes());
		anchor[24..28].copy_from_slice(&u32::try_from(16 * SECTOR).unwrap().to_le_bytes());
		anchor[28..32].copy_from_slice(&RESERVE_VDS.to_le_bytes());
		seal(&mut anchor, 2, ANCHOR, version);
		let at = ANCHOR as usize * SECTOR;
		image[at..at + anchor.len()].copy_from_slice(&anchor);

		image.extend_from_slice(&partition);
		image
	}

	fn partition(&self, location: u32, blocks: u32, version: u16) -> Vec<u8> {
		let mut descriptor = vec![0; 512];
		descriptor[16..20].copy_from_slice(&1u32.to_le_bytes());
		descriptor[20..22].copy_from_slice(&1u16.to_le_bytes());
		descriptor[25..31].copy_from_slice(if self.extended() { b"+NSR03" } else { b"+NSR02" });
		descriptor[184..188].copy_from_slice(&1u32.to_le_bytes());
		descriptor[188..192].copy_from_slice(&PARTITION_START.to_le_bytes());
		descriptor[192..196].copy_from_slice(&blocks.to_le_bytes());
		seal(&mut descriptor, 5, location, version);
		descriptor
	}

	fn logical_volume(&self, location: u32, version: u16) -> Vec<u8> {
		let mut maps = vec![1, 6, 1, 0, 0, 0];
		if self.extended() {
			let mut metadata = vec![0; 64];
			metadata[0] = 2;
			metadata[1] = 64;
			metadata[5..28].copy_from_slice(b"*UDF Metadata Partition");
			metadata[28..30].copy_from_slice(&0x0250u16.to_le_bytes());
			metadata[36..38].copy_from_slice(&1u16.to_le_bytes());
			metadata[40..44].copy_from_slice(&0u32.to_le_bytes());
			metadata[44..48].copy_from_slice(&1u32.to_le_bytes());
			metadata[48..52].copy_from_slice(&u32::MAX.to_le_bytes());
			metadata[52..56].copy_from_slice(&32u32.to_le_bytes());
			metadata[56..58].copy_from_slice(&1u16.to_le_bytes());
			maps.extend(metadata);
		}

		let mut descriptor = vec![0; 440];
		descriptor[16..20].copy_from_slice(&1u32.to_le_bytes());
		descriptor[212..216].copy_from_slice(&u32::try_from(SECTOR).unwrap().to_le_bytes());
		descriptor[217..236].copy_from_slice(b"*OSTA UDF Compliant");
		let revision: u16 = if self.extended() { 0x0250 } else { 0x0102 };
		descriptor[240..242].copy_from_slice(&revision.to_le_bytes());
		descriptor[248..264].copy_from_slice(&long_ad(SECTOR, 0, self.tree_part()));
		descriptor[264..268].copy_from_slice(&u32::try_from(maps.len()).unwrap().to_le_bytes());
		descriptor[268..272].copy_from_slice(&(1 + u32::from(self.extended())).to_le_bytes());
		descriptor.extend(maps);
		seal(&mut descriptor, 6, location, version);
		descriptor
	}
}

fn primary(location: u32, version: u16) -> Vec<u8> {
	let mut descriptor = vec![0; 512];
	descriptor[16..20].copy_from_slice(&1u32.to_le_bytes());
	seal(&mut descriptor, 1, location, version);
	descriptor
}

fn terminator(location: u32, version: u16) -> Vec<u8> {
	let mut descriptor = vec![0; 512];
	seal(&mut descriptor, 8, location, version);
	descriptor
}

/// OSTA CS0: 8-bit when every character fits a byte, otherwise UTF-16 big-endian.
fn encode(name: &str) -> Vec<u8> {
	if name.chars().all(|c| u32::from(c) <= 0xFF) {
		let mut bytes = vec![8];
		bytes.extend(name.chars().map(|c| u8::try_from(u32::from(c)).unwrap()));
		bytes
	} else {
		let mut bytes = vec![16];
		bytes.extend(name.encode_utf16().flat_map(u16::to_be_bytes));
		bytes
	}
}

fn short_ad(length: usize, block: u32) -> Vec<u8> {
	[u32::try_from(length).unwrap().to_le_bytes(), block.to_le_bytes()].concat()
}

fn long_ad(length: usize, block: u32, part: u16) -> [u8; 16] {
	let mut ad = [0; 16];
	ad[0..4].copy_from_slice(&u32::try_from(length).unwrap().to_le_bytes());
	ad[4..8].copy_from_slice(&block.to_le_bytes());
	ad[8..10].copy_from_slice(&part.to_le_bytes());
	ad
}

fn seal(descriptor: &mut [u8], id: u16, location: u32, version: u16) {
	descriptor[0..2].copy_from_slice(&id.to_le_bytes());
	descriptor[2..4].copy_from_slice(&version.to_le_bytes());
	descriptor[12..16].copy_from_slice(&location.to_le_bytes());
	descriptor[4] = descriptor[..16]
		.iter()
		.enumerate()
		.filter(|(index, _)| *index != 4)
		.fold(0u8, |sum, (_, byte)| sum.wrapping_add(*byte));
}
