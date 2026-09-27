//! Leads (people new to the roster) per minute of running time, Wilder-smoothed: a plain mean
//! over the first `PERIOD`, then `avg += (x − avg) / PERIOD` per minute. Only time spent running counts.

use std::{path::PathBuf, time::Instant};

use color_eyre::eyre::{Result, WrapErr};
use jiff::SignedDuration;
use serde::{Deserialize, Serialize};

pub const PERIOD_MIN: f64 = 60.;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LeadRate {
	pub per_minute: f64,
	/// running time the figure is built from; below `PERIOD_MIN` it is a plain mean over exactly this
	pub over: SignedDuration,
	pub leads: u64,
}
impl LeadRate {
	/// `leads` found in the `elapsed` since the last record.
	pub fn record(&mut self, elapsed: SignedDuration, leads: u64) {
		let over = self.over.as_secs_f64() / 60.;
		let dt = elapsed.as_secs_f64() / 60.;
		assert!(dt >= 0., "time runs forward");
		self.leads += leads;
		self.over += elapsed;
		let k = leads as f64;
		self.per_minute = match over + dt <= PERIOD_MIN {
			true => {
				assert!(over + dt > 0., "no lead is found in zero time");
				self.leads as f64 / (over + dt)
			}
			false => {
				// the part of `dt` still inside the first period closes the plain mean, which seeds the smoothing
				let (seed, rest) = match over < PERIOD_MIN {
					true => ((self.leads as f64 - k) / PERIOD_MIN, over + dt - PERIOD_MIN),
					false => (self.per_minute, dt),
				};
				seed * (1. - 1. / PERIOD_MIN).powf(rest) + k / PERIOD_MIN
			}
		};
	}
}

impl std::fmt::Display for LeadRate {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{:.3} leads/min over {:#}", self.per_minute, self.over.round(jiff::Unit::Second).expect("in range"))
	}
}

/// Persisted across runs, so the figure keeps accruing; the clock only ticks while a run is on.
pub(super) struct Tracker {
	rate: LeadRate,
	last: Instant,
	path: PathBuf,
}
impl Tracker {
	pub(super) fn load(path: PathBuf) -> Result<Self> {
		let rate = match std::fs::read_to_string(&path) {
			Ok(s) => toml::from_str(&s).wrap_err_with(|| format!("{} is corrupt", path.display()))?,
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => LeadRate::default(),
			Err(e) => return Err(e).wrap_err_with(|| format!("failed to read {}", path.display())),
		};
		Ok(Self { rate, last: Instant::now(), path })
	}

	pub(super) fn record(&mut self, leads: u64) -> Result<&LeadRate> {
		let now = Instant::now();
		self.rate.record(SignedDuration::try_from(now - self.last).expect("a run is shorter than centuries"), leads);
		self.last = now;
		std::fs::write(&self.path, toml::to_string(&self.rate)?).wrap_err_with(|| format!("failed to write {}", self.path.display()))?;
		Ok(&self.rate)
	}
}
