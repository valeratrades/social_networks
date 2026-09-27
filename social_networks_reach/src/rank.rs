//! Ranking people against each other. One formula for every purpose — the diagram is in
//! `social_networks/src/purpose/README.md` — and the recency axis its time-shaped terms share:
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
//! whatever cohort `Span::over` was handed, so ranking somebody alone would place their oldest
//! line at the same recency as anybody else's newest.

use std::{collections::BTreeSet, path::Path};

use color_eyre::eyre::Result;
use jiff::{Timestamp, tz::TimeZone};

use crate::{
	history::{self, ME},
	person::{Person, Value},
	purpose::{Near, Purpose, Signal, Term},
	venue,
};

/// One person's place in a ranking.
pub struct Ranked {
	pub person: Person,
	/// `Σ w·v / Σ w`, in `[0, 1]`.
	pub score: f64,
	/// Per term, in the order the purpose lists them. `None` is nothing to credit, which scores as 0.
	pub terms: Vec<Option<f64>>,
	/// No year files yet, so the transcript terms read nothing rather than a true zero.
	pub backfilling: bool,
}

/// Best first. Every cohort-relative term is relative to `people`, so the same person ranks
/// differently among different people.
pub fn rank(purpose: &Purpose, venues: &Path, people: Vec<Person>) -> Result<Vec<Ranked>> {
	let reads = |f: fn(&Signal) -> bool| purpose.rank.iter().any(|term| f(&term.signal));
	let transcript = reads(|s| matches!(s, Signal::Interactions | Signal::LastInteraction { .. }));
	let in_venues = reads(|s| matches!(s, Signal::VenueActivity { .. }));

	let mut facts = Vec::with_capacity(people.len());
	for person in &people {
		let dir = person.dir(&purpose.path);
		let lines = match transcript {
			true => venue::read(&dir, None)?,
			false => Vec::new(),
		};
		let spoke = match in_venues {
			true => venue::lines_by(venues, person, None)?.into_iter().map(|(_, line)| line.at).collect(),
			false => Vec::new(),
		};
		facts.push(Facts {
			days: lines
				.iter()
				.filter(|line| line.handle != ME)
				.map(|line| line.at.to_zoned(TimeZone::UTC).date())
				.collect::<BTreeSet<_>>()
				.len(),
			last: lines.iter().map(|line| line.at).max(),
			spoke,
			backfilling: history::Meta::load(&dir)?.backfill_status().is_some(),
		});
	}

	let columns: Vec<Vec<Option<f64>>> = purpose.rank.iter().map(|term| column(term, &people, &facts)).collect();
	let total: f64 = purpose.rank.iter().map(|term| term.weight).sum();
	let mut ranked: Vec<Ranked> = people
		.into_iter()
		.zip(facts)
		.enumerate()
		.map(|(i, (person, facts))| {
			let terms: Vec<Option<f64>> = columns.iter().map(|column| column[i]).collect();
			// absent is no credit, which makes every term a bonus
			let score = purpose.rank.iter().zip(&terms).map(|(term, v)| term.weight * v.unwrap_or(0.0)).sum::<f64>() / total;
			assert!((0.0..=1.0).contains(&score), "{}: a weighted mean of values in [0, 1] came out {score}", person.name);
			Ranked {
				person,
				score,
				terms,
				backfilling: facts.backfilling,
			}
		})
		.collect();
	ranked.sort_by(|a, b| b.score.partial_cmp(&a.score).expect("a score is finite"));
	Ok(ranked)
}

/// What their transcripts say, derived at rank time and never stored.
struct Facts {
	/// Distinct days with a line by them in their year files.
	days: usize,
	/// Their newest year-file line, in either direction.
	last: Option<Timestamp>,
	/// When they wrote each of their venue lines.
	spoke: Vec<Timestamp>,
	backfilling: bool,
}

fn column(term: &Term, people: &[Person], facts: &[Facts]) -> Vec<Option<f64>> {
	let tags = || people.iter().map(|person| person.tags.get(&term.of).and_then(Option::as_ref));
	let mistyped = |v: &Value| -> ! { unreachable!("`{}` = {} was typed against the purpose at load", term.of, v.nix()) };
	match &term.signal {
		Signal::Bool => tags()
			.map(|v| {
				v.map(|v| match v {
					Value::Bool(b) => f64::from(u8::from(*b)),
					v => mistyped(v),
				})
			})
			.collect(),
		Signal::Number { min, max } => tags()
			.map(|v| {
				v.map(|v| match v {
					Value::Number(n) => (n - min) / (max - min),
					v => mistyped(v),
				})
			})
			.collect(),
		Signal::Age { lo, hi } => {
			let today = Timestamp::now().to_zoned(TimeZone::UTC).date();
			tags()
				.map(|v| {
					v.map(|v| match v {
						Value::Birthday(birthday) => {
							let (min, max) = birthday.ages(today);
							let (min, max) = (f64::from(min), f64::from(max));
							match min == max {
								true => f64::from(u8::from((lo..=hi).contains(&&min))),
								false => (max.min(*hi) - min.max(*lo)).max(0.0) / (max - min),
							}
						}
						v => mistyped(v),
					})
				})
				.collect()
		}
		Signal::Place(near) => tags()
			.map(|v| {
				v.map(|v| match v {
					Value::Place { lat, lon, .. } => closeness(near, *lat, *lon),
					v => mistyped(v),
				})
			})
			.collect(),
		Signal::Timestamp { decay } => recency(
			tags()
				.map(|v| {
					v.map(|v| match v {
						Value::Timestamp(at) => *at,
						v => mistyped(v),
					})
				})
				.collect(),
			*decay,
		),
		Signal::LastInteraction { decay } => recency(facts.iter().map(|f| f.last.filter(|_| !f.backfilling)).collect(), *decay),
		Signal::Interactions => {
			let top = facts.iter().map(|f| f.days).max().unwrap_or(0);
			facts.iter().map(|f| (f.days > 0 && !f.backfilling).then(|| f.days as f64 / top as f64)).collect()
		}
		Signal::VenueActivity { decay } => {
			let span = Span::over(facts.iter().flat_map(|f| f.spoke.iter().copied()), *decay);
			let activity: Vec<Option<f64>> = facts
				.iter()
				.map(|f| (!f.spoke.is_empty()).then(|| span.expect("somebody spoke, so the cohort has an axis").activity(f.spoke.iter().copied())))
				.collect();
			let top = activity.iter().flatten().copied().fold(0.0, f64::max);
			activity.into_iter().map(|a| a.map(|a| a / top)).collect()
		}
	}
}

/// Over the cohort rather than per person: scored alone, somebody whose last line was two years ago
/// sits at the same recency as anybody else's newest.
fn recency(at: Vec<Option<Timestamp>>, decay: f64) -> Vec<Option<f64>> {
	let span = Span::over(at.iter().flatten().copied(), decay);
	at.into_iter()
		.map(|at| at.map(|at| span.expect("somebody has a value, so the cohort has an axis").weight(at)))
		.collect()
}

fn closeness(near: &Near, lat: f64, lon: f64) -> f64 {
	let d = haversine_km((near.lat, near.lon), (lat, lon));
	match d <= near.radius_km {
		true => 1.0,
		false => 0.5f64.powf((d - near.radius_km) / near.halving_km),
	}
}

fn haversine_km(a: (f64, f64), b: (f64, f64)) -> f64 {
	let (lat_a, lat_b) = (a.0.to_radians(), b.0.to_radians());
	let (d_lat, d_lon) = (lat_b - lat_a, (b.1 - a.1).to_radians());
	let h = (d_lat / 2.0).sin().powi(2) + lat_a.cos() * lat_b.cos() * (d_lon / 2.0).sin().powi(2);
	2.0 * 6371.0 * h.sqrt().min(1.0).asin()
}

/// The cohort a score is relative to: its newest point, and the width of it.
#[derive(Clone, Copy, Debug)]
struct Span {
	newest: Timestamp,
	/// `ln(1 + seconds)`. `None` when the cohort is one instant wide and every weight is therefore 1.
	log_width: Option<f64>,
	decay: f64,
}
impl Span {
	/// `None` when the cohort did nothing at all, which is not a score of zero — there is no axis to
	/// put anybody on.
	fn over(at: impl IntoIterator<Item = Timestamp>, decay: f64) -> Option<Self> {
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
	fn weight(&self, at: Timestamp) -> f64 {
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
	fn activity(&self, at: impl IntoIterator<Item = Timestamp>) -> f64 {
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
