//! How a session paced against a rate-sensitive platform spends its time. Detection keys on volume
//! and query pattern before micro-motion, so this is ordered that way: when the session is on at all
//! (active hours, bursts and breaks), how much it does (caps per hour and per day), then how long it
//! dwells before each action. Nothing here names a platform.

use std::{
	collections::VecDeque,
	io::Write as _,
	ops::Deref,
	path::{Path, PathBuf},
	time::Duration,
};

use color_eyre::eyre::{Result, WrapErr};
use jiff::{SignedDuration, Timestamp, tz::TimeZone};
use serde::{Deserialize, Serialize};

/// How far a burst or a break strays from its median, as the σ of its log-normal.
const PHASE_SPREAD: f64 = 0.4;
const BAND: usize = 50;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BehaviourConfig {
	/// `[start, end)` local hours
	pub active_hours: [u8; 2],
	pub burst_min: f64,
	pub break_min: f64,
	/// of loads, how many are ordinary ones, which the adapter picks
	pub noise_share: f64,
	pub load: Pace,
	pub scroll: ScrollPace,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Pace {
	pub per_hour: u32,
	pub per_day: u32,
	/// median of the log-normal pause before the action
	pub dwell_secs: f64,
	/// its σ
	pub spread: f64,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScrollPace {
	pub per_hour: u32,
	pub per_day: u32,
	pub dwell_secs: f64,
	pub spread: f64,
	/// added to the dwell for each item the last page showed
	pub read_secs_per_item: f64,
}

#[derive(Clone, Copy, Debug)]
pub enum Action {
	Load,
	/// `seen`: the items the page showed since the last action
	Scroll {
		seen: usize,
	},
}

/// One per session. The logs and the phase sit in its state dir, so a restart continues the same shape.
pub struct Behaviour {
	config: BehaviourConfig,
	phase: Phase,
	phase_file: PathBuf,
	loads: Log,
	scrolls: Log,
}
impl Behaviour {
	pub fn load(config: &BehaviourConfig, dir: &Path) -> Result<Self> {
		let c = config;
		let [start, end] = c.active_hours;
		assert!(start < end && end <= 24, "`active_hours` is `[start, end)` within a day, got {:?}", c.active_hours);
		assert!(c.burst_min > 0. && c.break_min > 0., "a burst and a break take time");
		assert!((0. ..1.).contains(&c.noise_share), "`noise_share` is a share of loads below 1");
		for (per_hour, per_day, spread) in [(c.load.per_hour, c.load.per_day, c.load.spread), (c.scroll.per_hour, c.scroll.per_day, c.scroll.spread)] {
			assert!(per_hour > 0 && per_day >= per_hour, "caps are positive and a day holds at least an hour");
			assert!(spread >= 0., "`spread` is a σ");
		}
		let phase_file = dir.join("phase.toml");
		let phase = match std::fs::read_to_string(&phase_file) {
			Ok(s) => toml::from_str(&s).wrap_err_with(|| format!("{} is corrupt", phase_file.display()))?,
			// a break long over: the first action starts a burst
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => Phase {
				burst: false,
				until: Timestamp::UNIX_EPOCH,
			},
			Err(e) => return Err(e).wrap_err_with(|| format!("failed to read {}", phase_file.display())),
		};
		Ok(Self {
			config: config.clone(),
			phase,
			phase_file,
			loads: Log::load(dir.join("views"))?,
			scrolls: Log::load(dir.join("scrolls"))?,
		})
	}

	/// Sleeps until `action` is due, dwells, then logs it.
	pub async fn act(&mut self, action: Action) -> Result<()> {
		while let Some((until, why)) = self.due(action, Timestamp::now())? {
			let wait = until.duration_since(Timestamp::now());
			if wait.is_positive() {
				eprintln!("{why}; sleeping until {}", until.to_zoned(TimeZone::system()).strftime("%H:%M:%S"));
				tokio::time::sleep(Duration::try_from(wait).expect("positive")).await;
			}
		}
		let dwell = match action {
			Action::Load => log_normal(self.config.load.dwell_secs, self.config.load.spread),
			Action::Scroll { seen } => log_normal(self.config.scroll.dwell_secs, self.config.scroll.spread) + seen as f64 * self.config.scroll.read_secs_per_item,
		};
		tokio::time::sleep(Duration::from_secs_f64(dwell)).await;
		match action {
			Action::Load => &mut self.loads,
			Action::Scroll { .. } => &mut self.scrolls,
		}
		.push(Timestamp::now())
	}

	/// Whether the next load should be an ordinary one rather than the one the work wants.
	pub fn noise(&self) -> bool {
		rand::random_bool(self.config.noise_share)
	}

	/// When `action` may go next and why not now; `None` is now.
	fn due(&mut self, action: Action, now: Timestamp) -> Result<Option<(Timestamp, &'static str)>> {
		let local = now.to_zoned(TimeZone::system());
		let [start, end] = self.config.active_hours.map(i8::try_from).map(|h| h.expect("asserted ≤ 24"));
		if !(start..end).contains(&local.hour()) {
			let today = local.with().hour(start).minute(0).second(0).subsec_nanosecond(0).build()?;
			let next = match local.hour() < start {
				true => today,
				false => today.tomorrow()?,
			};
			return Ok(Some((next.timestamp(), "outside active hours")));
		}

		if now >= self.phase.until {
			self.phase = match self.phase.burst {
				// the break began where the burst ended, so one that has already passed is not taken again
				true => Phase {
					burst: false,
					until: self.phase.until + minutes(self.config.break_min),
				},
				false => Phase {
					burst: true,
					until: now + minutes(self.config.burst_min),
				},
			};
			if !self.phase.burst && now >= self.phase.until {
				self.phase = Phase {
					burst: true,
					until: now + minutes(self.config.burst_min),
				};
			}
			std::fs::write(&self.phase_file, toml::to_string(&self.phase)?).wrap_err_with(|| format!("failed to write {}", self.phase_file.display()))?;
		}
		if !self.phase.burst {
			return Ok(Some((self.phase.until, "on a break")));
		}

		let (log, per_hour, per_day) = match action {
			Action::Load => (&mut self.loads, self.config.load.per_hour, self.config.load.per_day),
			Action::Scroll { .. } => (&mut self.scrolls, self.config.scroll.per_hour, self.config.scroll.per_day),
		};
		let [hour, day] = [SignedDuration::from_hours(1), SignedDuration::from_hours(24)];
		log.prune(now - day);
		let aged_out = |cap: u32, span: SignedDuration| {
			let within = log.recent.iter().filter(|t| **t > now - span).count();
			(within >= cap as usize).then(|| log.recent[log.recent.len() - cap as usize] + span)
		};
		Ok(match (aged_out(per_day, day), aged_out(per_hour, hour)) {
			(Some(t), _) => Some((t, "daily cap reached")),
			(None, Some(t)) => Some((t, "hourly cap reached")),
			(None, None) => None,
		})
	}
}

/// The items shuffled within consecutive bands of [`BAND`], the bands kept in order. The seed is made
/// once at `seed` and reused, so the order survives a restart and a cursor into it still resumes.
pub struct Order<T>(Vec<T>);
impl<T> Order<T> {
	pub fn load(items: impl IntoIterator<Item = T>, seed: &Path) -> Result<Self> {
		let seed = match std::fs::read_to_string(seed) {
			Ok(s) => s.trim().parse::<u64>().wrap_err_with(|| format!("{} holds no seed", seed.display()))?,
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
				let s: u64 = rand::random();
				std::fs::write(seed, s.to_string()).wrap_err_with(|| format!("failed to write {}", seed.display()))?;
				s
			}
			Err(e) => return Err(e).wrap_err_with(|| format!("failed to read {}", seed.display())),
		};
		// splitmix64 rather than a `rand` generator, whose streams are free to change between versions
		let mut state = seed;
		let mut next = move || {
			state = state.wrapping_add(0x9e3779b97f4a7c15);
			let mut z = state;
			z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
			z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
			z ^ (z >> 31)
		};
		let mut items: Vec<T> = items.into_iter().collect();
		for band in items.chunks_mut(BAND) {
			for i in (1..band.len()).rev() {
				band.swap(i, (next() % (i as u64 + 1)) as usize);
			}
		}
		Ok(Self(items))
	}
}
impl<T> Deref for Order<T> {
	type Target = [T];

	fn deref(&self) -> &[T] {
		&self.0
	}
}

#[derive(Debug, Deserialize, Serialize)]
struct Phase {
	burst: bool,
	until: Timestamp,
}

/// Append-only, one timestamp a line, pruned to a day on load.
struct Log {
	recent: VecDeque<Timestamp>,
	path: PathBuf,
}
impl Log {
	fn load(path: PathBuf) -> Result<Self> {
		let mut recent = match std::fs::read_to_string(&path) {
			Ok(s) => s
				.lines()
				.map(|l| l.parse::<Timestamp>().wrap_err_with(|| format!("{} holds `{l}`, not a timestamp", path.display())))
				.collect::<Result<VecDeque<_>>>()?,
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => VecDeque::new(),
			Err(e) => return Err(e).wrap_err_with(|| format!("failed to read {}", path.display())),
		};
		assert!(recent.iter().is_sorted(), "{} is appended to in time order", path.display());
		let day_ago = Timestamp::now() - SignedDuration::from_hours(24);
		while recent.front().is_some_and(|t| *t <= day_ago) {
			recent.pop_front();
		}
		let pruned: String = recent.iter().map(|t| format!("{t}\n")).collect();
		std::fs::write(&path, pruned).wrap_err_with(|| format!("failed to write {}", path.display()))?;
		Ok(Self { recent, path })
	}

	fn prune(&mut self, before: Timestamp) {
		while self.recent.front().is_some_and(|t| *t <= before) {
			self.recent.pop_front();
		}
	}

	fn push(&mut self, at: Timestamp) -> Result<()> {
		self.recent.push_back(at);
		let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
		writeln!(f, "{at}").wrap_err_with(|| format!("failed to write {}", self.path.display()))
	}
}

fn minutes(m: f64) -> SignedDuration {
	SignedDuration::from_secs_f64(log_normal(m * 60., PHASE_SPREAD))
}

/// `median · e^(σ·z)`, z standard normal by Box–Muller.
pub(crate) fn log_normal(median: f64, sigma: f64) -> f64 {
	let u = 1. - rand::random::<f64>();
	let z = (-2. * u.ln()).sqrt() * (std::f64::consts::TAU * rand::random::<f64>()).cos();
	median * (sigma * z).exp()
}

#[cfg(test)]
mod tests {
	use super::*;

	fn config() -> BehaviourConfig {
		BehaviourConfig {
			active_hours: [0, 24],
			burst_min: 1e6,
			break_min: 1.,
			noise_share: 0.,
			load: Pace {
				per_hour: 30,
				per_day: 40,
				dwell_secs: 1.,
				spread: 0.,
			},
			scroll: ScrollPace {
				per_hour: 1,
				per_day: 1,
				dwell_secs: 1.,
				spread: 0.,
				read_secs_per_item: 0.,
			},
		}
	}

	#[test]
	fn a_full_day_waits_for_its_oldest_entry_to_age_out() {
		let dir = std::env::temp_dir().join(format!("behaviour_caps_{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let now = Timestamp::now();
		// 40 loads spread over the last 20 hours, none in the last hour: only the daily cap binds
		let seeded: String = (0..40).rev().map(|i| format!("{}\n", now - SignedDuration::from_mins(90 + i * 28))).collect();
		std::fs::write(dir.join("views"), &seeded).unwrap();
		let mut b = Behaviour::load(&config(), &dir).unwrap();
		let oldest = b.loads.recent[0];
		assert_eq!(b.due(Action::Load, now).unwrap(), Some((oldest + SignedDuration::from_hours(24), "daily cap reached")));
		assert_eq!(b.due(Action::Scroll { seen: 3 }, now).unwrap(), None);
		std::fs::remove_dir_all(&dir).unwrap();
	}
}
