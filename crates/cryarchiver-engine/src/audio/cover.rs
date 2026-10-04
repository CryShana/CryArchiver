use std::error::Error;
use std::path::Path;

use lofty::config::ParseOptions;
use lofty::file::{TaggedFile, TaggedFileExt};
use lofty::ogg::OggPictureStorage;
use lofty::picture::Picture;
use lofty::probe::Probe;
use lofty::tag::Tag;

use crate::audio::tags;

pub fn source_pictures(source: &Path) -> Vec<Picture> {
	let Ok(tagged) = read_tags(source) else {
		return Vec::new();
	};

	let mut unique: Vec<Picture> = Vec::new();
	for picture in tagged.tags().iter().flat_map(Tag::pictures) {
		if !unique
			.iter()
			.any(|seen| seen.pic_type() == picture.pic_type() && seen.data() == picture.data())
		{
			unique.push(picture.clone());
		}
	}
	unique
}

/// Writes `pictures` into an Ogg Opus or Vorbis file as `METADATA_BLOCK_PICTURE` comments.
pub fn add_pictures(ogg: &Path, pictures: Vec<Picture>) -> Result<(), Box<dyn Error + Send + Sync>> {
	if pictures.is_empty() {
		return Ok(());
	}

	tags::edit_comments(ogg, |comments| {
		let added = pictures.len();
		for picture in pictures {
			comments.insert_picture(picture, None)?;
		}
		Ok(added)
	})
	.map(drop)
}

fn read_tags(source: &Path) -> Result<TaggedFile, Box<dyn Error + Send + Sync>> {
	Ok(Probe::open(source)?
		.options(ParseOptions::new().read_properties(false))
		.guess_file_type()?
		.read()?)
}
