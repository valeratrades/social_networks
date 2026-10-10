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

use std::{collections::BTreeSet, path::Path, time::Duration};

use color_eyre::eyre::Result;
use derivs::{Cell, Fidelity, Id, Tape};
use jiff::{Timestamp, tz::TimeZone};
use v_utils::Timeframe;

use crate::{
	history::{self, ME},
	person::{Person, Value},
	purpose::{Near, Purpose, Refresh, Signal, Term},
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
	/// What the score stands to be off by for want of a pull, in `[0, 1]`: see `purpose/README.md`.
	pub stale: f64,
	/// What the score was multiplied by because their last line is ours, in `[0, 1)`.
	pub unanswered: Option<f64>,
}

/// Best first. Every cohort-relative term is relative to `people`, so the same person ranks
/// differently among different people.
pub fn rank(purpose: &Purpose, venues: &Path, people: Vec<Person>) -> Result<Vec<Ranked>> {
	Ok(evaluate(purpose, venues, people)?.0)
}

/// The ranking of `people` as it was computed, one row per person in the order given.
pub fn graph(purpose: &Purpose, venues: &Path, people: Vec<Person>) -> Result<Tape> {
	Ok(evaluate(purpose, venues, people)?.1)
}

fn evaluate(purpose: &Purpose, venues: &Path, people: Vec<Person>) -> Result<(Vec<Ranked>, Tape)> {
	let reads = |f: fn(&Signal) -> bool| purpose.rank.iter().any(|term| f(&term.signal));
	let in_venues = reads(|s| matches!(s, Signal::VenueActivity { .. }));
	let now = Timestamp::now();

	let mut facts = Vec::with_capacity(people.len());
	for person in &people {
		let dir = person.dir(&purpose.path);
		let lines = venue::read(&dir, None)?;
		let meta = history::Meta::load(&dir)?;
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
			last_ours: lines.iter().max_by_key(|line| line.at).filter(|line| line.handle == ME).map(|line| line.at),
			spoke,
			backfilling: meta.backfill_status().is_some(),
			fetched_at: meta.fetched_at,
			reasoned_at: meta.reasoned_at,
		});
	}

	let mut tape = Tape::new(people.iter().map(|p| p.name.clone()).collect(), String::new());
	let since = |at: Timestamp| {
		let since = now.duration_since(at);
		assert!(!since.is_negative(), "{at} is after now");
		since.unsigned_abs().as_secs_f64()
	};
	let terms: Vec<Id> = purpose.rank.iter().map(|term| record(&mut tape, purpose, term, &people, &facts, now)).collect();
	let mean = tape.weighted_mean("Σw·v/Σw", &purpose.rank.iter().zip(&terms).map(|(term, id)| (*id, term.weight)).collect::<Vec<_>>());
	let ours = tape.source(
		"ours_unanswered",
		"send",
		Some("seconds since our line, when it is the newest"),
		Fidelity::Exact,
		facts.iter().map(|f| f.last_ours.map(|at| Cell::At(since(at)))).collect(),
	);
	let half_life = purpose.unanswered_half_life;
	let four = 4.0
		* half_life
			.to_string()
			.parse::<Timeframe>()
			.expect("a half-life is written as a timeframe")
			.duration()
			.as_secs_f64();
	let unanswered = tape.map("unanswered", ours, &[], Some([0.0, four]), |c, _| 1.0 - half_life.left(Duration::from_secs_f64(point(c))));
	let score = tape.gate("score", mean, unanswered);
	let synced = [("fetched", Refresh::Fetch), ("reasoned", Refresh::Reasoning)].map(|(name, by)| {
		let cells = facts.iter().map(|f| synced(f, by).map(|at| Cell::At(since(at)))).collect();
		tape.source(name, "pull", Some("seconds since"), Fidelity::Exact, cells)
	});
	let stale = staleness(&mut tape, purpose, &terms, synced);

	let columns: Vec<Vec<Option<f64>>> = terms.iter().map(|id| tape.at(*id)).collect();
	let (means, scores, unanswered, stale) = (tape.at(mean), tape.at(score), tape.at(unanswered), tape.at(stale));
	let mut ranked: Vec<Ranked> = people
		.into_iter()
		.zip(facts)
		.enumerate()
		.map(|(i, (person, facts))| {
			let mean = means[i].expect("a mean is on every row");
			assert!((0.0..=1.0).contains(&mean), "{}: a weighted mean of values in [0, 1] came out {mean}", person.name);
			Ranked {
				person,
				score: scores[i].expect("a gate is on every row"),
				terms: columns.iter().map(|column| column[i]).collect(),
				backfilling: facts.backfilling,
				stale: stale[i].expect("staleness is on every row"),
				unanswered: unanswered[i],
			}
		})
		.collect();
	ranked.sort_by(|a, b| b.score.partial_cmp(&a.score).expect("a score is finite"));
	Ok((ranked, tape))
}

/// What their transcripts say, derived at rank time and never stored.
struct Facts {
	/// Distinct days with a line by them in their year files.
	days: usize,
	/// Their newest year-file line, in either direction.
	last: Option<Timestamp>,
	/// `last`, when that line is ours.
	last_ours: Option<Timestamp>,
	/// When they wrote each of their venue lines.
	spoke: Vec<Timestamp>,
	backfilling: bool,
	fetched_at: Option<Timestamp>,
	reasoned_at: Option<Timestamp>,
}

fn synced(f: &Facts, by: Refresh) -> Option<Timestamp> {
	match by {
		Refresh::Fetch => f.fetched_at,
		Refresh::Reasoning => f.reasoned_at,
		Refresh::Never => None,
	}
}

/// Per person, `Σ_t share_t · P(changed since the pull that refreshes t) · E|v_t − V_t|`, `V_t` being the
/// values of those that pull already reached plus one uniform draw, so an empty cohort still spreads.
fn staleness(tape: &mut Tape, purpose: &Purpose, terms: &[Id], [fetched, reasoned]: [Id; 2]) -> Id {
	let refresh: Vec<Refresh> = purpose.rank.iter().map(|term| purpose.refreshed_by(term)).collect();
	let total: f64 = purpose.rank.iter().map(|term| term.weight).sum();
	let deps: Vec<Id> = terms.iter().copied().chain([fetched, reasoned]).collect();
	tape.cohort("stale", &deps, |columns| {
		let (columns, [fetched, reasoned]) = columns.split_at(terms.len()) else { unreachable!() };
		let since = |i: usize, by: Refresh| match by {
			Refresh::Fetch => fetched[i].as_ref().map(point),
			Refresh::Reasoning => reasoned[i].as_ref().map(point),
			Refresh::Never => None,
		};
		let v = |column: &[Option<Cell>], i: usize| column[i].as_ref().map_or(0.0, point);
		// sorted, with running sums, so `Σ_c |v − c|` is a binary search rather than a pass over the cohort
		let cohorts: Vec<(Vec<f64>, Vec<f64>)> = columns
			.iter()
			.zip(&refresh)
			.map(|(column, by)| {
				let mut sorted: Vec<f64> = (0..column.len()).filter(|i| since(*i, *by).is_some()).map(|i| v(column, i)).collect();
				sorted.sort_by(f64::total_cmp);
				let sums = std::iter::once(0.0)
					.chain(sorted.iter().scan(0.0, |sum, v| {
						*sum += v;
						Some(*sum)
					}))
					.collect();
				(sorted, sums)
			})
			.collect();
		(0..fetched.len())
			.map(|i| {
				let stale = purpose
					.rank
					.iter()
					.zip(&refresh)
					.zip(columns.iter().zip(&cohorts))
					.filter(|((_, by), _)| **by != Refresh::Never)
					.map(|((term, by), (column, (sorted, sums)))| {
						let changed = match since(i, *by) {
							None => 1.0,
							Some(since) => 1.0 - purpose.stale_half_life.left(Duration::from_secs_f64(since)),
						};
						let v = v(column, i);
						let (k, m) = (sorted.partition_point(|c| *c < v), sorted.len());
						let (below, all) = (sums[k], sums[m]);
						let apart = v * k as f64 - below + (all - below) - v * (m - k) as f64;
						let far = (apart + v * v - v + 0.5) / (m + 1) as f64;
						term.weight / total * changed * far
					})
					.sum::<f64>();
				assert!((0.0..=1.0).contains(&stale), "a weighted mean of values in [0, 1] came out {stale}");
				Some(stale)
			})
			.collect()
	})
}

/// `term` onto the tape: its raw values as a source, whatever it reduces the cohort to, and its `v`.
fn record(tape: &mut Tape, purpose: &Purpose, term: &Term, people: &[Person], facts: &[Facts], now: Timestamp) -> Id {
	let of = term.of.as_str();
	let writer = match (purpose.refreshed_by(term), &term.signal) {
		(Refresh::Fetch, _) => "pull",
		(Refresh::Reasoning, _) => "extraction",
		(Refresh::Never, Signal::VenueActivity { .. }) => "recon",
		(Refresh::Never, _) => "human",
	};
	let tags = |f: &dyn Fn(&Value) -> Cell| -> Vec<Option<Cell>> { people.iter().map(|person| person.tags.get(of).and_then(Option::as_ref).map(f)).collect() };
	let mistyped = |v: &Value| -> ! { unreachable!("`{of}` = {} was typed against the purpose at load", v.nix()) };
	let backfilling = facts.iter().filter(|f| f.backfilling).count();
	let transcripts = match backfilling {
		0 => Fidelity::Exact,
		n => Fidelity::Partial(format!("{n} backfilling, read as absent")),
	};
	let v = format!("{of}.v");
	match &term.signal {
		Signal::Bool | Signal::Present => {
			let raw = tape.source(
				of,
				writer,
				None,
				Fidelity::Exact,
				tags(&|v| match v {
					Value::Bool(b) => Cell::At(f64::from(u8::from(*b))),
					Value::Text(_) => Cell::At(1.0),
					v => mistyped(v),
				}),
			);
			tape.map(&v, raw, &[], Some([0.0, 1.0]), |c, _| point(c))
		}
		Signal::Number { min, max } => {
			let raw = tape.source(
				of,
				writer,
				None,
				Fidelity::Exact,
				tags(&|v| match v {
					Value::Number(n) => Cell::At(*n),
					v => mistyped(v),
				}),
			);
			tape.map(&v, raw, &[], Some([*min, *max]), |c, _| (point(c) - min) / (max - min))
		}
		Signal::Age { lo, hi } => {
			let today = now.to_zoned(TimeZone::UTC).date();
			let raw = tape.source(
				of,
				writer,
				Some("years old"),
				Fidelity::Exact,
				tags(&|v| match v {
					Value::Birthday(birthday) => {
						let (min, max) = birthday.ages(today);
						let (min, max) = (f64::from(min), f64::from(max));
						match min == max {
							true => Cell::At(min),
							false => Cell::Within(min, max),
						}
					}
					v => mistyped(v),
				}),
			);
			tape.map(&v, raw, &[], Some([*lo, *hi]), |c, _| match c {
				Cell::At(age) => f64::from(u8::from((lo..=hi).contains(&age))),
				Cell::Within(min, max) => (max.min(*hi) - min.max(*lo)).max(0.0) / (max - min),
				Cell::Each(_) => unreachable!("an age is a point or a range"),
			})
		}
		Signal::Place(near) => {
			let raw = tape.source(
				of,
				writer,
				Some(&format!("km from {}, {}", near.lat, near.lon)),
				Fidelity::Exact,
				tags(&|v| match v {
					Value::Place { lat, lon, .. } => Cell::At(haversine_km((near.lat, near.lon), (*lat, *lon))),
					v => mistyped(v),
				}),
			);
			tape.map(&v, raw, &[], Some([0.0, near.radius_km + near.halving_km]), |c, _| closeness(near, point(c)))
		}
		Signal::Timestamp { decay } => {
			let raw = tape.source(
				of,
				writer,
				Some("unix seconds"),
				Fidelity::Exact,
				tags(&|v| match v {
					Value::Timestamp(at) => Cell::At(at.as_second() as f64),
					v => mistyped(v),
				}),
			);
			recency(tape, of, raw, *decay)
		}
		Signal::LastInteraction { decay } => {
			let cells = facts.iter().map(|f| f.last.filter(|_| !f.backfilling).map(|at| Cell::At(at.as_second() as f64))).collect();
			let raw = tape.source(of, writer, Some("unix seconds"), transcripts, cells);
			recency(tape, of, raw, *decay)
		}
		Signal::Interactions => {
			let cells = facts.iter().map(|f| (f.days > 0 && !f.backfilling).then_some(Cell::At(f.days as f64))).collect();
			let raw = tape.source(of, writer, Some("days with a line by them"), transcripts, cells);
			let top = tape.reduce(&format!("{of}.max"), &[raw], ["max"], |c| c[0].iter().flatten().map(point).reduce(f64::max).map(|top| [top]));
			tape.map(&v, raw, &[top], Some([0.0, 1.0]), |c, s| point(c) / s[0])
		}
		Signal::VenueActivity { decay } => {
			let cells = facts
				.iter()
				.map(|f| (!f.spoke.is_empty()).then(|| Cell::Each(f.spoke.iter().map(|at| at.as_second() as f64).collect())))
				.collect();
			let raw = tape.source(of, writer, Some("unix seconds of each line"), Fidelity::Exact, cells);
			let span = tape.reduce(&format!("{of}.span"), &[raw], ["newest", "width"], |c| {
				Span::over(
					c[0].iter().flatten().flat_map(|c| match c {
						Cell::Each(items) => items.iter().copied(),
						c => unreachable!("lines are many, got {c:?}"),
					}),
					*decay,
				)
				.map(Span::scalars)
			});
			let activity = tape.map(&format!("{of}.activity"), raw, &[span], None, |c, s| {
				let span = Span::new(s, *decay);
				match c {
					Cell::At(at) => span.weight(*at),
					Cell::Each(items) => span.activity(items.iter().copied()),
					Cell::Within(..) => unreachable!("lines are instants"),
				}
			});
			let top = tape.reduce(&format!("{of}.max"), &[activity], ["max"], |c| {
				let activity: Vec<f64> = c[0].iter().flatten().map(point).collect();
				(!activity.is_empty()).then(|| [activity.into_iter().fold(0.0, f64::max)])
			});
			tape.map(&v, activity, &[top], Some([0.0, 0.0]), |c, s| point(c) / s[0])
		}
	}
}

/// Over the cohort rather than per person: scored alone, somebody whose last line was two years ago
/// sits at the same recency as anybody else's newest.
fn recency(tape: &mut Tape, of: &str, raw: Id, decay: f64) -> Id {
	let span = tape.reduce(&format!("{of}.span"), &[raw], ["newest", "width"], |c| {
		Span::over(c[0].iter().flatten().map(point), decay).map(Span::scalars)
	});
	tape.map(&format!("{of}.v"), raw, &[span], None, |c, s| Span::new(s, decay).weight(point(c)))
}

fn point(cell: &Cell) -> f64 {
	match cell {
		Cell::At(v) => *v,
		cell => unreachable!("read as a point, holds {cell:?}"),
	}
}

fn closeness(near: &Near, d: f64) -> f64 {
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
	/// Unix seconds.
	newest: f64,
	/// Seconds. `0` when the cohort is one instant wide and every weight is therefore 1.
	width: f64,
	decay: f64,
}
impl Span {
	/// `None` when the cohort did nothing at all, which is not a score of zero — there is no axis to
	/// put anybody on.
	fn over(at: impl IntoIterator<Item = f64>, decay: f64) -> Option<Self> {
		assert!(decay.is_finite() && decay >= 0.0, "a decay is a finite discount of age, got {decay}");
		let (mut newest, mut oldest): (Option<f64>, Option<f64>) = (None, None);
		for at in at {
			newest = Some(newest.map_or(at, |new: f64| new.max(at)));
			oldest = Some(oldest.map_or(at, |old: f64| old.min(at)));
		}
		let (newest, oldest) = (newest?, oldest.expect("set alongside `newest`"));
		Some(Self {
			newest,
			width: newest - oldest,
			decay,
		})
	}

	/// Off the tape, as [`Self::scalars`] put it there.
	fn new(scalars: &[f64], decay: f64) -> Self {
		let [newest, width] = scalars else {
			unreachable!("a span is recorded as its newest point and its width")
		};
		Self {
			newest: *newest,
			width: *width,
			decay,
		}
	}

	fn scalars(self) -> [f64; 2] {
		[self.newest, self.width]
	}

	/// What one item is worth, in `(0, 1]` — `1` at the cohort's newest point.
	fn weight(&self, at: f64) -> f64 {
		let age = self.newest - at;
		assert!(age >= 0.0, "{at} is above the cohort the span was built over");
		let u = match self.width > 0.0 {
			true => (1.0 + age).ln() / (1.0 + self.width).ln(),
			false => 0.0,
		};
		(-self.decay * u).exp()
	}

	/// How much somebody did, discounted by when they did it. Unbounded above and comparable only
	/// within the cohort, so a caller ranking people divides by the largest it gets back.
	fn activity(&self, at: impl IntoIterator<Item = f64>) -> f64 {
		at.into_iter().map(|at| self.weight(at)).sum()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn at(rfc3339: &str) -> f64 {
		rfc3339.parse::<Timestamp>().expect("a test timestamp").as_second() as f64
	}

	/// The knob is the whole interface, so what its ends mean is the thing worth pinning: at `0` a
	/// score counts, and turning it up hands the ranking to whoever is more recent. The crossover is
	/// what a caller is choosing between when they pick a number.
	#[test]
	fn the_decay_trades_volume_against_recency() {
		let loud = ["2024-01-01T00:00:00Z", "2024-01-02T00:00:00Z", "2024-01-03T00:00:00Z", "2024-01-04T00:00:00Z"];
		let recent = ["2026-01-01T00:00:00Z"];
		let cohort: Vec<f64> = loud.iter().chain(&recent).map(|s| at(s)).collect();

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
