//! Ranking people against each other by what they did and when.
//!
//! ```text
//!   age ──► ln(1 + age) ──► u ∈ [0,1] over the cohort ──► exp(-decay·u) ──► Σ over their items
//!            the axis          the most and least              one knob          their score
//!                              recent point in the data
//! ```
//!
//! Age is logarithmic because the steps that matter are multiplicative: today against a year ago is
//! a far wider gap than one year against two. Under a decay applied to age directly those two gaps
//! differ by a fixed factor; applied to `ln(age)` the first is orders of magnitude wider.
//!
//! `decay` is the whole of the tuning. At `0` a score is a plain count, and every item ever posted
//! weighs the same; as it rises the cohort's newest items crowd everything else out.
//!
//! A score means nothing on its own and nothing across two calls. The axis is normalised over
//! whatever cohort [`Span::over`] was handed, so ranking somebody alone would place their oldest
//! line at the same recency as anybody else's newest.

use jiff::Timestamp;

/// The cohort a score is relative to: its newest point, and the width of it.
#[derive(Clone, Copy, Debug)]
pub struct Span {
	newest: Timestamp,
	/// `ln(1 + seconds)`. `None` when the cohort is one instant wide and every weight is therefore 1.
	log_width: Option<f64>,
	decay: f64,
}
impl Span {
	/// `None` when the cohort did nothing at all, which is not a score of zero — there is no axis to
	/// put anybody on.
	pub fn over(at: impl IntoIterator<Item = Timestamp>, decay: f64) -> Option<Self> {
		assert!(decay.is_finite() && decay >= 0.0, "a decay is a finite discount of age, got {decay}");
		let (mut newest, mut oldest): (Option<Timestamp>, Option<Timestamp>) = (None, None);
		for at in at {
			newest = newest.max(Some(at));
			oldest = Some(oldest.map_or(at, |old: Timestamp| old.min(at)));
		}
		let (newest, oldest) = (newest?, oldest.expect("set alongside `newest`"));
		let width = (newest.as_second() - oldest.as_second()) as f64;
		Some(Self {
			newest,
			log_width: (width > 0.0).then(|| (1.0 + width).ln()),
			decay,
		})
	}

	/// What one item is worth, in `(0, 1]` — `1` at the cohort's newest point.
	pub fn weight(&self, at: Timestamp) -> f64 {
		let age = (self.newest.as_second() - at.as_second()) as f64;
		assert!(age >= 0.0, "{at} is above the cohort the span was built over");
		let u = match self.log_width {
			Some(log_width) => (1.0 + age).ln() / log_width,
			None => 0.0,
		};
		(-self.decay * u).exp()
	}

	/// How much somebody did, discounted by when they did it. Unbounded above and comparable only
	/// within the cohort, so a caller ranking people divides by the largest it gets back.
	pub fn activity(&self, at: impl IntoIterator<Item = Timestamp>) -> f64 {
		at.into_iter().map(|at| self.weight(at)).sum()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn at(rfc3339: &str) -> Timestamp {
		rfc3339.parse().expect("a test timestamp")
	}

	/// The knob is the whole interface, so what its ends mean is the thing worth pinning: at `0` a
	/// score counts, and turning it up hands the ranking to whoever is more recent. The crossover is
	/// what a caller is choosing between when they pick a number.
	#[test]
	fn the_decay_trades_volume_against_recency() {
		let loud = ["2024-01-01T00:00:00Z", "2024-01-02T00:00:00Z", "2024-01-03T00:00:00Z", "2024-01-04T00:00:00Z"];
		let recent = ["2026-01-01T00:00:00Z"];
		let cohort: Vec<Timestamp> = loud.iter().chain(&recent).map(|s| at(s)).collect();

		let counting = Span::over(cohort.clone(), 0.0).expect("a non-empty cohort");
		assert_eq!(counting.activity(loud.map(at)), 4.0, "every item weighs 1 when age is not discounted");
		assert_eq!(counting.activity(recent.map(at)), 1.0);

		let discounting = Span::over(cohort, 8.0).expect("a non-empty cohort");
		assert!(
			discounting.activity(recent.map(at)) > discounting.activity(loud.map(at)),
			"one item today outranks four from two years ago once age is discounted hard"
		);
	}

	/// Why the axis is `ln(age)` and not `age`: the drop over the first year has to dwarf the drop
	/// over the second. A decay applied to age directly makes the two equal, and that is the shape
	/// this rejects.
	#[test]
	fn a_step_back_in_time_costs_less_the_further_back_it_starts() {
		let now = at("2026-01-01T00:00:00Z");
		let year = at("2025-01-01T00:00:00Z");
		let two_years = at("2024-01-01T00:00:00Z");
		let span = Span::over([now, two_years], 1.0).expect("a non-empty cohort");

		let first = span.weight(now) - span.weight(year);
		let second = span.weight(year) - span.weight(two_years);
		assert!(first > second * 10.0, "the first year costs {first}, the second {second}");
	}

	/// A cohort that happened in one instant has no axis to spread anybody along, and every item in
	/// it is equally recent rather than infinitely old.
	#[test]
	fn a_cohort_of_one_instant_still_scores() {
		let only = at("2026-01-01T00:00:00Z");
		let span = Span::over([only], 5.0).expect("a non-empty cohort");
		assert_eq!(span.activity([only, only]), 2.0);
		assert!(Span::over([], 5.0).is_none(), "nothing happened, so there is no axis");
	}
}
