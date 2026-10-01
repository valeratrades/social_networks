use std::{
	collections::BTreeMap,
	path::{Path, PathBuf},
};

use color_eyre::eyre::{Result, WrapErr, bail};
use jiff::{Timestamp, civil::Date};
use serde::Deserialize;
use social_networks_adapters::reach::Place;

use crate::purpose::{Purpose, snake, snake_keys};

const MAIN: &str = "__main__.nix";

/// What a person's directory states about them, next to the conversation itself. `MAIN` is a
/// rendered view of this struct — `render` regenerates it whole, so comments and hand formatting
/// do not survive a `pull`.
///
/// `deny_unknown_fields` because the alternative is a misspelled or unwrapped attribute reading as
/// an empty person, which `pull` then reports as "nothing new" forever.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Person {
	/// Directory name. Not in the file itself.
	#[serde(skip)]
	pub name: String,
	/// What is said about them rather than what a platform says, typed by the purpose's vocabulary —
	/// a load refuses a tag it does not name, or a value of the wrong type. `None` is a judgement the
	/// extraction made and found nothing to support, which is not the same as one never asked for.
	#[serde(default, deserialize_with = "snake_keys")]
	pub tags: BTreeMap<String, Option<Value>>,
	/// Platform → handle. `discord`, `telegram`, `github` and `linkedin` are what `pull` knows how to
	/// fetch; the rest come from discord's connected accounts and are there for a human to read.
	#[serde(default)]
	pub handles: BTreeMap<String, String>,
	#[serde(default)]
	pub summary: String,
	#[serde(default)]
	pub log: Vec<LogEntry>,
	/// Verbatim platform-authored text (`discord:note`, `telegram:about`, …), keyed `platform:kind`.
	/// Diffing this against a fresh fetch is what decides whether there is anything to extract.
	#[serde(default)]
	pub sources: BTreeMap<String, String>,
	/// `<platform>:<slug>` per venue the platform says they are in, as of the last pull that reached
	/// a platform which states it. Replaced whole rather than merged: a membership that survived
	/// because nothing removed it is the bug this exists to catch.
	///
	/// `None` is "never asked", which is not the same answer as `Some([])` — a person no platform has
	/// been asked about must not read as one who has left everywhere.
	#[serde(default)]
	pub venues: Option<Vec<String>>,
	/// Platform → what it said when it refused to carry a message to them. Only ever written from an
	/// [`Unreachable`](social_networks_adapters::reach::Unreachable), so a network failure or an
	/// expired session cannot strand somebody here; cleared the moment a send to them lands.
	#[serde(default)]
	pub unreachable: BTreeMap<String, String>,
}
impl Person {
	pub fn skeleton(name: &str) -> Self {
		Self {
			name: name.to_string(),
			..Default::default()
		}
	}

	/// Their whole directory: `MAIN` and the conversation `history` keeps next to it.
	pub fn dir(&self, root: &Path) -> PathBuf {
		root.join(&self.name)
	}

	pub fn path(&self, root: &Path) -> PathBuf {
		self.dir(root).join(MAIN)
	}

	/// Match on the directory name and on every handle, so `pull dev_ardi` finds the person whose
	/// discord handle that is without anyone having to know what their directory is called. A bool tag
	/// that is `true`, and a `<group>:<value>`, match whole rather than by substring: a cohort that
	/// swallowed a name fragment is not a cohort.
	pub fn matches(&self, pattern: &str) -> bool {
		if let Some((group, value)) = pattern.split_once(':') {
			return matches!(self.tags.get(&snake(group)), Some(Some(Value::Text(w))) if w == value);
		}
		if self.tags.get(&snake(pattern)) == Some(&Some(Value::Bool(true))) {
			return true;
		}
		let pattern = pattern.to_lowercase();
		self.name.to_lowercase().contains(&pattern) || self.handles.values().any(|h| h.to_lowercase().contains(&pattern))
	}

	/// Puts `value` under `tag` unless what is there is better evidence: a birthday moves only to one
	/// that [supersedes](Birthday::supersedes) it, and a judgement of nothing never erases one.
	pub fn weigh(&mut self, tag: &str, value: Option<Value>) {
		match (self.tags.get(tag), &value) {
			(Some(Some(Value::Birthday(old))), Some(Value::Birthday(new))) if !new.supersedes(old) => {}
			(Some(Some(Value::Birthday(_))), None) => {}
			_ => {
				self.tags.insert(tag.to_string(), value);
			}
		}
	}

	/// Existing handles win: what a human typed outranks what discord's connected accounts guessed.
	pub fn absorb(&mut self, summary: String, new_log: Vec<LogEntry>, sources: BTreeMap<String, String>, handles: BTreeMap<String, String>) {
		self.summary = summary;
		self.log.extend(new_log);
		self.sources.extend(sources);
		for (platform, handle) in handles {
			self.handles.entry(platform).or_insert(handle);
		}
		self.normalize();
	}

	/// Replaced whole rather than merged: a membership that survived because nothing removed it is the
	/// bug this exists to catch. `None` — no platform that states membership answered this run —
	/// leaves the last one standing rather than overwriting it with silence.
	///
	/// Reports whether it moved, which no text delta can: leaving a venue adds no words and no items.
	pub fn set_venues(&mut self, venues: Option<Vec<String>>) -> bool {
		let Some(mut venues) = venues else { return false };
		venues.sort();
		venues.dedup();
		let moved = self.venues.as_ref() != Some(&venues);
		self.venues = Some(venues);
		moved
	}

	/// `''` blocks always end in a newline, so trailing whitespace cannot survive a write. Stripping
	/// it up front is what makes `render -> nix eval -> Person` an identity, and therefore what stops
	/// an unchanged `discord:note` from reading as changed on every pull.
	fn normalize(&mut self) {
		self.summary = self.summary.trim_end().to_string();
		for value in self.sources.values_mut().chain(self.unreachable.values_mut()) {
			*value = value.trim_end().to_string();
		}
	}

	pub fn write(&self, root: &Path) -> Result<()> {
		let dir = self.dir(root);
		std::fs::create_dir_all(&dir).wrap_err_with(|| format!("failed to create {}", dir.display()))?;
		let path = self.path(root);
		std::fs::write(&path, render(self)).wrap_err_with(|| format!("failed to write {}", path.display()))
	}
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LogEntry {
	pub date: String,
	pub text: String,
	/// Telegram DMs have no per-message URL, so this is absent for them.
	#[serde(default)]
	pub source: Option<String>,
}

/// A tag's value. Untyped here: which of these a tag may hold is the purpose's to say, and
/// [`Purpose::check`] says it.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Value {
	Bool(bool),
	Number(f64),
	Timestamp(Timestamp),
	/// After [`Self::Timestamp`], which no bare date parses as.
	Birthday(Birthday),
	/// A group's value, or free text. After [`Self::Timestamp`] and [`Self::Birthday`], which no bare word parses as.
	Text(String),
	Place {
		name: String,
		lat: f64,
		lon: f64,
	},
}
impl Value {
	pub fn nix(&self) -> String {
		match self {
			Self::Bool(b) => b.to_string(),
			Self::Number(n) => n.to_string(),
			Self::Timestamp(at) => nix_dq(&at.to_string()),
			Self::Text(text) => nix_dq(text),
			Self::Birthday(Birthday::Exact(date)) => nix_dq(&date.to_string()),
			Self::Birthday(Birthday::Rough { min, max, as_of }) => match as_of {
				Some(at) => format!("{{ min = {min}; max = {max}; as_of = {}; }}", nix_dq(&at.to_string())),
				None => format!("{{ min = {min}; max = {max}; }}"),
			},
			Self::Place { name, lat, lon } => format!("{{ name = {}; lat = {lat}; lon = {lon}; }}", nix_dq(name)),
		}
	}
}

/// When somebody was born, as precisely as anything has stated it. Stored rather than an age, so it
/// never goes stale: the age is derived at rank time.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Birthday {
	/// A platform states the date, year included.
	Exact(Date),
	/// Born in one of the years `min..=max` — `34` said in 2026 is `1991..=1992`. `as_of` is when it
	/// was said; `None` is a text that carries no date of its own, a bio or a note.
	Rough {
		min: i16,
		max: i16,
		#[serde(default)]
		as_of: Option<Date>,
	},
}
impl Birthday {
	/// The ages it puts them at on `today`, youngest first.
	pub fn ages(&self, today: Date) -> (i16, i16) {
		match self {
			Self::Exact(born) => {
				let age = born.until((jiff::Unit::Year, today)).expect("two civil dates are always a span apart").get_years();
				(age, age)
			}
			Self::Rough { min, max, .. } => (today.year() - max - 1, today.year() - min),
		}
	}

	/// An exact date outranks any rough one, and the latest exact date the one before it. Between two
	/// rough ones the newer statement wins, or the one that narrows the old range; an undated one only
	/// ever fills a gap.
	pub fn supersedes(&self, old: &Self) -> bool {
		match (self, old) {
			(Self::Exact(_), _) => true,
			(Self::Rough { .. }, Self::Exact(_)) => false,
			(
				Self::Rough { min, max, as_of },
				Self::Rough {
					min: old_min,
					max: old_max,
					as_of: old_as_of,
				},
			) => {
				let newer = match (as_of, old_as_of) {
					(Some(new), Some(old)) => new > old,
					(Some(_), None) => true,
					(None, _) => false,
				};
				let narrower = old_min <= min && max <= old_max && (min, max) != (old_min, old_max);
				newer || narrower
			}
		}
	}
}

impl From<Place> for Value {
	fn from(Place { name, lat, lon }: Place) -> Self {
		Self::Place { name, lat, lon }
	}
}

/// Evaluate every `<name>/``MAIN` under the purpose's path in one nix process, keyed by directory
/// name. Holding that file is what makes a directory a person's, so a stray one in there costs nothing.
///
/// A tag the purpose does not name, or a value of the wrong type, is refused here rather than read as
/// a cohort of one, for the same reason as `deny_unknown_fields` above.
pub fn load_dir(purpose: &Purpose) -> Result<BTreeMap<String, Person>> {
	let dir = &purpose.path;
	if !dir.exists() {
		return Ok(BTreeMap::new());
	}
	// `/. + <string>` rather than a bare path literal: a configured path may carry a trailing slash
	// or a space, neither of which a nix path literal accepts.
	let expr = format!(
		r#"let d = /. + {dir}; in builtins.listToAttrs (map (n: {{ name = n; value = import (d + "/${{n}}/{MAIN}"); }}) (builtins.filter (n: builtins.pathExists (d + "/${{n}}/{MAIN}")) (builtins.attrNames (builtins.readDir d))))"#,
		dir = nix_dq(&dir.display().to_string())
	);
	let raw: BTreeMap<String, Person> = serde_json::from_slice(&nix_eval(&["--expr", &expr])?).wrap_err_with(|| format!("a person file in {} is not a person", dir.display()))?;
	raw.into_iter()
		.map(|(name, mut person)| {
			person.name = name.clone();
			person.normalize();
			check_tags(&person, purpose)?;
			Ok((name, person))
		})
		.collect()
}

pub fn load_one(purpose: &Purpose, path: &Path) -> Result<Person> {
	let mut person: Person = serde_json::from_slice(&nix_eval(&["--file", &path.display().to_string()])?).wrap_err_with(|| format!("{} is not a person", path.display()))?;
	person.name = path
		.parent()
		.expect("a path we built under a person directory")
		.file_name()
		.expect("the person directory is named after them")
		.to_string_lossy()
		.into_owned();
	person.normalize();
	check_tags(&person, purpose)?;
	Ok(person)
}

fn render(person: &Person) -> String {
	let mut s = String::from("{\n");

	if !person.tags.is_empty() {
		s.push_str("  tags = {\n");
		for (tag, value) in &person.tags {
			s.push_str(&format!("    {} = {};\n", nix_attr(tag), value.as_ref().map_or_else(|| "null".to_string(), Value::nix)));
		}
		s.push_str("  };\n");
	}

	s.push_str("  handles = {\n");
	for (platform, handle) in &person.handles {
		s.push_str(&format!("    {} = {};\n", nix_attr(platform), nix_dq(handle)));
	}
	s.push_str("  };\n");

	s.push_str(&format!("  summary = {};\n", nix_str(&person.summary, 2)));

	s.push_str("  log = [\n");
	for entry in &person.log {
		s.push_str(&format!("    {{ date = {}; text = {};", nix_dq(&entry.date), nix_dq(&entry.text)));
		if let Some(source) = &entry.source {
			s.push_str(&format!(" source = {};", nix_dq(source)));
		}
		s.push_str(" }\n");
	}
	s.push_str("  ];\n");

	s.push_str("  sources = {\n");
	for (key, value) in &person.sources {
		s.push_str(&format!("    {} = {};\n", nix_attr(key), nix_str(value, 4)));
	}
	s.push_str("  };\n");

	if !person.unreachable.is_empty() {
		s.push_str("  unreachable = {\n");
		for (platform, reason) in &person.unreachable {
			s.push_str(&format!("    {} = {};\n", nix_attr(platform), nix_str(reason, 4)));
		}
		s.push_str("  };\n");
	}

	// absent rather than empty when nobody has been asked, which is what `Option` is carrying here
	if let Some(venues) = &person.venues {
		s.push_str("  venues = [\n");
		for venue in venues {
			s.push_str(&format!("    {}\n", nix_dq(venue)));
		}
		s.push_str("  ];\n");
	}

	s.push_str("}\n");
	s
}
fn check_tags(person: &Person, purpose: &Purpose) -> Result<()> {
	for (tag, value) in &person.tags {
		purpose.check(tag, value.as_ref()).wrap_err_with(|| format!("{} in {}", person.name, purpose.path.display()))?;
	}
	Ok(())
}

/// `--impure` because person files live outside the store, which pure eval forbids.
fn nix_eval(args: &[&str]) -> Result<Vec<u8>> {
	let out = std::process::Command::new("nix")
		.args(["eval", "--impure", "--json"])
		.args(args)
		.output()
		.wrap_err("failed to run `nix eval`")?;
	if !out.status.success() {
		bail!("nix eval failed:\n{}", String::from_utf8_lossy(&out.stderr));
	}
	Ok(out.stdout)
}

/// An indented block whenever nix's indentation stripping cannot lose anything — a line that starts
/// with whitespace would raise the computed minimum indentation and get silently outdented.
fn nix_str(value: &str, indent: usize) -> String {
	let value = value.trim_end();
	if !value.contains('\n') || value.lines().any(|l| l.starts_with([' ', '\t'])) {
		return nix_dq(value);
	}
	let pad = " ".repeat(indent + 2);
	let body: String = value.lines().map(|l| format!("{pad}{}\n", l.replace("''", "'''").replace("${", "''${"))).collect();
	format!("''\n{body}{}''", " ".repeat(indent))
}

fn nix_dq(value: &str) -> String {
	format!(
		"\"{}\"",
		value.replace('\\', "\\\\").replace('"', "\\\"").replace("${", "\\${").replace('\n', "\\n").replace('\t', "\\t")
	)
}

fn nix_attr(name: &str) -> String {
	let bare = !name.is_empty() && !name.starts_with(|c: char| c.is_ascii_digit()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
	if bare { name.to_string() } else { nix_dq(name) }
}

#[cfg(test)]
mod tests {
	use std::collections::BTreeMap;

	use super::*;
	use crate::purpose::Purposes;

	/// The file is the storage format, so anything `render` writes must come back identical through
	/// nix — quoting, escaping and the indented-block trailing newline included.
	///
	/// `load_one` shells out to a real evaluator, which the CI runner does not have and the nix
	/// build sandbox cannot provide to itself. Gating beats deleting the only check on the format.
	#[test]
	fn render_survives_nix() {
		if std::process::Command::new("nix").arg("--version").output().is_err() {
			eprintln!("render_survives_nix: skipped, no `nix` on PATH");
			return;
		}

		let dir = std::env::temp_dir().join("social_networks_rolodex_render_test");
		let purpose = |tags: serde_json::Value| -> Purpose {
			let purposes: Purposes =
				serde_json::from_value(serde_json::json!({ "t": { "path": dir, "tags": tags, "rank": [{ "of": "interactions", "weight": 1 }], "half_life": "30d" } })).unwrap();
			purposes.get("t").unwrap().clone()
		};
		let tags = serde_json::json!({
			"ServiceArb": { "type": "bool" },
			"Rust": { "type": "bool" },
			"interest": { "type": "number", "min": -1, "max": 1 },
			"judged": { "type": "number", "min": 0, "max": 1, "about": "left unjudged" },
			"birthday": { "type": "birthday" }, "born": { "type": "birthday" },
			"lives_in": { "type": "place" },
			"last_login": { "type": "timestamp" },
			"location": ["lyon", "paris"],
		});
		let vocabulary = purpose(tags.clone());
		let person = Person {
			name: "ardi".to_string(),
			tags: BTreeMap::from([
				("service_arb".to_string(), Some(Value::Bool(true))),
				("rust".to_string(), Some(Value::Bool(false))),
				("interest".to_string(), Some(Value::Number(-0.25))),
				("judged".to_string(), None),
				(
					"birthday".to_string(),
					Some(Value::Birthday(Birthday::Rough {
						min: 1990,
						max: 1991,
						as_of: Some("2026-03-04".parse().unwrap()),
					})),
				),
				("born".to_string(), Some(Value::Birthday(Birthday::Exact("2002-09-25".parse().unwrap())))),
				(
					"lives_in".to_string(),
					Some(Value::Place {
						name: "São \"Paulo\"".to_string(),
						lat: -23.55,
						lon: -46.63,
					}),
				),
				("last_login".to_string(), Some(Value::Timestamp("2026-09-01T12:30:00Z".parse().unwrap()))),
				("location".to_string(), Some(Value::Text("lyon".to_string()))),
			]),
			handles: BTreeMap::from([("discord".to_string(), "dev_ardi".to_string()), ("telegram".to_string(), "deevsdeevs".to_string())]),
			summary: "Rust dev. Crab guy.\n\nWrites \"exchange adapters\".".to_string(),
			log: vec![
				LogEntry {
					date: "2026-03-04".to_string(),
					text: "Shipped v1 of his exchange adapter".to_string(),
					source: Some("https://discord.com/channels/@me/118/150".to_string()),
				},
				LogEntry {
					date: "2026-03-05".to_string(),
					text: "Moved to ${HOME}\\tmp".to_string(),
					source: None,
				},
			],
			sources: BTreeMap::from([
				("discord:note".to_string(), "Orion Gonzales, ~25yo".to_string()),
				("discord:bio".to_string(), "Failure is not an option, it's a `Result<T, E>`".to_string()),
				("telegram:about".to_string(), "lol. 🧉. jenat.\n  indented second line".to_string()),
			]),
			venues: Some(vec!["skool:20kmodrop".to_string(), "telegram:some/chat".to_string()]),
			unreachable: BTreeMap::from([("skool".to_string(), "no group of mine opens a chat with them:\n400: not a member".to_string())]),
		};

		let _ = std::fs::remove_dir_all(&dir);
		person.write(&dir).unwrap();
		assert_eq!(load_one(&vocabulary, &person.path(&dir)).unwrap(), person);
		// what the vocabulary is for: the same file, against a config that never named the tag
		assert!(load_one(&purpose(serde_json::json!({})), &person.path(&dir)).is_err());
		// and against one that types it differently
		assert!(load_one(&purpose(serde_json::json!({ "ServiceArb": { "type": "bool" }, "Rust": { "type": "bool" }, "interest": { "type": "number", "min": 0, "max": 1 }, "judged": { "type": "number", "min": 0, "max": 1, "about": "x" }, "birthday": { "type": "birthday" }, "born": { "type": "birthday" }, "lives_in": { "type": "place" }, "last_login": { "type": "timestamp" } })), &person.path(&dir)).is_err());
		// and against one whose group lacks their value
		let mut elsewhere = tags;
		elsewhere["location"] = serde_json::json!(["paris", "berlin"]);
		assert!(load_one(&purpose(elsewhere), &person.path(&dir)).is_err());
		std::fs::remove_dir_all(&dir).unwrap();
	}
}
