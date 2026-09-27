use std::{collections::VecDeque, io::Write as _, path::PathBuf, time::Duration};

use color_eyre::eyre::{Result, WrapErr};
use jiff::{SignedDuration, Timestamp};

/// ≤ `per_hour` actions in any trailing hour, each after a pause. Every action is appended to `log`,
/// so a restart does not reset the budget.
pub(super) struct Pacer {
	what: &'static str,
	per_hour: usize,
	pause_ms: [u64; 2],
	recent: VecDeque<Timestamp>,
	log: PathBuf,
}
impl Pacer {
	pub(super) fn load(log: PathBuf, what: &'static str, per_hour: u32, pause_secs: [u64; 2]) -> Result<Self> {
		assert!(per_hour > 0, "a cap of 0 {what}/h would never do anything");
		assert!(pause_secs[0] <= pause_secs[1], "`pause_secs` is `[min, max]`");
		let hour_ago = Timestamp::now().checked_sub(SignedDuration::from_hours(1)).expect("in range");
		let mut recent = match std::fs::read_to_string(&log) {
			Ok(s) => s
				.lines()
				.map(|l| l.parse::<Timestamp>().wrap_err_with(|| format!("{} holds `{l}`, not a timestamp", log.display())))
				.collect::<Result<VecDeque<_>>>()?,
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => VecDeque::new(),
			Err(e) => return Err(e).wrap_err_with(|| format!("failed to read {}", log.display())),
		};
		recent.retain(|t| *t > hour_ago);
		let mut pruned = String::new();
		recent.iter().for_each(|t| pruned += &format!("{t}\n"));
		std::fs::write(&log, pruned).wrap_err_with(|| format!("failed to write {}", log.display()))?;
		Ok(Self {
			what,
			per_hour: per_hour as usize,
			pause_ms: pause_secs.map(|s| s * 1000),
			recent,
			log,
		})
	}

	pub(super) async fn wait(&mut self) -> Result<()> {
		let hour = SignedDuration::from_hours(1);
		loop {
			while self.recent.front().is_some_and(|t| Timestamp::now().duration_since(*t) > hour) {
				self.recent.pop_front();
			}
			if self.recent.len() < self.per_hour {
				break;
			}
			let until = self.recent.front().expect("len ≥ per_hour > 0").checked_add(hour).expect("in range");
			let wait = Duration::try_from(until.duration_since(Timestamp::now())).expect("front is within the last hour");
			eprintln!("hourly cap of {} {} reached; sleeping {}s", self.per_hour, self.what, wait.as_secs());
			tokio::time::sleep(wait).await;
		}
		tokio::time::sleep(Duration::from_millis(rand::random_range(self.pause_ms[0]..=self.pause_ms[1]))).await;
		let now = Timestamp::now();
		self.recent.push_back(now);
		let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.log)?;
		writeln!(f, "{now}").wrap_err_with(|| format!("failed to write {}", self.log.display()))
	}
}
