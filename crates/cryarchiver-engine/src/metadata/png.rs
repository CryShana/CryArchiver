use std::io::Read;

use crate::metadata::SourceMetadata;
use crate::metadata::icc::Cicp;

const SIGNATURE_LEN: usize = 8;
const EXIF_PREFIX: &[u8] = b"Exif\0\0";
const XMP_KEYWORD: &str = "XML:com.adobe.xmp";
const RAW_PROFILE: &str = "Raw profile type ";
const MAX_INFLATED: u64 = 64 << 20;

pub(super) fn extract(bytes: &[u8]) -> SourceMetadata {
	let mut metadata = SourceMetadata::default();
	let mut at = SIGNATURE_LEN;

	while at + 12 <= bytes.len() {
		let len = u32::from_be_bytes(bytes[at..at + 4].try_into().expect("4 bytes"));
		let kind: [u8; 4] = bytes[at + 4..at + 8].try_into().expect("4 bytes");
		let name = String::from_utf8_lossy(&kind).into_owned();
		let Some(end) = usize::try_from(len)
			.ok()
			.and_then(|len| (at + 8).checked_add(len))
			.filter(|end| end + 4 <= bytes.len())
		else {
			metadata.warnings.push(format!("the {name} chunk is cut off"));
			break;
		};

		let body = &bytes[at + 8..end];
		match &kind {
			b"eXIf" => metadata.exif = Some(body.strip_prefix(EXIF_PREFIX).unwrap_or(body).to_vec()),
			b"iCCP" => read_icc(body, &mut metadata),
			b"cICP" if body.len() >= 2 => {
				metadata.cicp = Some(Cicp {
					primaries: body[0],
					transfer: body[1],
				});
			}
			b"tEXt" | b"zTXt" | b"iTXt" => match text_chunk(kind, body) {
				Ok((keyword, value)) => read_text(keyword, value, &mut metadata),
				Err(err) => metadata.warnings.push(format!("{name}: {err}")),
			},
			b"IEND" => break,
			_ => {}
		}
		at = end + 4;
	}
	metadata
}

fn read_icc(body: &[u8], metadata: &mut SourceMetadata) {
	let compressed = body
		.iter()
		.position(|&byte| byte == 0)
		.filter(|&nul| body.get(nul + 1) == Some(&0))
		.map(|nul| &body[nul + 2..]);
	match compressed.map(inflate) {
		Some(Ok(icc)) => metadata.icc = Some(icc),
		Some(Err(err)) => metadata.icc_problem = Some(format!("the iCCP chunk is unreadable: {err}")),
		None => metadata.icc_problem = Some("the iCCP chunk is malformed".into()),
	}
}

fn inflate(compressed: &[u8]) -> Result<Vec<u8>, String> {
	let mut out = Vec::new();
	flate2::read::ZlibDecoder::new(compressed)
		.take(MAX_INFLATED + 1)
		.read_to_end(&mut out)
		.map_err(|err| format!("zlib: {err}"))?;

	if out.len() as u64 > MAX_INFLATED {
		return Err("it inflates past 64 MiB".into());
	}
	Ok(out)
}

fn latin1(bytes: &[u8]) -> String {
	bytes.iter().copied().map(char::from).collect()
}

fn text_chunk(kind: [u8; 4], body: &[u8]) -> Result<(String, Vec<u8>), String> {
	let nul = body.iter().position(|&byte| byte == 0).ok_or("no keyword terminator")?;
	let keyword = latin1(&body[..nul]);
	let rest = &body[nul + 1..];

	match &kind {
		b"tEXt" => Ok((keyword, latin1(rest).into_bytes())),
		b"zTXt" => {
			let (&method, compressed) = rest.split_first().ok_or("empty zTXt chunk")?;
			if method != 0 {
				return Err("unknown compression method".into());
			}
			Ok((keyword, latin1(&inflate(compressed)?).into_bytes()))
		}
		_ => {
			let [flag, method, rest @ ..] = rest else {
				return Err("short iTXt chunk".into());
			};
			let language_end = rest
				.iter()
				.position(|&byte| byte == 0)
				.ok_or("no language terminator")?;
			let rest = &rest[language_end + 1..];
			let translated_end = rest.iter().position(|&byte| byte == 0).ok_or("no keyword terminator")?;
			let text = &rest[translated_end + 1..];
			match (flag, method) {
				(0, _) => Ok((keyword, text.to_vec())),
				(1, 0) => Ok((keyword, inflate(text)?)),
				_ => Err("unknown compression method".into()),
			}
		}
	}
}

fn read_text(keyword: String, value: Vec<u8>, metadata: &mut SourceMetadata) {
	if keyword == XMP_KEYWORD {
		metadata.xmp.get_or_insert(value);
		return;
	}

	let Some(profile) = keyword.strip_prefix(RAW_PROFILE) else {
		metadata
			.text
			.push((keyword, String::from_utf8_lossy(&value).into_owned()));
		return;
	};
	let bytes = match raw_profile(&value) {
		Ok(bytes) => bytes,
		Err(err) => {
			metadata
				.warnings
				.push(format!("raw profile '{profile}' not read: {err}"));
			return;
		}
	};
	match profile {
		"exif" | "APP1" => {
			metadata
				.exif
				.get_or_insert_with(|| bytes.strip_prefix(EXIF_PREFIX).unwrap_or(&bytes).to_vec());
		}
		"xmp" => {
			metadata.xmp.get_or_insert(bytes);
		}
		"iptc" | "8bim" => {
			metadata.irb.get_or_insert(bytes);
		}
		"icc" | "icm" => {
			metadata.icc.get_or_insert(bytes);
		}
		other => metadata.warnings.push(format!("raw profile '{other}' was dropped")),
	}
}

fn raw_profile(value: &[u8]) -> Result<Vec<u8>, String> {
	let text = std::str::from_utf8(value).map_err(|_| "not ASCII")?;
	let mut lines = text.trim_start_matches('\n').splitn(3, '\n');
	lines.next().ok_or("no profile name")?;
	let len: usize = lines
		.next()
		.ok_or("no length")?
		.trim()
		.parse()
		.map_err(|_| "unreadable length")?;
	let digits: Vec<u8> = lines
		.next()
		.ok_or("no data")?
		.bytes()
		.filter(u8::is_ascii_hexdigit)
		.collect();
	if digits.len() < len * 2 {
		return Err(format!("fewer hex digits than the declared {len} bytes"));
	}

	let nibble = |digit: u8| u8::try_from(char::from(digit).to_digit(16).expect("a hex digit")).expect("below 16");
	Ok(digits[..len * 2]
		.as_chunks::<2>()
		.0
		.iter()
		.map(|[high, low]| (nibble(*high) << 4) | nibble(*low))
		.collect())
}
