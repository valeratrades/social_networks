//! What a Messenger thread says about when the other side was last on facebook: the "Active now" /
//! "Active 3h ago" line in its header, and a "Seen" receipt under our last message. Both are lower
//! bounds on a last login, never an upper one.

use std::sync::LazyLock;

use color_eyre::eyre::{Result, bail};
use jiff::{SignedDuration, Timestamp};
use regex::Regex;

/// The newest time `head` (the thread's text outside its message log) says they were active.
/// A stated age is rounded to one unit by facebook, so it counts one unit older than it reads.
pub fn active(head: &[String], read: Timestamp) -> Result<Option<Timestamp>> {
	let mut newest = None;
	for line in head.iter().filter(|l| l.starts_with("Active ")) {
		let Some(c) = AGO.captures(line) else {
			bail!("`{line}` is a presence line in a shape this does not know")
		};
		let at = match c.get(1) {
			Some(_) => read,
			None => {
				let n: i64 = c[2].parse()?;
				let unit = match &c[3][..1] {
					"m" => 60,
					"h" => 3600,
					"d" => 86400,
					"w" => 604800,
					_ => bail!("`{line}` counts in a unit this does not know"),
				};
				read - SignedDuration::from_secs((n + 1) * unit)
			}
		};
		newest = newest.max(Some(at));
	}
	Ok(newest)
}
/// Whether `last`, the final line of the message log, is a receipt: only our own messages carry one.
pub fn seen(last: &str) -> bool {
	SEEN.is_match(last)
}
static AGO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^Active (?:(now)|(\d+) ?([a-z]+) ago)$").expect("literal pattern"));
static SEEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^Seen(?: (?:by|at|just|\d).*)?$").expect("literal pattern"));
