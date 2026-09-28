use std::ops::RangeInclusive;

use serde::{Deserialize, Serialize};

pub const VIDEO_MEAN_RANGE: RangeInclusive<f64> = 80.0..=99.0;
pub const VIDEO_FLOOR_MIN: f64 = 70.0;
pub const IMAGE_RANGE: RangeInclusive<f64> = 50.0..=95.0;
pub const AUDIO_STEREO_RANGE: RangeInclusive<u32> = 64..=320;
pub const AUDIO_STEP_KBPS: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
pub enum Speed {
	Slow,
	#[default]
	Moderate,
	Fast,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct VmafOverride {
	pub mean: f64,
	pub floor: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Overrides {
	pub video: Option<VmafOverride>,
	pub image: Option<f64>,
	pub audio_stereo_kbps: Option<u32>,
}

impl Overrides {
	pub fn is_empty(&self) -> bool {
		*self == Self::default()
	}

	pub fn validate(&self) -> Result<(), String> {
		if let Some(video) = self.video {
			if !VIDEO_MEAN_RANGE.contains(&video.mean) {
				return Err(format!(
					"the video VMAF mean must be between {} and {}",
					VIDEO_MEAN_RANGE.start(),
					VIDEO_MEAN_RANGE.end()
				));
			}

			if !(VIDEO_FLOOR_MIN..=video.mean).contains(&video.floor) {
				return Err(format!(
					"the video VMAF floor must be between {VIDEO_FLOOR_MIN} and the mean ({})",
					video.mean
				));
			}
		}

		if let Some(image) = self.image
			&& !IMAGE_RANGE.contains(&image)
		{
			return Err(format!(
				"the image SSIMULACRA2 target must be between {} and {}",
				IMAGE_RANGE.start(),
				IMAGE_RANGE.end()
			));
		}

		if let Some(kbps) = self.audio_stereo_kbps
			&& (!AUDIO_STEREO_RANGE.contains(&kbps) || kbps % AUDIO_STEP_KBPS != 0)
		{
			return Err(format!(
				"the stereo audio bitrate must be between {} and {} kbps, in steps of {AUDIO_STEP_KBPS}",
				AUDIO_STEREO_RANGE.start(),
				AUDIO_STEREO_RANGE.end()
			));
		}

		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn video(mean: f64, floor: f64) -> Overrides {
		Overrides {
			video: Some(VmafOverride { mean, floor }),
			..Overrides::default()
		}
	}

	fn image(target: f64) -> Overrides {
		Overrides {
			image: Some(target),
			..Overrides::default()
		}
	}

	fn audio(kbps: u32) -> Overrides {
		Overrides {
			audio_stereo_kbps: Some(kbps),
			..Overrides::default()
		}
	}

	fn reason(overrides: Overrides) -> String {
		overrides.validate().expect_err("the override is out of range")
	}

	#[test]
	fn no_overrides_are_empty_and_valid() {
		assert!(Overrides::default().is_empty());
		assert!(!image(80.0).is_empty());
		assert_eq!(Overrides::default().validate(), Ok(()));
	}

	#[test]
	fn video_overrides_accept_the_range_edges_and_name_the_range() {
		for valid in [video(80.0, 70.0), video(99.0, 99.0), video(90.0, 90.0)] {
			assert_eq!(valid.validate(), Ok(()), "{valid:?}");
		}

		assert!(reason(video(79.9, 70.0)).contains("between 80 and 99"));
		assert!(reason(video(99.1, 90.0)).contains("between 80 and 99"));
		assert!(reason(video(f64::NAN, 70.0)).contains("between 80 and 99"));
		assert!(reason(video(90.0, 69.9)).contains("between 70 and the mean (90)"));
		assert!(reason(video(90.0, 90.1)).contains("between 70 and the mean (90)"));
	}

	#[test]
	fn image_overrides_accept_the_range_edges_and_name_the_range() {
		assert_eq!(image(50.0).validate(), Ok(()));
		assert_eq!(image(95.0).validate(), Ok(()));

		assert!(reason(image(49.9)).contains("between 50 and 95"));
		assert!(reason(image(95.1)).contains("between 50 and 95"));
	}

	#[test]
	fn audio_overrides_accept_the_range_edges_in_steps_of_eight() {
		assert_eq!(audio(64).validate(), Ok(()));
		assert_eq!(audio(320).validate(), Ok(()));
		assert_eq!(audio(160).validate(), Ok(()));

		for invalid in [56, 328, 100] {
			assert!(
				reason(audio(invalid)).contains("between 64 and 320 kbps, in steps of 8"),
				"{invalid}"
			);
		}
	}

	#[test]
	fn overrides_read_records_that_lack_a_field() {
		let read: Overrides = serde_json::from_str(r#"{"image":60.0}"#).unwrap();

		assert_eq!(read, image(60.0));
		assert_eq!(
			serde_json::from_str::<Overrides>(&serde_json::to_string(&video(90.0, 85.0)).unwrap()).unwrap(),
			video(90.0, 85.0)
		);
	}
}
