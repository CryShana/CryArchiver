use crate::probe::Stream;

const SCALE_MATRICES: &[&str] = &["bt709", "bt470bg", "smpte170m", "fcc", "smpte240m", "bt2020nc"];
const UNSET: &[&str] = &["unknown", "unspecified", "reserved"];
const RGB_PREFIXES: &[&str] = &[
	"rgb", "bgr", "gbr", "argb", "abgr", "0rgb", "0bgr", "x2rgb", "x2bgr", "pal8",
];
const PAL_RATES: &[f64] = &[25.0, 50.0];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
	Limited,
	Full,
}

impl Range {
	pub fn name(self) -> &'static str {
		match self {
			Self::Limited => "tv",
			Self::Full => "pc",
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColourTags {
	pub primaries: String,
	pub transfer: String,
	pub matrix: String,
	pub range: Range,
	/// Where the source's chroma samples sit, when it says.
	pub chroma_location: Option<String>,
}

pub fn output_tags(stream: &Stream) -> ColourTags {
	let [primaries, transfer, matrix] = inferred(stream);
	let tagged = |value: Option<&str>, fallback: &str| {
		value
			.filter(|name| !UNSET.contains(name))
			.unwrap_or(fallback)
			.to_owned()
	};

	if is_rgb(stream) {
		return ColourTags {
			primaries: tagged(stream.color_primaries.as_deref(), primaries),
			transfer: tagged(stream.color_transfer.as_deref(), transfer),
			matrix: matrix.to_owned(),
			range: Range::Limited,
			chroma_location: None,
		};
	}

	let full = stream.color_range.as_deref() == Some("pc")
		|| stream
			.pix_fmt
			.as_deref()
			.is_some_and(|format| format.starts_with("yuvj"));
	ColourTags {
		primaries: tagged(stream.color_primaries.as_deref(), primaries),
		transfer: tagged(stream.color_transfer.as_deref(), transfer),
		matrix: tagged(stream.color_space.as_deref(), matrix),
		range: if full { Range::Full } else { Range::Limited },
		chroma_location: stream
			.chroma_location
			.clone()
			.filter(|location| !UNSET.contains(&location.as_str())),
	}
}

/// The scale drops the frames' chroma location unless it is named, so it is passed through.
pub fn conversion(stream: &Stream, tags: &ColourTags, target_pix_fmt: &str) -> Option<String> {
	if stream.pix_fmt.as_deref() == Some(target_pix_fmt) {
		return None;
	}

	let range = tags.range.name();
	let chroma = tags.chroma_location.as_deref().map_or_else(String::new, |location| {
		format!(":in_chroma_loc={location}:out_chroma_loc={location}")
	});
	let scale = if is_rgb(stream) {
		format!("scale=out_range={range}:out_color_matrix={}", tags.matrix)
	} else if SCALE_MATRICES.contains(&tags.matrix.as_str()) {
		let matrix = &tags.matrix;
		format!("scale=in_range={range}:out_range={range}:in_color_matrix={matrix}:out_color_matrix={matrix}{chroma}")
	} else {
		format!("scale=in_range={range}:out_range={range}{chroma}")
	};
	Some(format!("{scale},format={target_pix_fmt}"))
}

pub fn setparams(tags: &ColourTags) -> String {
	let chroma = tags
		.chroma_location
		.as_deref()
		.map_or_else(String::new, |location| format!(":chroma_location={location}"));
	format!(
		"setparams=field_mode=prog:range={}:color_primaries={}:color_trc={}:colorspace={}{chroma}",
		tags.range.name(),
		tags.primaries,
		tags.transfer,
		tags.matrix
	)
}

fn inferred(stream: &Stream) -> [&'static str; 3] {
	let height = stream.height.unwrap_or(1080);
	if height > 576 {
		return ["bt709", "bt709", "bt709"];
	}

	let pal_rate = stream
		.fps()
		.is_some_and(|fps| PAL_RATES.iter().any(|rate| (fps - rate).abs() < 0.01));
	if height > 480 || pal_rate {
		["bt470bg", "smpte170m", "bt470bg"]
	} else {
		["smpte170m", "smpte170m", "smpte170m"]
	}
}

fn is_rgb(stream: &Stream) -> bool {
	stream.color_space.as_deref() == Some("gbr")
		|| stream
			.pix_fmt
			.as_deref()
			.is_some_and(|format| RGB_PREFIXES.iter().any(|prefix| format.starts_with(prefix)))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn stream(json: &str) -> Stream {
		serde_json::from_str(json).unwrap()
	}

	#[test]
	fn tagged_sources_keep_their_tags_verbatim() {
		let hlg = stream(
			r#"{"index":0,"codec_type":"video","width":1920,"height":1080,"pix_fmt":"yuv420p10le","color_range":"tv",
			"color_space":"bt2020nc","color_transfer":"arib-std-b67","color_primaries":"bt2020"}"#,
		);

		let tags = output_tags(&hlg);

		assert_eq!(tags.transfer, "arib-std-b67");
		assert_eq!(tags.matrix, "bt2020nc");
		assert_eq!(tags.range, Range::Limited);
		assert_eq!(conversion(&hlg, &tags, "yuv420p10le"), None);
		assert_eq!(
			setparams(&tags),
			"setparams=field_mode=prog:range=tv:color_primaries=bt2020:color_trc=arib-std-b67:colorspace=bt2020nc"
		);
	}

	#[test]
	fn untagged_sources_are_inferred_from_their_size_and_rate() {
		let ntsc = stream(
			r#"{"index":0,"codec_type":"video","width":720,"height":480,"pix_fmt":"yuv420p","r_frame_rate":"30000/1001"}"#,
		);
		let pal = stream(
			r#"{"index":0,"codec_type":"video","width":720,"height":576,"pix_fmt":"yuv420p","r_frame_rate":"25/1"}"#,
		);
		let cif = stream(
			r#"{"index":0,"codec_type":"video","width":352,"height":288,"pix_fmt":"yuv420p","r_frame_rate":"25/1"}"#,
		);
		let hd = stream(r#"{"index":0,"codec_type":"video","width":1280,"height":720,"pix_fmt":"yuv420p"}"#);

		assert_eq!(output_tags(&ntsc).matrix, "smpte170m");
		assert_eq!(output_tags(&ntsc).primaries, "smpte170m");
		assert_eq!(output_tags(&pal).primaries, "bt470bg");
		assert_eq!(output_tags(&pal).matrix, "bt470bg");
		assert_eq!(output_tags(&cif).primaries, "bt470bg");
		assert_eq!(output_tags(&hd).matrix, "bt709");
		assert_eq!(output_tags(&hd).transfer, "bt709");
	}

	#[test]
	fn only_missing_tags_are_inferred() {
		let nikon = stream(
			r#"{"index":0,"codec_type":"video","width":1920,"height":1080,"pix_fmt":"yuvj420p","color_range":"pc",
			"color_space":"smpte170m","color_primaries":"bt709"}"#,
		);

		let tags = output_tags(&nikon);

		assert_eq!(tags.matrix, "smpte170m");
		assert_eq!(tags.primaries, "bt709");
		assert_eq!(tags.transfer, "bt709");
		assert_eq!(tags.range, Range::Full);
	}

	#[test]
	fn conversions_keep_matrix_and_range() {
		let full = stream(
			r#"{"index":0,"codec_type":"video","width":640,"height":360,"pix_fmt":"yuvj420p","color_space":"bt709"}"#,
		);
		let tags = output_tags(&full);

		assert_eq!(
			conversion(&full, &tags, "yuv420p10le").as_deref(),
			Some("scale=in_range=pc:out_range=pc:in_color_matrix=bt709:out_color_matrix=bt709,format=yuv420p10le")
		);
	}

	#[test]
	fn the_chroma_location_is_carried_through_the_conversion() {
		let topleft = stream(
			r#"{"index":0,"codec_type":"video","width":1918,"height":802,"pix_fmt":"yuv420p","color_space":"bt709",
			"chroma_location":"topleft"}"#,
		);
		let unspecified = stream(
			r#"{"index":0,"codec_type":"video","width":1920,"height":1080,"pix_fmt":"yuv420p","chroma_location":"unspecified"}"#,
		);
		let tags = output_tags(&topleft);

		assert_eq!(tags.chroma_location.as_deref(), Some("topleft"));
		assert_eq!(
			conversion(&topleft, &tags, "yuv420p10le").as_deref(),
			Some(
				"scale=in_range=tv:out_range=tv:in_color_matrix=bt709:out_color_matrix=bt709:in_chroma_loc=topleft:\
				 out_chroma_loc=topleft,format=yuv420p10le"
			)
		);
		assert!(setparams(&tags).ends_with(":colorspace=bt709:chroma_location=topleft"));
		assert_eq!(output_tags(&unspecified).chroma_location, None);
		assert!(!setparams(&output_tags(&unspecified)).contains("chroma"));
	}

	#[test]
	fn matrices_scale_cannot_name_are_left_to_setparams() {
		let constant_luminance = stream(
			r#"{"index":0,"codec_type":"video","width":3840,"height":2160,"pix_fmt":"yuv422p10le","color_range":"tv",
			"color_space":"bt2020c","color_transfer":"smpte2084","color_primaries":"bt2020"}"#,
		);
		let tags = output_tags(&constant_luminance);

		assert_eq!(tags.matrix, "bt2020c");
		assert_eq!(
			conversion(&constant_luminance, &tags, "yuv420p10le").as_deref(),
			Some("scale=in_range=tv:out_range=tv,format=yuv420p10le")
		);
	}

	#[test]
	fn rgb_sources_get_a_yuv_matrix() {
		let argb = stream(r#"{"index":0,"codec_type":"video","width":640,"height":360,"pix_fmt":"argb"}"#);
		let tags = output_tags(&argb);

		assert_eq!(tags.matrix, "smpte170m");
		assert_eq!(tags.range, Range::Limited);
		assert_eq!(
			conversion(&argb, &tags, "yuva420p").as_deref(),
			Some("scale=out_range=tv:out_color_matrix=smpte170m,format=yuva420p")
		);
	}
}
