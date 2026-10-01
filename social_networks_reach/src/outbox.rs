//! `<person>/outbox/<messenger>/<at>.md`: a message written ahead of time, sent by `purpose send` once
//! `at` has passed. Whatever writes a campaign only drops files here; sending reads nothing else.

use std::path::{Path, PathBuf};

use color_eyre::eyre::{Result, WrapErr, bail};
use jiff::{Timestamp, civil::DateTime, tz::TimeZone};

/// Fixed width, minute precision, so names sort in time order.
const AT: &str = "%Y-%m-%dT%H:%MZ";

#[derive(Debug)]
pub struct Scheduled {
	pub at: Timestamp,
	/// The directory name, which is a `handles` key.
	pub messenger: String,
	pub path: PathBuf,
	pub text: String,
}

/// The earliest file whose `at` is not after `now`. Every file in the outbox is checked, due or not.
pub fn due(person_dir: &Path, now: Timestamp) -> Result<Option<Scheduled>> {
	let outbox = person_dir.join("outbox");
	if !outbox.exists() {
		return Ok(None);
	}
	let mut earliest: Option<Scheduled> = None;
	for messenger in std::fs::read_dir(&outbox).wrap_err_with(|| format!("failed to read {}", outbox.display()))? {
		let messenger = messenger?.path();
		if !messenger.is_dir() {
			bail!("{} is not a messenger directory", messenger.display());
		}
		for file in std::fs::read_dir(&messenger).wrap_err_with(|| format!("failed to read {}", messenger.display()))? {
			let path = file?.path();
			let at = parse_at(&path)?;
			let text = std::fs::read_to_string(&path).wrap_err_with(|| format!("failed to read {}", path.display()))?;
			let text = text.strip_suffix('\n').unwrap_or(&text).to_owned(); // an editor's trailing newline is not part of the message
			if text.trim().is_empty() {
				bail!("{} is empty", path.display());
			}
			if at > now || earliest.as_ref().is_some_and(|e| e.at <= at) {
				continue;
			}
			let messenger = messenger.file_name().expect("read_dir yields named entries").to_string_lossy().into_owned();
			earliest = Some(Scheduled { at, messenger, path, text });
		}
	}
	Ok(earliest)
}

fn parse_at(path: &Path) -> Result<Timestamp> {
	let name = path.file_name().expect("read_dir yields named entries").to_string_lossy();
	let Some(stem) = name.strip_suffix(".md") else {
		bail!("{} is not `<at>.md`", path.display())
	};
	let at = stem
		.strip_suffix('Z')
		.and_then(|civil| DateTime::strptime("%Y-%m-%dT%H:%M", civil).ok()) // reported below, with the format it should have had
		.and_then(|dt| dt.to_zoned(TimeZone::UTC).ok())
		.map(|z| z.timestamp());
	match at {
		Some(at) if at.strftime(AT).to_string() == stem => Ok(at),
		_ => bail!("{} is not named `{AT}.md`, e.g. `2026-10-14T09:30Z.md`", path.display()),
	}
}
