/// `SplitMix64`: small, seedable and identical on every platform, which keeps the snapshots stable.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
	pub const fn new(seed: u64) -> Self {
		Self(seed)
	}

	pub fn next_u64(&mut self) -> u64 {
		self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
		let mut z = self.0;
		z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
		z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
		z ^ (z >> 31)
	}

	/// Uniform in `[0, 1)`.
	pub fn unit(&mut self) -> f64 {
		let top_53_bits = self.next_u64() >> 11;
		f64::from_bits(0x3FF0_0000_0000_0000 | (top_53_bits >> 1)) - 1.0
	}

	pub fn range_f64(&mut self, low: f64, high: f64) -> f64 {
		low + (high - low) * self.unit()
	}

	pub fn below(&mut self, bound: u64) -> u64 {
		if bound == 0 {
			return 0;
		}

		self.next_u64() % bound
	}

	pub fn range_u64(&mut self, low: u64, high_inclusive: u64) -> u64 {
		low + self.below(high_inclusive - low + 1)
	}

	pub fn chance(&mut self, probability: f64) -> bool {
		self.unit() < probability
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn same_seed_same_sequence() {
		let mut a = Rng::new(7);
		let mut b = Rng::new(7);

		let first: Vec<u64> = (0..5).map(|_| a.next_u64()).collect();
		let second: Vec<u64> = (0..5).map(|_| b.next_u64()).collect();

		assert_eq!(first, second);
		assert_ne!(Rng::new(8).next_u64(), first[0]);
	}

	#[test]
	fn unit_stays_in_range() {
		let mut rng = Rng::new(1);

		assert!((0..10_000).map(|_| rng.unit()).all(|x| (0.0..1.0).contains(&x)));
	}
}
