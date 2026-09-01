//! The durable half of a pull: the conversation itself, next to the person file rather than derived
//! from it. Labels in `<person>.nix` are what an LLM made of these; these are what happened.
//!
//! ```text
//! BACKFILLING                                 STEADY
//! every message → cache jsonl                 every new message → append <year>.md
//! no year files yet                           the cache is gone
//! meta saved per page: an interrupt costs one append-only: no parser, no rewrite
//!      └──── all sources backfill_done ────► render the year files once, drop the cache ────┘
//! ```
//!
//! A backfill walks backwards, so its output is older than that source's own slice but not
//! necessarily older than another source's. Holding every year file back until the last source is
//! done is what keeps each of them a single whole-file write.

use std::{
	collections::{BTreeMap, HashSet},
	io::Write as _,
	path::{Path, PathBuf},
};

use color_eyre::eyre::{Result, WrapErr, eyre};
use jiff::{Timestamp, tz::TimeZone};
use serde::{Deserialize, Serialize};
use strum::IntoEnumIterator as _;
use tracing::warn;

use super::sources::{Attachment, Msg, Source};

/// `<person>/meta.json`. Every "where did we stop" the rolodex holds, in one place a full
/// regeneration of the person file never touches.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Meta {
	#[serde(skip)]
	dir: PathBuf,
	sources: BTreeMap<String, SourceMeta>,
}
impl Meta {
	pub fn load(person_dir: &Path) -> Result<Self> {
		let path = person_dir.join("meta.json");
		let mut meta = match std::fs::read(&path) {
			Ok(bytes) => serde_json::from_slice(&bytes).wrap_err_with(|| format!("{} is not rolodex history state", path.display()))?,
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
			Err(e) => return Err(e).wrap_err_with(|| format!("failed to read {}", path.display())),
		};
		meta.dir = person_dir.to_path_buf();
		Ok(meta)
	}

	/// Through a temporary, so a kill during the write leaves the previous state rather than half of
	/// this one.
	fn save(&self) -> Result<()> {
		std::fs::create_dir_all(&self.dir).wrap_err_with(|| format!("failed to create {}", self.dir.display()))?;
		let path = self.dir.join("meta.json");
		let tmp = self.dir.join("meta.json.tmp");
		std::fs::write(&tmp, serde_json::to_vec_pretty(self)?).wrap_err_with(|| format!("failed to write {}", tmp.display()))?;
		std::fs::rename(&tmp, &path).wrap_err_with(|| format!("failed to replace {}", path.display()))
	}

	/// Where `source` stopped, and the scratch its backfill checkpoints into.
	pub fn cursor(&mut self, source: Source) -> Result<Cursor<'_>> {
		let person = self.person().to_string();
		let cache = cache_dir(&person)?.join(format!("{}.jsonl", source.as_ref()));
		// A source first seen on a person whose archive is already rendered cannot be backfilled into
		// it: the year files are written whole, once, and merging a second pass into them would take
		// the parser this design does without. It tails forward from now instead.
		let rendered = has_year_files(&self.dir)?;
		let fresh = !self.sources.contains_key(source.as_ref());
		if fresh && rendered && source.has_history() {
			warn!(
				"{person}: {} joined an archive that is already rendered, so only its new messages are kept — remove {} and pull again to rebuild the whole of it",
				source.as_ref(),
				self.dir.display()
			);
		}
		let state = self.sources.entry(source.as_ref().to_string()).or_insert_with(|| SourceMeta {
			backfill_done: !source.has_history() || rendered,
			..Default::default()
		});
		// A cleaner took the scratch out from under a running backfill. Continuing below `oldest`
		// would leave everything above it in no file at all, so the walk starts over.
		if !state.backfill_done && state.oldest.is_some() && !cache.exists() {
			warn!("{person}: the {} backfill cache is gone, restarting that backfill from the newest message", source.as_ref());
			state.oldest = None;
			state.messages = 0;
		}
		Ok(Cursor {
			platform: source.as_ref().to_string(),
			cache,
			meta: self,
		})
	}

	/// `None` once every source has reached the end of its history.
	pub fn backfill_status(&self) -> Option<String> {
		let pending: Vec<(&str, usize)> = self.sources.iter().filter(|(_, s)| !s.backfill_done).map(|(name, s)| (name.as_str(), s.messages)).collect();
		(!pending.is_empty()).then(|| {
			format!(
				"backfilling {}, {} msgs",
				pending.iter().map(|(name, _)| *name).collect::<Vec<_>>().join(" + "),
				pending.iter().map(|(_, n)| n).sum::<usize>()
			)
		})
	}

	fn person(&self) -> &str {
		self.dir
			.file_name()
			.expect("a person directory is <rolodex dir>/<name>")
			.to_str()
			.expect("a person name came out of a utf-8 file stem")
	}

	fn backfilling(&self) -> bool {
		self.sources.values().any(|s| !s.backfill_done)
	}

	fn last_message(&self) -> Option<Timestamp> {
		self.sources.values().filter_map(|s| s.last_message).max()
	}

	fn absorb(&mut self, msgs: &[Msg]) {
		for source in Source::iter() {
			let of_source: Vec<&Msg> = msgs.iter().filter(|m| m.source == source).collect();
			if !of_source.is_empty() {
				self.sources.entry(source.as_ref().to_string()).or_default().absorb(&of_source);
			}
		}
	}
}

/// The half of [`Meta`] a fetch touches: where to resume, and where to put a page so the next run
/// does not have to ask for it again.
pub struct Cursor<'a> {
	meta: &'a mut Meta,
	platform: String,
	cache: PathBuf,
}
impl Cursor<'_> {
	/// The newest item already seen. Opaque: a discord snowflake, a telegram message id, a date.
	pub fn newest(&self) -> Option<&str> {
		self.state().newest.as_deref()
	}

	/// The floor a backfill continues below. `None` when it has yet to take its first page.
	pub fn floor(&self) -> Option<&str> {
		self.state().oldest.as_deref()
	}

	/// Whether *this* source still has history below it to walk down to.
	pub fn backfilling(&self) -> bool {
		!self.state().backfill_done
	}

	/// Whether the person has no year files yet, and so everything fetched belongs in the cache. Not
	/// the same question as [`Cursor::backfilling`]: one source can be finished while another is not,
	/// and until the last one is, nothing is rendered.
	pub fn archiving(&self) -> bool {
		self.meta.backfilling()
	}

	/// The slice fetched above the cursor, checked in the moment it is fetched. Leaves the backfill
	/// floor alone: this sits above it, not below.
	pub fn stash(&mut self, msgs: &[Msg]) -> Result<()> {
		if msgs.is_empty() {
			return Ok(());
		}
		let refs: Vec<&Msg> = msgs.iter().collect();
		append_jsonl(&self.cache, &refs)?;
		self.state_mut().absorb(&refs);
		self.meta.save()
	}

	pub fn advance(&mut self, newest: String) {
		self.state_mut().newest = Some(newest);
	}

	/// One page below [`Cursor::floor`], checked in before the fetch asks for the next one.
	pub fn page(&mut self, msgs: &[Msg], floor: String) -> Result<()> {
		let refs: Vec<&Msg> = msgs.iter().collect();
		append_jsonl(&self.cache, &refs)?;
		let state = self.state_mut();
		state.oldest = Some(floor);
		state.absorb(&refs);
		self.meta.save()
	}

	/// There is nothing below the last page.
	pub fn exhausted(&mut self) -> Result<()> {
		self.state_mut().backfill_done = true;
		self.meta.save()
	}

	fn state(&self) -> &SourceMeta {
		self.meta.sources.get(&self.platform).expect("`cursor` inserts the entry it hands out")
	}

	fn state_mut(&mut self) -> &mut SourceMeta {
		self.meta.sources.get_mut(&self.platform).expect("`cursor` inserts the entry it hands out")
	}
}

/// Steady state: append to the year files. Backfilling: stash to the cache. Renders the year files
/// once and drops the cache when the last source has finished.
pub fn record(person_dir: &Path, mut msgs: Vec<Msg>, meta: &mut Meta) -> Result<()> {
	msgs.sort_by(order);
	if meta.backfilling() {
		// every fetch checked its own slice into the cache as it went
	} else if cache_dir(meta.person())?.exists() {
		// the last backfill finished during this pull, so the whole archive lands in one pass
		render(person_dir, meta)?;
	} else {
		append(person_dir, &msgs, meta.last_message())?;
		meta.absorb(&msgs);
	}
	meta.save()
}
/// `newest` is the cursor the incremental fetch resumes above; `oldest` the floor a backfill
/// continues below. `last_message` orders two messengers against each other, which a date alone
/// cannot.
#[derive(Debug, Default, Deserialize, Serialize)]
struct SourceMeta {
	newest: Option<String>,
	oldest: Option<String>,
	backfill_done: bool,
	messages: usize,
	last_message: Option<Timestamp>,
}

impl SourceMeta {
	fn absorb(&mut self, msgs: &[&Msg]) {
		self.messages += msgs.len();
		self.last_message = msgs.iter().map(|m| m.at).max().max(self.last_message);
	}
}

/// Every cached page, deduplicated by id — a crash between "page appended" and "meta saved" replays
/// one page on the next run — and written out through the same appender the steady state uses, so
/// an interrupted backfill and an uninterrupted one cannot render differently.
fn render(person_dir: &Path, meta: &Meta) -> Result<()> {
	let cache = cache_dir(meta.person())?;
	let mut msgs = Vec::new();
	let mut seen = HashSet::new();
	for source in Source::iter() {
		let path = cache.join(format!("{}.jsonl", source.as_ref()));
		let body = match std::fs::read_to_string(&path) {
			Ok(body) => body,
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
			Err(e) => return Err(e).wrap_err_with(|| format!("failed to read {}", path.display())),
		};
		for line in body.lines().filter(|l| !l.trim().is_empty()) {
			let msg: Msg = serde_json::from_str(line).wrap_err_with(|| format!("{} holds a line that is not a message", path.display()))?;
			if seen.insert((msg.source, msg.id.clone())) {
				msgs.push(msg);
			}
		}
	}
	msgs.sort_by(order);

	append(person_dir, &msgs, None)?;
	std::fs::remove_dir_all(&cache).wrap_err_with(|| format!("failed to remove {}", cache.display()))
}

/// `last` is the newest message already in the files, and decides whether the first message here
/// opens a new day. Messages must be [`order`]ed.
fn append(person_dir: &Path, msgs: &[Msg], last: Option<Timestamp>) -> Result<()> {
	if msgs.is_empty() {
		return Ok(());
	}
	std::fs::create_dir_all(person_dir).wrap_err_with(|| format!("failed to create {}", person_dir.display()))?;
	let person = person_dir.file_name().expect("a person directory is <rolodex dir>/<name>").to_string_lossy().into_owned();

	let mut day = last.map(|t| t.to_zoned(TimeZone::UTC).date());
	let mut open: Option<(i16, std::fs::File)> = None;
	for msg in msgs {
		let zoned = msg.at.to_zoned(TimeZone::UTC);
		let (year, date) = (zoned.year(), zoned.date());
		if open.as_ref().map(|(y, _)| *y) != Some(year) {
			let path = person_dir.join(format!("{year}.md"));
			let fresh = !path.exists();
			let mut file = std::fs::OpenOptions::new()
				.create(true)
				.append(true)
				.open(&path)
				.wrap_err_with(|| format!("failed to open {}", path.display()))?;
			if fresh {
				writeln!(file, "# {person} — {year} (times UTC)")?;
				day = None;
			}
			open = Some((year, file));
		}
		let file = &mut open.as_mut().expect("just opened").1;
		if day != Some(date) {
			write!(file, "\n## {date}\n\n")?;
			day = Some(date);
		}
		writeln!(file, "{}", line(&person, msg))?;
	}
	Ok(())
}

fn line(person: &str, msg: &Msg) -> String {
	let who = if msg.outgoing { "me" } else { person };
	let time = msg.at.to_zoned(TimeZone::UTC).time();
	let mut text = msg.text.trim().to_string();
	for name in msg.attachments.iter().filter_map(|a| match a {
		Attachment::File { name } => Some(name),
		Attachment::Image { .. } => None,
	}) {
		text.push_str(&format!(" [{name}]"));
	}

	// a continuation line has to stay indented under the list item, or it closes it
	let mut out = format!(
		"- {:02}:{:02}:{:02} [{who}/{}] {}",
		time.hour(),
		time.minute(),
		time.second(),
		msg.source.as_ref(),
		text.trim().replace('\n', "\n  ")
	);
	out.truncate(out.trim_end().len());
	for file in msg.attachments.iter().filter_map(|a| match a {
		Attachment::Image { file } => Some(file),
		Attachment::File { .. } => None,
	}) {
		out.push_str(&format!("\n  ![](assets/{file})"));
	}
	out
}

/// Total, and independent of the order sources were fetched in: two runs that saw the same messages
/// must write the same bytes.
fn order(a: &Msg, b: &Msg) -> std::cmp::Ordering {
	(a.at, a.source.as_ref(), &a.id).cmp(&(b.at, b.source.as_ref(), &b.id))
}

fn append_jsonl(path: &Path, msgs: &[&Msg]) -> Result<()> {
	let parent = path.parent().expect("a cache path carries a directory");
	std::fs::create_dir_all(parent).wrap_err_with(|| format!("failed to create {}", parent.display()))?;
	let mut file = std::fs::OpenOptions::new()
		.create(true)
		.append(true)
		.open(path)
		.wrap_err_with(|| format!("failed to open {}", path.display()))?;
	let mut body = String::new();
	for msg in msgs {
		body.push_str(&serde_json::to_string(msg)?);
		body.push('\n');
	}
	file.write_all(body.as_bytes()).wrap_err_with(|| format!("failed to write {}", path.display()))
}

/// A rendered archive is one no backfill may write into again — see [`Meta::cursor`].
fn has_year_files(person_dir: &Path) -> Result<bool> {
	let entries = match std::fs::read_dir(person_dir) {
		Ok(entries) => entries,
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
		Err(e) => return Err(e).wrap_err_with(|| format!("failed to read {}", person_dir.display())),
	};
	for entry in entries {
		if entry?.path().extension().is_some_and(|e| e == "md") {
			return Ok(true);
		}
	}
	Ok(false)
}

/// Transient by construction: it holds pages a backfill has not turned into year files yet, and is
/// removed the moment it has.
fn cache_dir(person: &str) -> Result<PathBuf> {
	let home = xdg::BaseDirectories::with_prefix("social_networks")
		.get_cache_home()
		.ok_or_else(|| eyre!("no XDG cache home to back a rolodex backfill with"))?;
	Ok(home.join("rolodex").join(person))
}

#[cfg(test)]
mod tests {
	use std::{
		collections::BTreeMap,
		path::{Path, PathBuf},
	};

	use super::*;

	fn msg(source: Source, id: &str, at: &str, outgoing: bool, text: &str) -> Msg {
		Msg {
			id: id.to_string(),
			source,
			at: at.parse().expect("a test timestamp"),
			outgoing,
			text: text.to_string(),
			attachments: Vec::new(),
			permalink: None,
		}
	}

	/// A conversation across two sources and a year boundary, in the order a backwards walk pages it.
	fn pages() -> Vec<Vec<Msg>> {
		vec![
			vec![
				msg(Source::Discord, "40", "2026-01-04T09:00:00Z", false, "and back"),
				msg(Source::Telegram, "41", "2026-01-03T22:10:33Z", false, "can't sleep"),
			],
			vec![
				msg(Source::Discord, "30", "2025-12-31T23:59:00Z", true, "happy new year"),
				msg(Source::Telegram, "31", "2025-12-31T10:00:00Z", false, "same day, other messenger"),
			],
			vec![
				msg(Source::Discord, "20", "2025-06-02T12:00:00Z", false, "line one\nline two"),
				msg(Source::Discord, "10", "2025-06-01T08:30:00Z", true, "hey"),
			],
		]
	}

	fn check_in(meta: &mut Meta, page: &[Msg]) -> Result<()> {
		for source in [Source::Discord, Source::Telegram] {
			let of_source: Vec<Msg> = page.iter().filter(|m| m.source == source).cloned().collect();
			if !of_source.is_empty() {
				let floor = of_source.iter().map(|m| m.id.clone()).min().expect("a non-empty page");
				meta.cursor(source)?.page(&of_source, floor)?;
			}
		}
		Ok(())
	}

	fn finish(dir: &Path, meta: &mut Meta) -> Result<()> {
		for source in [Source::Discord, Source::Telegram] {
			meta.cursor(source)?.exhausted()?;
		}
		record(dir, Vec::new(), meta)
	}

	fn archive(dir: &Path) -> BTreeMap<String, String> {
		std::fs::read_dir(dir)
			.expect("the person directory")
			.filter_map(|e| {
				let path = e.expect("readable entry").path();
				(path.extension()? == "md").then(|| {
					(
						path.file_name().expect("a file we just listed").to_string_lossy().into_owned(),
						std::fs::read_to_string(&path).expect("readable year file"),
					)
				})
			})
			.collect()
	}

	fn scratch(name: &str) -> PathBuf {
		let dir = std::env::temp_dir().join("social_networks_rolodex_history").join(name);
		let _ = std::fs::remove_dir_all(&dir);
		let _ = std::fs::remove_dir_all(cache_dir(name).expect("an XDG cache home"));
		dir
	}

	/// The invariant the two-state design rests on: what the year files hold cannot depend on how many
	/// times the backfill that filled them was killed and resumed.
	#[test]
	fn interrupted_backfill_renders_the_same_archive() {
		let straight = scratch("orion");
		let mut meta = Meta::load(&straight).unwrap();
		for page in pages() {
			check_in(&mut meta, &page).unwrap();
		}
		finish(&straight, &mut meta).unwrap();
		let expected = archive(&straight);

		let resumed = scratch("orion");
		let mut meta = Meta::load(&resumed).unwrap();
		check_in(&mut meta, &pages()[0]).unwrap();
		check_in(&mut meta, &pages()[1]).unwrap();
		// killed between the page landing in the cache and the floor that follows it reaching disk
		let last = pages().remove(2);
		append_jsonl(&cache_dir("orion").unwrap().join("discord.jsonl"), &last.iter().collect::<Vec<_>>()).unwrap();
		assert!(archive(&resumed).is_empty(), "a backfill in flight writes no year file");

		let mut meta = Meta::load(&resumed).unwrap();
		assert_eq!(meta.cursor(Source::Discord).unwrap().floor(), Some("30"), "the resume picks the page up again");
		check_in(&mut meta, &pages()[2]).unwrap();
		finish(&resumed, &mut meta).unwrap();

		assert_eq!(archive(&resumed), expected);
		assert!(!cache_dir("orion").unwrap().exists(), "the cache goes when the archive lands");

		assert_eq!(
			expected.get("2025.md").expect("a 2025 file"),
			"# orion — 2025 (times UTC)\n\
			 \n## 2025-06-01\n\n\
			 - 08:30:00 [me/discord] hey\n\
			 \n## 2025-06-02\n\n\
			 - 12:00:00 [orion/discord] line one\n  line two\n\
			 \n## 2025-12-31\n\n\
			 - 10:00:00 [orion/telegram] same day, other messenger\n\
			 - 23:59:00 [me/discord] happy new year\n"
		);
		assert_eq!(
			expected.get("2026.md").expect("a 2026 file"),
			"# orion — 2026 (times UTC)\n\
			 \n## 2026-01-03\n\n\
			 - 22:10:33 [orion/telegram] can't sleep\n\
			 \n## 2026-01-04\n\n\
			 - 09:00:00 [orion/discord] and back\n"
		);

		std::fs::remove_dir_all(&resumed).unwrap();
	}

	/// The steady state continues the file the backfill wrote rather than restarting it, and only
	/// opens a day heading the archive does not already stand under.
	#[test]
	fn steady_state_appends_under_the_open_day() {
		let dir = scratch("ardi");
		let mut meta = Meta::load(&dir).unwrap();
		check_in(&mut meta, &[msg(Source::Discord, "10", "2026-03-04T14:02:11Z", true, "hey")]).unwrap();
		finish(&dir, &mut meta).unwrap();

		let mut meta = Meta::load(&dir).unwrap();
		record(
			&dir,
			vec![
				msg(Source::Discord, "11", "2026-03-04T14:03:40Z", false, "yeah, v1 is out"),
				msg(Source::Discord, "12", "2026-03-05T09:00:00Z", false, "morning"),
			],
			&mut meta,
		)
		.unwrap();

		assert_eq!(
			archive(&dir).get("2026.md").expect("a 2026 file"),
			"# ardi — 2026 (times UTC)\n\
			 \n## 2026-03-04\n\n\
			 - 14:02:11 [me/discord] hey\n\
			 - 14:03:40 [ardi/discord] yeah, v1 is out\n\
			 \n## 2026-03-05\n\n\
			 - 09:00:00 [ardi/discord] morning\n"
		);
		std::fs::remove_dir_all(&dir).unwrap();
	}

	/// Rendering is what a year file gets, once. A source that shows up afterwards must not start a
	/// backfill, because the render that would end it rebuilds the files from a cache holding only
	/// what that one source saw.
	#[test]
	fn a_late_source_cannot_rewrite_a_rendered_archive() {
		let dir = scratch("mel");
		let mut meta = Meta::load(&dir).unwrap();
		check_in(&mut meta, &[msg(Source::Discord, "10", "2026-03-04T14:02:11Z", true, "hey")]).unwrap();
		finish(&dir, &mut meta).unwrap();
		let rendered = archive(&dir);

		let mut meta = Meta::load(&dir).unwrap();
		let telegram = meta.cursor(Source::Telegram).unwrap();
		assert!(!telegram.backfilling(), "a late source tails forward instead");
		assert!(!telegram.archiving());
		record(&dir, vec![msg(Source::Telegram, "1", "2026-03-06T08:00:00Z", false, "hi from the new one")], &mut meta).unwrap();

		assert_eq!(
			archive(&dir).get("2026.md").expect("a 2026 file"),
			&format!(
				"{}\n## 2026-03-06\n\n- 08:00:00 [mel/telegram] hi from the new one\n",
				rendered.get("2026.md").expect("a 2026 file")
			)
		);
		std::fs::remove_dir_all(&dir).unwrap();
	}

	/// An attachment-only message is still a message: the filename carries it, and an image hangs
	/// off a continuation line rather than closing the list item.
	#[test]
	fn attachments_reach_the_transcript() {
		let mut image = msg(Source::Discord, "1", "2026-03-04T14:03:40Z", false, "yeah, v1 is out");
		image.attachments = vec![Attachment::Image {
			file: "discord-1349938102838738944.avif".to_string(),
		}];
		assert_eq!(
			line("orion", &image),
			"- 14:03:40 [orion/discord] yeah, v1 is out\n  ![](assets/discord-1349938102838738944.avif)"
		);

		let mut file = msg(Source::Discord, "2", "2026-03-04T14:05:02Z", false, "");
		file.attachments = vec![Attachment::File {
			name: "adapter_bench.csv".to_string(),
		}];
		assert_eq!(line("orion", &file), "- 14:05:02 [orion/discord] [adapter_bench.csv]");
	}
}
