//! A venue transcript, in the same place and the same format as a person's:
//!
//! ```text
//! <dir>/venues/<platform>/<slug>/
//!         <year>.md      the transcript, one line per item
//!         members.json   the roster
//!         meta.json      where the last read stopped
//! ```
//!
//! Nothing is derived from the markdown that cannot be rebuilt from it. A person's own venue lines
//! are selected out of it at `pull` time by the prefix the writer put there — [`Line::read`] reads
//! the fixed `- HH:MM:SS [who/platform@slug]` it wrote and the day heading above it, and treats the
//! rest as text. There is no index, and an index would have this as its input anyway.

use std::path::{Path, PathBuf};

use color_eyre::eyre::{Result, WrapErr, eyre};
use jiff::{Timestamp, civil::Date, tz::TimeZone};
use serde::{Deserialize, Serialize};
use social_networks_adapters::reach::{Member, Page, VenueRef, VenueSource};

use crate::history::{self, Facing};

/// One transcript line, as the writer put it there.
#[derive(Clone, Debug)]
pub struct Line {
	pub handle: String,
	pub at: Timestamp,
	pub text: String,
}
impl Line {
	/// The day heading carries the date, the item prefix the time and the author. A line that is not
	/// an item is prose somebody added to the file, and belongs to nobody.
	fn read(body: &str) -> Result<Vec<Self>> {
		let mut out: Vec<Self> = Vec::new();
		let mut day: Option<Date> = None;
		for raw in body.lines() {
			if let Some(date) = raw.strip_prefix("## ") {
				day = Some(date.trim().parse().wrap_err_with(|| format!("`{date}` is not a day heading"))?);
				continue;
			}
			// a continuation is indented under the item it belongs to, which is what keeps it out of
			// the next author's line
			if raw.starts_with("  ")
				&& let Some(last) = out.last_mut()
			{
				last.text.push('\n');
				last.text.push_str(raw.trim_start());
				continue;
			}
			let Some((handle, at, text)) = split(raw, day) else { continue };
			out.push(Self { handle, at, text });
		}
		Ok(out)
	}
}

pub struct Store {
	dir: PathBuf,
	at: VenueRef,
	meta: Meta,
}
impl Store {
	/// `<rolodex dir>/venues/<platform>/<slug>`. A slug carrying a `/` — a github repo — nests, which
	/// is what its own URL does too.
	pub fn open(root: &Path, at: &VenueRef) -> Result<Self> {
		let dir = root.join("venues").join(at.platform.as_ref()).join(&at.slug);
		let path = dir.join("meta.json");
		let meta = match std::fs::read(&path) {
			Ok(bytes) => serde_json::from_slice(&bytes).wrap_err_with(|| format!("{} is not venue state", path.display()))?,
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => Meta::default(),
			Err(e) => return Err(e).wrap_err_with(|| format!("failed to read {}", path.display())),
		};
		Ok(Self { dir, at: at.clone(), meta })
	}

	pub fn dir(&self) -> &Path {
		&self.dir
	}

	/// The checkpoint the next read resumes above.
	pub fn cursor(&self) -> Option<&str> {
		self.meta.newest.as_deref()
	}

	pub fn put_roster(&self, members: &[Member]) -> Result<()> {
		std::fs::create_dir_all(&self.dir).wrap_err_with(|| format!("failed to create {}", self.dir.display()))?;
		let path = self.dir.join("members.json");
		std::fs::write(&path, serde_json::to_vec_pretty(members)?).wrap_err_with(|| format!("failed to write {}", path.display()))
	}

	pub fn roster(&self) -> Result<Vec<Member>> {
		let path = self.dir.join("members.json");
		let bytes = std::fs::read(&path).wrap_err_with(|| format!("no roster for {} — `recon members {}` writes one", self.at, self.at))?;
		serde_json::from_slice(&bytes).wrap_err_with(|| format!("{} is not a roster", path.display()))
	}

	/// Appends what is above the checkpoint and moves it. Re-reading one window twice therefore
	/// leaves the year files byte-identical.
	pub fn record(&mut self, page: Page) -> Result<usize> {
		let mut items = page.items;
		items.retain(|item| self.meta.last_item.is_none_or(|last| item.at > last));
		history::append(&self.dir, Facing::Venue(&self.at), &items, self.meta.last_item)?;

		let landed = items.len();
		self.meta.items += landed;
		self.meta.last_item = items.iter().map(|item| item.at).max().max(self.meta.last_item);
		if let Some(newest) = page.newest {
			self.meta.newest = Some(newest);
		}
		self.save()?;
		Ok(landed)
	}

	/// Every line in the transcript, oldest year first. Bounded by `since`, which is what keeps a
	/// per-pull scan proportional to what the pull is going to read anyway.
	pub fn lines(&self, since: Option<Timestamp>) -> Result<Vec<Line>> {
		let mut years: Vec<PathBuf> = match std::fs::read_dir(&self.dir) {
			Ok(entries) => entries
				.map(|e| e.map(|e| e.path()))
				.collect::<std::result::Result<Vec<_>, _>>()
				.wrap_err_with(|| format!("failed to read {}", self.dir.display()))?
				.into_iter()
				.filter(|p| p.extension().is_some_and(|e| e == "md"))
				.collect(),
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
			Err(e) => return Err(e).wrap_err_with(|| format!("failed to read {}", self.dir.display())),
		};
		years.sort();

		let mut out = Vec::new();
		for year in years {
			let body = std::fs::read_to_string(&year).wrap_err_with(|| format!("failed to read {}", year.display()))?;
			out.extend(Line::read(&body).wrap_err_with(|| format!("{}", year.display()))?);
		}
		out.retain(|line| since.is_none_or(|floor| line.at >= floor));
		Ok(out)
	}

	fn save(&self) -> Result<()> {
		std::fs::create_dir_all(&self.dir).wrap_err_with(|| format!("failed to create {}", self.dir.display()))?;
		let path = self.dir.join("meta.json");
		let tmp = self.dir.join("meta.json.tmp");
		std::fs::write(&tmp, serde_json::to_vec_pretty(&self.meta)?).wrap_err_with(|| format!("failed to write {}", tmp.display()))?;
		std::fs::rename(&tmp, &path).wrap_err_with(|| format!("failed to replace {}", path.display()))
	}
}

/// Every venue with a transcript under `root`, which is what a `pull` scans for a person's own
/// lines. A venue is a directory holding a `meta.json`; a github slug nests one level further, which
/// is what its own URL does too.
pub fn all(root: &Path) -> Result<Vec<VenueRef>> {
	let venues = root.join("venues");
	let mut out = Vec::new();
	for platform in read_dirs(&venues)? {
		let Some(name) = platform.file_name().and_then(|n| n.to_str()) else { continue };
		let Ok(platform_source) = name.parse::<VenueSource>() else { continue };
		for slug in read_dirs(&platform)? {
			match slug.join("meta.json").exists() {
				true => out.push(VenueRef::new(platform_source, relative(&platform, &slug))),
				// an owner directory, whose repos are one level down
				false =>
					for repo in read_dirs(&slug)? {
						if repo.join("meta.json").exists() {
							out.push(VenueRef::new(platform_source, relative(&platform, &repo)));
						}
					},
			}
		}
	}
	out.sort();
	Ok(out)
}
/// The roster joined against its own transcript, as an ephemeral table, so a selection can be
/// written in SQL rather than in a grammar of our own. Nothing is persisted — the markdown is the
/// store, and this table is rebuilt from it on every call.
///
/// Columns: `handle`, `display`, `joined`, `lat`, `lon`, `zone`, `posts`, `first_post`, `last_post`.
/// Dates are RFC3339 text, which sqlite compares lexicographically in the same order it compares them
/// chronologically. `lat`/`lon` are coarse by construction, so a bounding box is the honest shape of
/// a question over them.
pub async fn select(members: &[Member], lines: &[Line], predicate: &str) -> Result<Vec<Member>> {
	let db = libsql::Builder::new_local(":memory:").build().await.wrap_err("failed to open the roster table")?;
	let conn = db.connect().wrap_err("failed to connect to the roster table")?;
	conn.execute(
		"CREATE TABLE members (handle TEXT PRIMARY KEY, display TEXT NOT NULL, joined TEXT, lat REAL, lon REAL, zone TEXT, posts INTEGER NOT NULL, first_post TEXT, last_post TEXT)",
		(),
	)
	.await?;

	for member in members {
		let of_theirs: Vec<&Line> = lines.iter().filter(|line| line.handle == member.handle).collect();
		conn.execute(
			"INSERT INTO members VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
			libsql::params![
				member.handle.clone(),
				member.display.clone(),
				member.joined.map(|t| t.to_string()),
				member.lat,
				member.lon,
				member.zone.clone(),
				of_theirs.len() as i64,
				of_theirs.iter().map(|l| l.at).min().map(|t| t.to_string()),
				of_theirs.iter().map(|l| l.at).max().map(|t| t.to_string()),
			],
		)
		.await?;
	}

	let mut rows = conn
		.query(&format!("SELECT handle FROM members WHERE {predicate}"), ())
		.await
		.wrap_err_with(|| format!("`{predicate}` is not a WHERE clause over (handle, display, joined, lat, lon, zone, posts, first_post, last_post)"))?;
	let mut chosen = Vec::new();
	//LOOP: over a finite result set
	while let Some(row) = rows.next().await.wrap_err("failed to read a roster row")? {
		let handle: String = row.get(0).wrap_err("a roster row without a handle")?;
		chosen.push(
			members
				.iter()
				.find(|m| m.handle == handle)
				.cloned()
				.ok_or_else(|| eyre!("`{handle}` came back out of a table only the roster was put into"))?,
		);
	}
	Ok(chosen)
}
/// Where the last read of this venue stopped. A venue has no backfill — its feed is what the
/// platform still serves — so a checkpoint and a running tally are the whole of its state.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct Meta {
	newest: Option<String>,
	last_item: Option<Timestamp>,
	items: usize,
}

fn read_dirs(dir: &Path) -> Result<Vec<PathBuf>> {
	let entries = match std::fs::read_dir(dir) {
		Ok(entries) => entries,
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
		Err(e) => return Err(e).wrap_err_with(|| format!("failed to read {}", dir.display())),
	};
	let mut out = Vec::new();
	for entry in entries {
		let path = entry.wrap_err_with(|| format!("failed to read {}", dir.display()))?.path();
		if path.is_dir() {
			out.push(path);
		}
	}
	out.sort();
	Ok(out)
}

fn relative(base: &Path, path: &Path) -> String {
	path.strip_prefix(base).expect("walked down from `base`").to_string_lossy().into_owned()
}

/// `- HH:MM:SS [who/platform@slug] text`. `None` for anything else in the file.
fn split(raw: &str, day: Option<Date>) -> Option<(String, Timestamp, String)> {
	let rest = raw.strip_prefix("- ")?;
	let (time, rest) = rest.split_once(' ')?;
	let slot = rest.strip_prefix('[')?;
	let (slot, text) = slot.split_once("] ").or_else(|| slot.strip_suffix(']').map(|s| (s, "")))?;
	let (handle, _) = slot.split_once('/')?;
	let at = day?
		.at(time.get(0..2)?.parse().ok()?, time.get(3..5)?.parse().ok()?, time.get(6..8)?.parse().ok()?, 0)
		.to_zoned(TimeZone::UTC)
		.expect("UTC has no gaps for a civil datetime to fall into")
		.timestamp();
	Some((handle.to_string(), at, text.to_string()))
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The whole of the "no parser" claim: a fixed prefix and the day heading over it, and everything
	/// after the slot is text — including a `]` a person typed.
	#[test]
	fn a_transcript_line_names_its_author() {
		let body = "# skool:20kmodrop — 2026 (times UTC)\n\
			 \n## 2026-03-04\n\n\
			 - 14:03:40 [lory/skool@20kmodrop] shipped it\n  \
			 and it works [really]\n\
			 - 14:05:02 [josh/skool@20kmodrop] nice\n\
			 \nsome prose nobody attributed\n\
			 \n## 2026-03-05\n\n\
			 - 09:00:00 [me/skool@20kmodrop] congrats\n";
		let lines = Line::read(body).unwrap();
		assert_eq!(lines.len(), 3);
		assert_eq!(lines[0].handle, "lory");
		assert_eq!(lines[0].text, "shipped it\nand it works [really]");
		assert_eq!(lines[0].at, "2026-03-04T14:03:40Z".parse::<Timestamp>().unwrap());
		assert_eq!(lines[1].handle, "josh");
		assert_eq!(lines[2].handle, "me");
		assert_eq!(lines[2].at, "2026-03-05T09:00:00Z".parse::<Timestamp>().unwrap());
	}

	/// The invariant the venue axis pays for: pulling one author must not reach another's lines.
	#[test]
	fn a_pull_reaches_only_its_own_author() {
		let body = "# skool:g — 2026 (times UTC)\n\n## 2026-03-04\n\n- 10:00:00 [lory/skool@g] mine\n- 11:00:00 [josh/skool@g] theirs\n";
		let lines = Line::read(body).unwrap();
		let mine: Vec<&Line> = lines.iter().filter(|l| l.handle == "lory").collect();
		assert_eq!(mine.len(), 1);
		assert_eq!(mine[0].text, "mine");
	}

	/// Two reads over one window must leave the same bytes, or every re-read grows the transcript and
	/// every `pull` after it re-reads what it already folded in.
	#[test]
	fn a_second_read_of_one_window_writes_nothing() {
		use social_networks_adapters::reach::{Author, Item, Kind, Source, VenueSource};

		let root = std::env::temp_dir().join("social_networks_venue_idempotence");
		let _ = std::fs::remove_dir_all(&root);
		let at = VenueRef::new(VenueSource::Skool, "g");
		let page = || Page {
			items: vec![
				Item {
					id: "a".to_string(),
					source: Source::Skool,
					at: "2026-03-04T10:00:00Z".parse().unwrap(),
					kind: Kind::Post,
					author: Author::Handle("lory".to_string()),
					text: "mine".to_string(),
					attachments: Vec::new(),
					permalink: None,
				},
				Item {
					id: "b".to_string(),
					source: Source::Skool,
					at: "2026-03-04T11:00:00Z".parse().unwrap(),
					kind: Kind::Post,
					author: Author::Handle("josh".to_string()),
					text: "theirs".to_string(),
					attachments: Vec::new(),
					permalink: None,
				},
			],
			newest: Some("b".to_string()),
			oldest: Some("a".to_string()),
			exhausted: true,
		};

		let mut store = Store::open(&root, &at).unwrap();
		assert_eq!(store.record(page()).unwrap(), 2);
		let first = std::fs::read_to_string(store.dir().join("2026.md")).unwrap();

		let mut store = Store::open(&root, &at).unwrap();
		assert_eq!(store.record(page()).unwrap(), 0, "the same window carries nothing new");
		assert_eq!(std::fs::read_to_string(store.dir().join("2026.md")).unwrap(), first);
		assert_eq!(store.cursor(), Some("b"));
		assert_eq!(all(&root).unwrap(), vec![at]);

		std::fs::remove_dir_all(&root).unwrap();
	}
}
