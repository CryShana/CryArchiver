//! Paths relative to the source or destination root, as they are stored in the project file: the components' OS
//! bytes joined with `/` (WTF-8 on Windows, raw bytes on Unix). Neither OS allows `/` inside a name.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Component, Path, PathBuf};

use cryarchiver_engine::text::display_name;
use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

const SEPARATOR: u8 = b'/';

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct RelPath(Vec<u8>);

impl RelPath {
	pub fn root() -> Self {
		Self(Vec::new())
	}

	pub fn from_bytes(bytes: Vec<u8>) -> Self {
		Self(bytes)
	}

	/// `None` when the path isn't a plain relative path: it has a root, a prefix or a `..`.
	pub fn from_path(path: &Path) -> Option<Self> {
		let mut rel = Self::root();
		for component in path.components() {
			match component {
				Component::Normal(name) => rel = rel.join(name),
				Component::CurDir => {}
				Component::RootDir | Component::Prefix(_) | Component::ParentDir => return None,
			}
		}
		Some(rel)
	}

	pub fn as_bytes(&self) -> &[u8] {
		&self.0
	}

	pub fn is_root(&self) -> bool {
		self.0.is_empty()
	}

	#[must_use]
	pub fn join(&self, name: &OsStr) -> Self {
		let mut bytes = self.0.clone();
		if !bytes.is_empty() {
			bytes.push(SEPARATOR);
		}
		bytes.extend_from_slice(name.as_encoded_bytes());
		Self(bytes)
	}

	pub fn parent(&self) -> Option<Self> {
		if self.is_root() {
			return None;
		}

		let end = self.0.iter().rposition(|&byte| byte == SEPARATOR).unwrap_or(0);
		Some(Self(self.0[..end].to_vec()))
	}

	pub fn name(&self) -> OsString {
		let start = self
			.0
			.iter()
			.rposition(|&byte| byte == SEPARATOR)
			.map_or(0, |at| at + 1);
		os_from_bytes(&self.0[start..])
	}

	pub fn depth(&self) -> usize {
		if self.is_root() {
			0
		} else {
			self.0.split(|&byte| byte == SEPARATOR).count()
		}
	}

	pub fn components(&self) -> impl Iterator<Item = OsString> + '_ {
		self.0
			.split(|&byte| byte == SEPARATOR)
			.filter(|component| !component.is_empty())
			.map(os_from_bytes)
	}

	pub fn is_within(&self, ancestor: &Self) -> bool {
		ancestor.is_root()
			|| self.0 == ancestor.0
			|| (self.0.starts_with(&ancestor.0) && self.0.get(ancestor.0.len()) == Some(&SEPARATOR))
	}

	pub fn to_path(&self, root: &Path) -> PathBuf {
		let mut path = root.to_path_buf();
		path.extend(self.components());
		path
	}

	/// Display-safe, with `/` between components.
	pub fn display(&self) -> String {
		self.components()
			.map(|component| display_name(&component))
			.collect::<Vec<_>>()
			.join("/")
	}

	/// The path from the folder `from` to `self`, with `/` separators: `../Other/a.avif`.
	pub fn relative_from(&self, from: &Self) -> String {
		let target: Vec<OsString> = self.components().collect();
		let base: Vec<OsString> = from.components().collect();
		let shared = target.iter().zip(&base).take_while(|(a, b)| a == b).count();

		let ups = std::iter::repeat_n("..".to_owned(), base.len() - shared);
		let downs = target[shared..].iter().map(|name| name.to_string_lossy().into_owned());
		ups.chain(downs).collect::<Vec<_>>().join("/")
	}
}

impl fmt::Debug for RelPath {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{:?}", self.display())
	}
}

impl Serialize for RelPath {
	fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		serializer.serialize_bytes(&self.0)
	}
}

impl<'de> Deserialize<'de> for RelPath {
	fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
		deserializer.deserialize_bytes(BytesVisitor).map(Self)
	}
}

pub(crate) struct BytesVisitor;

impl<'de> Visitor<'de> for BytesVisitor {
	type Value = Vec<u8>;

	fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("bytes")
	}

	fn visit_bytes<E: de::Error>(self, bytes: &[u8]) -> Result<Self::Value, E> {
		Ok(bytes.to_vec())
	}

	fn visit_byte_buf<E: de::Error>(self, bytes: Vec<u8>) -> Result<Self::Value, E> {
		Ok(bytes)
	}

	fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
		let mut bytes = Vec::with_capacity(seq.size_hint().unwrap_or(0));
		while let Some(byte) = seq.next_element()? {
			bytes.push(byte);
		}
		Ok(bytes)
	}
}

pub fn os_bytes(name: &OsStr) -> Vec<u8> {
	name.as_encoded_bytes().to_vec()
}

pub fn os_from_bytes(bytes: &[u8]) -> OsString {
	match std::str::from_utf8(bytes) {
		Ok(text) => OsString::from(text),
		Err(_) => native::decode(bytes),
	}
}

/// Serde for a root path stored as OS bytes, so non-UTF-8 roots survive.
pub mod path_bytes {
	use std::path::{Path, PathBuf};

	use serde::{Deserializer, Serializer};

	use super::{BytesVisitor, os_from_bytes};

	#[allow(clippy::ptr_arg)]
	pub fn serialize<S: Serializer>(path: &PathBuf, serializer: S) -> Result<S::Ok, S::Error> {
		serializer.serialize_bytes(Path::as_os_str(path).as_encoded_bytes())
	}

	pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<PathBuf, D::Error> {
		let bytes = deserializer.deserialize_bytes(BytesVisitor)?;
		Ok(PathBuf::from(os_from_bytes(&bytes)))
	}
}

#[cfg(unix)]
mod native {
	use std::ffi::OsString;
	use std::os::unix::ffi::OsStringExt;

	pub fn decode(bytes: &[u8]) -> OsString {
		OsString::from_vec(bytes.to_vec())
	}
}

#[cfg(windows)]
mod native {
	use std::ffi::OsString;
	use std::os::windows::ffi::OsStringExt;

	const REPLACEMENT: u16 = 0xFFFD;

	/// WTF-8 is UTF-8 that may also encode lone surrogates; bytes that are neither decode to U+FFFD.
	pub fn decode(bytes: &[u8]) -> OsString {
		let mut wide = Vec::with_capacity(bytes.len());
		let mut at = 0;

		while at < bytes.len() {
			let lead = u32::from(bytes[at]);
			let width = match lead {
				0x00..=0x7F => 1,
				0xC0..=0xDF => 2,
				0xE0..=0xEF => 3,
				0xF0..=0xF7 => 4,
				_ => {
					wide.push(REPLACEMENT);
					at += 1;
					continue;
				}
			};
			let Some(sequence) = bytes.get(at..at + width) else {
				wide.push(REPLACEMENT);
				break;
			};

			let payload_bits = [0x7F, 0x1F, 0x0F, 0x07][width - 1];
			let code = sequence[1..].iter().fold(lead & payload_bits, |code, &byte| {
				(code << 6) | (u32::from(byte) & 0x3F)
			});
			at += width;

			if let Ok(unit) = u16::try_from(code) {
				wide.push(unit);
			} else {
				let offset = code - 0x1_0000;
				wide.push(0xD800 | u16::try_from(offset >> 10).unwrap_or(0));
				wide.push(0xDC00 | u16::try_from(offset & 0x3FF).unwrap_or(0));
			}
		}
		OsString::from_wide(&wide)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn joins_and_splits_components() {
		let path = RelPath::root()
			.join(OsStr::new("Photos"))
			.join(OsStr::new("2021"))
			.join(OsStr::new("a.jpg"));

		assert_eq!(path.as_bytes(), b"Photos/2021/a.jpg");
		assert_eq!(path.depth(), 3);
		assert_eq!(path.name(), "a.jpg");
		assert_eq!(path.parent().unwrap().as_bytes(), b"Photos/2021");
		assert_eq!(RelPath::root().join(OsStr::new("a")).parent(), Some(RelPath::root()));
		assert_eq!(RelPath::root().parent(), None);
		assert_eq!(
			path.to_path(Path::new("root")),
			Path::new("root").join("Photos").join("2021").join("a.jpg")
		);
	}

	#[test]
	fn converts_plain_relative_paths_only() {
		assert_eq!(
			RelPath::from_path(&Path::new("a").join(".").join("b"))
				.unwrap()
				.as_bytes(),
			b"a/b"
		);
		assert_eq!(RelPath::from_path(&Path::new("..").join("b")), None);
	}

	#[test]
	fn knows_its_ancestors() {
		let folder = RelPath::from_bytes(b"Photos".to_vec());

		assert!(RelPath::from_bytes(b"Photos/a.jpg".to_vec()).is_within(&folder));
		assert!(!RelPath::from_bytes(b"Photos2/a.jpg".to_vec()).is_within(&folder));
		assert!(folder.is_within(&RelPath::root()));
	}

	#[test]
	fn relative_links_climb_to_the_shared_folder() {
		let target = RelPath::from_bytes(b"Photos/2020/a.avif".to_vec());

		assert_eq!(
			target.relative_from(&RelPath::from_bytes(b"Photos/2021/x".to_vec())),
			"../../2020/a.avif"
		);
		assert_eq!(
			target.relative_from(&RelPath::from_bytes(b"Photos/2020".to_vec())),
			"a.avif"
		);
		assert_eq!(target.relative_from(&RelPath::root()), "Photos/2020/a.avif");
	}

	#[test]
	fn survives_messagepack() {
		let path = RelPath::root().join(OsStr::new("日本")).join(OsStr::new("a b.flac"));

		let bytes = rmp_serde::to_vec_named(&path).unwrap();

		assert_eq!(rmp_serde::from_slice::<RelPath>(&bytes).unwrap(), path);
	}

	#[cfg(windows)]
	#[test]
	fn unpaired_surrogates_survive() {
		use std::os::windows::ffi::OsStringExt;

		let name = OsString::from_wide(&[0x61, 0xD800, 0x62, 0xD83D, 0xDE00]);
		let path = RelPath::root().join(&name);

		assert_eq!(path.name(), name);
	}

	#[cfg(unix)]
	#[test]
	fn non_utf8_names_survive() {
		use std::os::unix::ffi::OsStrExt;

		let name = OsStr::from_bytes(b"caf\xe9.flac");
		let path = RelPath::root().join(name);

		assert_eq!(path.name(), name);
	}
}
