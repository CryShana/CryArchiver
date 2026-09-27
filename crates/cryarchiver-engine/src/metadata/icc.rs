const HEADER_LEN: usize = 128;
const COLORANT_TOLERANCE: f64 = 0.0025;
const CURVE_TOLERANCE: f64 = 0.004;
const MAX_TAGS: usize = 200;

// D50-adapted (Bradford) colorants as they appear in rXYZ, gXYZ and bXYZ.
const SRGB: [[f64; 3]; 3] = [
	[0.4361, 0.2225, 0.0139],
	[0.3851, 0.7169, 0.0971],
	[0.1431, 0.0606, 0.7141],
];
const DISPLAY_P3: [[f64; 3]; 3] = [
	[0.5151, 0.2412, -0.0011],
	[0.2919, 0.6922, 0.0419],
	[0.1572, 0.0666, 0.7841],
];
const REC2020: [[f64; 3]; 3] = [
	[0.6734, 0.2790, -0.0019],
	[0.1656, 0.6753, 0.0299],
	[0.1251, 0.0456, 0.7967],
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cicp {
	pub primaries: u8,
	pub transfer: u8,
}

impl Cicp {
	pub const SRGB: Self = Self {
		primaries: 1,
		transfer: 13,
	};
	pub const UNSPECIFIED: Self = Self {
		primaries: 2,
		transfer: 2,
	};
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StandardSpace {
	Srgb,
	DisplayP3,
	Rec2020Srgb,
	Rec2020Bt709,
}

impl StandardSpace {
	pub fn cicp(self) -> Cicp {
		let (primaries, transfer) = match self {
			Self::Srgb => (1, 13),
			Self::DisplayP3 => (12, 13),
			Self::Rec2020Srgb => (9, 13),
			Self::Rec2020Bt709 => (9, 1),
		};
		Cicp { primaries, transfer }
	}
}

#[derive(Debug, Clone, PartialEq)]
enum Curve {
	Gamma(f64),
	Parametric(u16, Vec<f64>),
	Table(Vec<u16>),
}

pub fn problem(icc: &[u8]) -> Option<String> {
	if icc.len() < HEADER_LEN {
		return Some(format!(
			"ICC profile is {} bytes, shorter than its 128-byte header",
			icc.len()
		));
	}

	let declared = u32_at(icc, 0)?;
	if u64::from(declared) > icc.len() as u64 {
		return Some(format!(
			"ICC profile declares {declared} bytes but the block holds {}",
			icc.len()
		));
	}

	(&icc[36..40] != b"acsp").then(|| "ICC profile lacks the 'acsp' signature".to_owned())
}

pub fn colour_space(icc: &[u8]) -> Option<[u8; 4]> {
	icc.get(16..20)?.try_into().ok()
}

pub fn cicp_for(icc: &[u8]) -> Cicp {
	classify(icc).map_or(Cicp::UNSPECIFIED, StandardSpace::cicp)
}

pub fn classify(icc: &[u8]) -> Option<StandardSpace> {
	let class = icc.get(12..16)?;
	if class != b"mntr" && class != b"spac" {
		return None;
	}

	if icc.get(16..20)? != b"RGB " || icc.get(20..24)? != b"XYZ " {
		return None;
	}

	let colorants = [xyz(icc, *b"rXYZ")?, xyz(icc, *b"gXYZ")?, xyz(icc, *b"bXYZ")?];
	let red = tone_curve(icc, *b"rTRC")?;
	if tone_curve(icc, *b"gTRC")? != red || tone_curve(icc, *b"bTRC")? != red {
		return None;
	}

	let srgb_curve = max_error(&red, srgb_eotf) < CURVE_TOLERANCE;
	let bt709_curve = max_error(&red, bt709_eotf) < CURVE_TOLERANCE;
	let matches = |reference: &[[f64; 3]; 3]| distance(&colorants, reference) < COLORANT_TOLERANCE;

	if matches(&SRGB) && srgb_curve {
		Some(StandardSpace::Srgb)
	} else if matches(&DISPLAY_P3) && srgb_curve {
		Some(StandardSpace::DisplayP3)
	} else if matches(&REC2020) && srgb_curve {
		Some(StandardSpace::Rec2020Srgb)
	} else if matches(&REC2020) && bt709_curve {
		Some(StandardSpace::Rec2020Bt709)
	} else {
		None
	}
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
	Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn fixed_point(data: &[u8], at: usize) -> Option<f64> {
	Some(f64::from(u32_at(data, at)?.cast_signed()) / 65536.0)
}

fn tag(icc: &[u8], signature: [u8; 4]) -> Option<&[u8]> {
	let count = usize::try_from(u32_at(icc, HEADER_LEN)?).ok()?.min(MAX_TAGS);
	(0..count).find_map(|index| {
		let entry = HEADER_LEN + 4 + index * 12;
		if icc.get(entry..entry + 4)? != signature.as_slice() {
			return None;
		}

		let offset = usize::try_from(u32_at(icc, entry + 4)?).ok()?;
		let len = usize::try_from(u32_at(icc, entry + 8)?).ok()?;
		icc.get(offset..offset.checked_add(len)?)
	})
}

fn xyz(icc: &[u8], signature: [u8; 4]) -> Option<[f64; 3]> {
	let data = tag(icc, signature)?;
	if data.get(..4)? != b"XYZ " {
		return None;
	}
	Some([fixed_point(data, 8)?, fixed_point(data, 12)?, fixed_point(data, 16)?])
}

fn tone_curve(icc: &[u8], signature: [u8; 4]) -> Option<Curve> {
	let data = tag(icc, signature)?;
	match data.get(..4)? {
		b"curv" => {
			let count = usize::try_from(u32_at(data, 8)?).ok()?;
			let entries = data.get(12..12 + count * 2)?;
			let values: Vec<u16> = entries
				.as_chunks::<2>()
				.0
				.iter()
				.map(|pair| u16::from_be_bytes(*pair))
				.collect();
			match values.as_slice() {
				[] => Some(Curve::Gamma(1.0)),
				[gamma] => Some(Curve::Gamma(f64::from(*gamma) / 256.0)),
				_ => Some(Curve::Table(values)),
			}
		}
		b"para" => {
			let function = u16::from_be_bytes(data.get(8..10)?.try_into().ok()?);
			let count = [1, 3, 4, 5, 7][usize::from(function.min(4))];
			let parameters = (0..count)
				.map(|index| fixed_point(data, 12 + index * 4))
				.collect::<Option<Vec<f64>>>()?;
			Some(Curve::Parametric(function, parameters))
		}
		_ => None,
	}
}

#[allow(
	clippy::cast_precision_loss,
	clippy::cast_possible_truncation,
	clippy::cast_sign_loss
)]
fn evaluate(curve: &Curve, x: f64) -> f64 {
	match curve {
		Curve::Gamma(gamma) => x.powf(*gamma),
		Curve::Table(table) => {
			let position = x * (table.len() - 1) as f64;
			let index = (position.floor() as usize).min(table.len() - 2);
			let fraction = position - index as f64;
			(f64::from(table[index]) * (1.0 - fraction) + f64::from(table[index + 1]) * fraction) / 65535.0
		}
		Curve::Parametric(function, p) => match function {
			0 => x.powf(p[0]),
			1 if x >= -p[2] / p[1] => (p[1] * x + p[2]).powf(p[0]),
			1 => 0.0,
			2 if x >= -p[2] / p[1] => (p[1] * x + p[2]).powf(p[0]) + p[3],
			2 => p[3],
			3 if x >= p[4] => (p[1] * x + p[2]).powf(p[0]),
			3 => p[3] * x,
			_ if x >= p[4] => (p[1] * x + p[2]).powf(p[0]) + p[5],
			_ => p[3] * x + p[6],
		},
	}
}

fn srgb_eotf(value: f64) -> f64 {
	if value <= 0.04045 {
		value / 12.92
	} else {
		((value + 0.055) / 1.055).powf(2.4)
	}
}

fn bt709_eotf(value: f64) -> f64 {
	if value < 0.081 {
		value / 4.5
	} else {
		((value + 0.099) / 1.099).powf(1.0 / 0.45)
	}
}

fn max_error(curve: &Curve, reference: fn(f64) -> f64) -> f64 {
	(0..=255u8)
		.map(|step| {
			let x = f64::from(step) / 255.0;
			(evaluate(curve, x) - reference(x)).abs()
		})
		.fold(0.0, f64::max)
}

fn distance(colorants: &[[f64; 3]; 3], reference: &[[f64; 3]; 3]) -> f64 {
	colorants
		.iter()
		.flatten()
		.zip(reference.iter().flatten())
		.map(|(a, b)| (a - b).abs())
		.fold(0.0, f64::max)
}
