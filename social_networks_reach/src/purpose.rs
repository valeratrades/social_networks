//! What the people in a folder are *for*: where they live, how they get there, what may be said about
//! them, and how they are ordered. The store, the transcripts and the outreach are the same for every
//! purpose; this is the whole of what differs. See `social_networks/src/purpose/README.md`.

use std::{
	collections::{BTreeMap, BTreeSet},
	ops::Range,
	path::PathBuf,
};

use color_eyre::eyre::{Report, Result, WrapErr, bail, eyre};
use jiff::Timestamp;
use serde::{Deserialize, Deserializer, de::Error as _};
use social_networks_adapters::reach::VenueRef;
use v_utils::HalfLife;

use crate::{
	person::{Birthday, Value},
	venue,
};

/// Where a platform says somebody lives.
pub const LIVES_IN: &str = "lives_in";
/// When a platform says somebody was born, or what they said their age was.
pub const BIRTHDAY: &str = "birthday";
/// Signals every purpose has without declaring them, derived at rank time from the transcripts.
const BUILTINS: [&str; 3] = ["interactions", "last_interaction", "venue_activity"];
/// Tags a platform states rather than anybody judging them, which `pull` writes from what a platform
/// answered. A purpose opts into one by declaring a tag of that name, of that type.
const FACTS: [(&str, TagType); 2] = [(LIVES_IN, TagType::Place), (BIRTHDAY, TagType::Birthday { about: None })];

/// `purposes` in the config, keyed by the name the CLI addresses a purpose by. Checked whole at load,
/// so no command ever holds a purpose whose ranking or procurement names what its vocabulary does not.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(try_from = "BTreeMap<String, RawPurpose>")]
pub struct Purposes(BTreeMap<String, Purpose>);
impl Purposes {
	pub fn get(&self, name: &str) -> Result<&Purpose> {
		self.0
			.get(name)
			.ok_or_else(|| eyre!("no `purposes.{name}` in the config; it names {}", self.0.keys().cloned().collect::<Vec<_>>().join(", ")))
	}

	pub fn iter(&self) -> impl Iterator<Item = &Purpose> {
		self.0.values()
	}
}
impl TryFrom<BTreeMap<String, RawPurpose>> for Purposes {
	/// The whole chain on one line, since serde keeps only the `Display` of what it is handed
	type Error = String;

	fn try_from(raw: BTreeMap<String, RawPurpose>) -> std::result::Result<Self, String> {
		raw.into_iter()
			.map(|(name, raw)| Ok((name.clone(), Purpose::try_new(name.clone(), raw).wrap_err_with(|| format!("`purposes.{name}`"))?)))
			.collect::<Result<_>>()
			.map(Self)
			.map_err(|e: Report| format!("{e:#}"))
	}
}

#[derive(Clone, Debug)]
pub struct Purpose {
	pub name: String,
	/// Its children are person directories.
	pub path: PathBuf,
	/// The whole vocabulary: a tag a person carries and this does not name is a misspelling, and a
	/// load refuses it rather than inventing a cohort of one.
	pub tags: BTreeMap<String, TagType>,
	pub procure: BTreeMap<String, Strategy>,
	/// Never empty: a purpose is kept in order to be ranked.
	pub rank: Vec<Term>,
	/// How fast what a pull refreshes goes out of date; what `stale` in a [ranking](crate::rank) is measured by.
	pub stale_half_life: HalfLife,
	/// How fast the penalty on a lead whose last line is ours wears off; see [`crate::rank`].
	pub unanswered_half_life: HalfLife,
}
impl Purpose {
	fn try_new(name: String, raw: RawPurpose) -> Result<Self> {
		let tags: BTreeMap<String, TagType> = raw.tags.into_iter().map(|(tag, raw)| Ok((tag.clone(), raw.typed(&tag)?))).collect::<Result<_>>()?;
		if let Some(tag) = tags.keys().find(|tag| BUILTINS.contains(&tag.as_str())) {
			bail!("`{tag}` is both a tag and a builtin signal");
		}
		for (fact, kind) in &FACTS {
			if let Some(declared) = tags.get(*fact)
				&& std::mem::discriminant(declared) != std::mem::discriminant(kind)
			{
				bail!("`{fact}` is a fact platforms state, which is {kind}, and it is declared {declared}");
			}
		}
		// the backfill cache is keyed by the absolute person directory
		if !raw.path.is_absolute() {
			bail!("`path` is {}, which is not absolute", raw.path.display());
		}
		if raw.rank.is_empty() {
			bail!("`rank` is empty, and a purpose is kept in order to be ranked");
		}
		let mut purpose = Self {
			name,
			path: raw.path,
			tags,
			procure: BTreeMap::new(),
			rank: Vec::new(),
			stale_half_life: raw.stale_half_life,
			unanswered_half_life: raw.unanswered_half_life,
		};
		purpose.rank = raw.rank.into_iter().map(|term| purpose.term(term)).collect::<Result<_>>()?;
		purpose.procure = raw
			.procure
			.into_iter()
			.map(|(name, raw)| {
				// `procure <platform>:<slug>` is told apart from `procure <name>` by the colon
				if name.contains(':') {
					bail!("strategy `{name}` carries a `:`, which is how an ad-hoc venue is addressed");
				}
				let strategy = purpose.strategy(raw).wrap_err_with(|| format!("strategy `{name}`"))?;
				Ok((name, strategy))
			})
			.collect::<Result<_>>()?;
		Ok(purpose)
	}

	/// Whether `value` may stand under `tag` here. The one check every writer of a tag goes through: a
	/// load, the `tag` command, a procurement strategy and the extraction. `None` — judged, nothing
	/// supports a value — is only ever the extraction's to write.
	pub fn check(&self, tag: &str, value: Option<&Value>) -> Result<()> {
		let (tag, kind) = self.tag(tag)?;
		let Some(value) = value else {
			return match kind.about() {
				Some(_) => Ok(()),
				None => bail!("`{tag}` is null, which only a tag the extraction judges may be"),
			};
		};
		match (kind, value) {
			(TagType::Bool { .. }, Value::Bool(_)) | (TagType::Timestamp, Value::Timestamp(_)) => Ok(()),
			(TagType::Number { min, max, .. }, Value::Number(n)) if (*min..=*max).contains(n) => Ok(()),
			(TagType::Birthday { .. }, Value::Birthday(Birthday::Exact(_))) => Ok(()),
			(TagType::Birthday { .. }, Value::Birthday(Birthday::Rough { min, max, .. })) if min <= max => Ok(()),
			(TagType::Place, Value::Place { lat, lon, .. }) if (-90.0..=90.0).contains(lat) && (-180.0..=180.0).contains(lon) => Ok(()),
			(TagType::Text { .. }, Value::Text(text)) if !text.trim().is_empty() => Ok(()),
			(TagType::Group(values), Value::Text(word)) if values.contains(word) => Ok(()),
			(kind, value) => bail!("`{tag}` is {kind}, and `{}` is not one", value.nix()),
		}
	}

	/// The tag `name` spells, however it is spelled, under the name it is shown by.
	pub fn tag(&self, name: &str) -> Result<(String, &TagType)> {
		let name = snake(name);
		let kind = self.tags.get(&name).ok_or_else(|| {
			eyre!(
				"`{name}` is not in `purposes.{}.tags`, which names {}",
				self.name,
				self.tags.keys().cloned().collect::<Vec<_>>().join(", ")
			)
		})?;
		Ok((name, kind))
	}

	fn term(&self, raw: RawTerm) -> Result<Term> {
		let RawTerm { of, weight, within, near, decay } = raw;
		let of = snake(&of);
		if !(weight.is_finite() && weight > 0.0) {
			bail!("rank term `{of}` has weight {weight}; a weight is a positive share of the score");
		}
		let given: Vec<&str> = [within.is_some().then_some("within"), near.is_some().then_some("near"), decay.is_some().then_some("decay")]
			.into_iter()
			.flatten()
			.collect();
		let shape = |takes: &[&str]| -> Result<()> {
			match given.iter().all(|g| takes.contains(g)) && takes.iter().all(|t| given.contains(t)) {
				true => Ok(()),
				false => bail!(
					"rank term `{of}` takes {}, got {}",
					if takes.is_empty() { "no parameters".to_string() } else { takes.join(" and ") },
					if given.is_empty() { "none".to_string() } else { given.join(" and ") }
				),
			}
		};
		let rate = || -> Result<f64> {
			let d = decay.expect("`shape` required it");
			match d.is_finite() && d >= 0.0 {
				true => Ok(d),
				false => bail!("rank term `{of}` has decay {d}; a decay is a finite discount of age, 0 or above"),
			}
		};
		let signal = match (self.tags.get(&of), of.as_str()) {
			(Some(TagType::Bool { .. }), _) => shape(&[]).map(|()| Signal::Bool)?,
			(Some(TagType::Text { .. }), _) => shape(&[]).map(|()| Signal::Present)?,
			(Some(TagType::Number { min, max, .. }), _) => shape(&[]).map(|()| Signal::Number { min: *min, max: *max })?,
			(Some(TagType::Birthday { .. }), _) => {
				shape(&["within"])?;
				let [lo, hi] = within.expect("`shape` required it");
				if !(lo.is_finite() && hi.is_finite() && lo <= hi) {
					bail!("rank term `{of}` has within = [{lo} {hi}], which is not a range of ages");
				}
				Signal::Age { lo, hi }
			}
			(Some(TagType::Place), _) => {
				shape(&["near"])?;
				let near = near.expect("`shape` required it");
				if !(near.radius_km.is_finite() && near.radius_km >= 0.0 && near.halving_km.is_finite() && near.halving_km > 0.0) {
					bail!("rank term `{of}` needs radius_km >= 0 and halving_km > 0");
				}
				Signal::Place(near)
			}
			(Some(TagType::Timestamp), _) => {
				shape(&["decay"])?;
				Signal::Timestamp { decay: rate()? }
			}
			(Some(TagType::Group(_)), _) => bail!("rank term `{of}` reads a group, which carries no order to rank by"),
			(None, "interactions") => shape(&[]).map(|()| Signal::Interactions)?,
			(None, "last_interaction") => {
				shape(&["decay"])?;
				Signal::LastInteraction { decay: rate()? }
			}
			(None, "venue_activity") => {
				shape(&["decay"])?;
				Signal::VenueActivity { decay: rate()? }
			}
			(None, _) => bail!("rank term `{of}` is neither a tag of this purpose nor a builtin ({})", BUILTINS.join(", ")),
		};
		Ok(Term { of, weight, signal })
	}

	fn strategy(&self, raw: RawStrategy) -> Result<Strategy> {
		let mut strategy = Strategy::from(raw.venue.parse::<VenueRef>()?);
		if let Some(predicate) = raw.predicate {
			strategy = self.narrow(&strategy, &predicate)?;
		}
		for (tag, value) in raw.tags {
			let value = match value {
				Value::Text(word) if word.starts_with('$') => {
					let over = self.over(&word)?;
					let (group, values) = over
						.first_key_value()
						.filter(|_| over.len() == 1 && placeholders(&word)[0].0 == (0..word.len()))
						.ok_or_else(|| eyre!("tag `{tag}` is `{word}`, and a placeholder stands for a tag's whole value"))?;
					for value in values {
						self.check(&tag, Some(&Value::Text(value.clone())))
							.wrap_err_with(|| format!("`{tag} = {word}` with ${group} = {value}"))?;
					}
					let group = group.clone();
					strategy.generic_over.extend(over);
					StrategyTag::Templated(group)
				}
				value => {
					self.check(&tag, Some(&value))?;
					StrategyTag::Given(value)
				}
			};
			strategy.tags.insert(tag, value);
		}
		Ok(strategy)
	}

	/// What a pull does that brings `term` up to date.
	pub(crate) fn refreshed_by(&self, term: &Term) -> Refresh {
		let reasoned = || match self.tags[&term.of].about() {
			Some(_) => Refresh::Reasoning,
			None => Refresh::Never,
		};
		match &term.signal {
			Signal::Interactions | Signal::LastInteraction { .. } => Refresh::Fetch,
			Signal::Place(_) | Signal::Age { .. } if FACTS.iter().any(|(fact, _)| *fact == term.of) => Refresh::Fetch,
			Signal::Bool | Signal::Present | Signal::Number { .. } | Signal::Age { .. } => reasoned(),
			// `recon` writes the venues and a human the timestamps
			Signal::Place(_) | Signal::Timestamp { .. } | Signal::VenueActivity { .. } => Refresh::Never,
		}
	}

	/// `strategy` with `predicate` — inline SQL or a path to it — ANDed onto its own `where`.
	pub fn narrow(&self, strategy: &Strategy, predicate: &str) -> Result<Strategy> {
		let predicate = venue::clause(predicate)?;
		let mut narrowed = strategy.clone();
		narrowed.generic_over.extend(self.over(&predicate)?);
		narrowed.predicate = Some(match &strategy.predicate {
			Some(own) => format!("({}) AND ({})", own.trim(), predicate.trim()),
			None => predicate,
		});
		Ok(narrowed)
	}

	/// The groups `text`'s placeholders name, with the values each may be bound to.
	fn over(&self, text: &str) -> Result<BTreeMap<String, BTreeSet<String>>> {
		placeholders(text)
			.into_iter()
			.map(|(_, group)| match self.tags.get(&snake(group)) {
				Some(TagType::Group(values)) => Ok((snake(group), values.clone())),
				_ => bail!(
					"`${group}` names no group of `purposes.{}.tags`, whose groups are {}",
					self.name,
					self.tags
						.iter()
						.filter(|(_, kind)| matches!(kind, TagType::Group(_)))
						.map(|(g, _)| g.as_str())
						.collect::<Vec<_>>()
						.join(", ")
				),
			})
			.collect()
	}
}

/// What a tag may hold. Only the kinds an extraction can fill carry an `about`, which is what puts
/// them in its prompt.
#[derive(Clone, Debug, derive_more::Display)]
pub enum TagType {
	#[display("a bool")]
	Bool { about: Option<String> },
	/// A few words, ranked by whether there are any.
	#[display("a text")]
	Text { about: Option<String> },
	/// Bounded, so it normalises on its own rather than against the cohort.
	#[display("a number in [{min}, {max}]")]
	Number { min: f64, max: f64, about: Option<String> },
	/// A date, or a range of years off a stated age. The extraction proposes the latter, and a
	/// proposal lands only when it [supersedes](Birthday::supersedes) what is there.
	#[display("a birthday: a date, or {{min; max; as_of}} years")]
	Birthday { about: Option<String> },
	#[display("a place {{name; lat; lon}}")]
	Place,
	#[display("a timestamp")]
	Timestamp,
	/// One value per person, out of these. What a strategy can be generic over.
	#[display("one of {}", _0.iter().map(String::as_str).collect::<Vec<_>>().join(", "))]
	Group(BTreeSet<String>),
}
impl TagType {
	pub fn about(&self) -> Option<&str> {
		match self {
			Self::Bool { about } | Self::Text { about } | Self::Number { about, .. } | Self::Birthday { about } => about.as_deref(),
			Self::Place | Self::Timestamp | Self::Group(_) => None,
		}
	}

	/// How a value is typed on the command line: `true`, `0.7`, `Lyon@45.76,4.84`, a timestamp or a
	/// bare date, a group's value as itself, and a birthday as a date, a year `1990` or years
	/// `1988..1992` — a range typed today is as of today.
	pub fn parse(&self, raw: &str) -> Result<Value> {
		let number = |s: &str| s.trim().parse::<f64>().wrap_err_with(|| format!("`{s}` is not a number"));
		Ok(match self {
			Self::Bool { .. } => Value::Bool(raw.parse().wrap_err_with(|| format!("`{raw}` is not true or false"))?),
			Self::Text { .. } => Value::Text(raw.to_string()),
			Self::Number { .. } => Value::Number(number(raw)?),
			Self::Birthday { .. } => Value::Birthday(match raw.parse::<jiff::civil::Date>() {
				Ok(date) => Birthday::Exact(date),
				Err(_) => {
					let year = |s: &str| s.trim().parse::<i16>().wrap_err_with(|| format!("`{s}` is not a year, and `{raw}` is no date"));
					let (min, max) = match raw.split_once("..") {
						Some((min, max)) => (year(min)?, year(max)?),
						None => (year(raw)?, year(raw)?),
					};
					Birthday::Rough {
						min,
						max,
						as_of: Some(Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC).date()),
					}
				}
			}),
			Self::Place => {
				let (name, at) = raw.rsplit_once('@').ok_or_else(|| eyre!("a place is `<name>@<lat>,<lon>`, got `{raw}`"))?;
				let (lat, lon) = at.split_once(',').ok_or_else(|| eyre!("a place is `<name>@<lat>,<lon>`, got `{raw}`"))?;
				Value::Place {
					name: name.to_string(),
					lat: number(lat)?,
					lon: number(lon)?,
				}
			}
			Self::Timestamp => Value::Timestamp(match raw.parse::<Timestamp>() {
				Ok(at) => at,
				Err(_) => raw
					.parse::<jiff::civil::Date>()
					.wrap_err_with(|| format!("`{raw}` is neither a timestamp nor a date"))?
					.to_zoned(jiff::tz::TimeZone::UTC)
					.wrap_err("a date at UTC midnight")?
					.timestamp(),
			}),
			Self::Group(values) => match values.contains(raw) {
				true => Value::Text(raw.to_string()),
				false => bail!("`{raw}` is not {self}"),
			},
		})
	}
}

/// One signal mapped to `[0, 1]`, and its share of the score.
#[derive(Clone, Debug)]
pub struct Term {
	/// The tag or builtin it reads, as the config names it.
	pub of: String,
	pub(crate) weight: f64,
	pub(crate) signal: Signal,
}
/// How people get procured into a purpose: selection over what `recon` already wrote, which fetches
/// nothing. Generic over the groups its `$<group>` placeholders name, and run only once [bound](Self::bind).
#[derive(Clone, Debug)]
pub struct Strategy {
	at: VenueRef,
	/// A SQL `WHERE` over the roster table. `None` is the whole roster.
	predicate: Option<String>,
	/// Put on everyone it selects, whether it created them or not.
	tags: BTreeMap<String, StrategyTag>,
	generic_over: BTreeMap<String, BTreeSet<String>>,
}
impl Strategy {
	pub fn generic_over(&self) -> impl Iterator<Item = &str> {
		self.generic_over.keys().map(String::as_str)
	}

	/// Exactly the groups it is generic over, each to one of its values.
	pub fn bind(&self, bindings: &BTreeMap<String, String>) -> Result<Bound> {
		for (group, values) in &self.generic_over {
			let joined = values.iter().map(String::as_str).collect::<Vec<_>>().join("|");
			match bindings.get(group) {
				None => bail!("generic over ${group} — pass --{group} <{joined}>"),
				Some(value) if !values.contains(value) => bail!("`{value}` is not a {group}: one of {joined}"),
				Some(_) => {}
			}
		}
		if let Some(group) = bindings.keys().find(|group| !self.generic_over.contains_key(*group)) {
			bail!("not generic over ${group}");
		}
		let bound = |group: &str| bindings.get(&snake(group)).expect("checked above for every group it is over").clone();
		Ok(Bound {
			at: self.at.clone(),
			predicate: self.predicate.as_ref().map(|predicate| {
				let mut out = String::new();
				let mut last = 0;
				for (span, group) in placeholders(predicate) {
					out.push_str(&predicate[last..span.start]);
					out.push_str(&bound(group));
					last = span.end;
				}
				out.push_str(&predicate[last..]);
				out
			}),
			tags: self
				.tags
				.iter()
				.map(|(tag, value)| {
					let value = match value {
						StrategyTag::Given(value) => value.clone(),
						StrategyTag::Templated(group) => Value::Text(bound(group)),
					};
					(tag.clone(), value)
				})
				.collect(),
			bindings: bindings.clone(),
		})
	}
}

/// A [`Strategy`] with every placeholder substituted.
#[derive(Clone, Debug)]
pub struct Bound {
	pub at: VenueRef,
	pub predicate: Option<String>,
	pub tags: BTreeMap<String, Value>,
	/// What it was bound with.
	pub bindings: BTreeMap<String, String>,
}
/// Every `$<name>` in `text`: the bytes it spans, and the name.
fn placeholders(text: &str) -> Vec<(Range<usize>, &str)> {
	let mut found = Vec::new();
	let mut from = 0;
	while let Some(at) = text[from..].find('$').map(|i| from + i) {
		let name = &text[at + 1..];
		let name = &name[..name.find(|c: char| !bare_group(c)).unwrap_or(name.len())];
		if !name.is_empty() {
			found.push((at..at + 1 + name.len(), name));
		}
		from = at + 1 + name.len();
	}
	found
}

/// The one spelling a tag name is kept and shown in: `ServiceArb`, `service-arb` and `service_arb`
/// are one tag.
pub(crate) fn snake(name: &str) -> String {
	heck::ToSnakeCase::to_snake_case(name)
}

/// A map keyed by tag names, each [snake]d; two spellings of one name are refused rather than one
/// silently winning.
pub(crate) fn snake_keys<'de, D: Deserializer<'de>, V: Deserialize<'de>>(d: D) -> std::result::Result<BTreeMap<String, V>, D::Error> {
	let mut out = BTreeMap::new();
	for (key, value) in BTreeMap::<String, V>::deserialize(d)? {
		let name = snake(&key);
		if out.insert(name.clone(), value).is_some() {
			return Err(D::Error::custom(format!("`{key}` is another spelling of the tag `{name}`, which is already named")));
		}
	}
	Ok(out)
}

/// What a group's name is spelled in, so a placeholder ends where the name does.
fn bare_group(c: char) -> bool {
	c.is_ascii_alphanumeric() || c == '_'
}

/// What a group's value is spelled in: it is spliced into SQL and typed as a flag value.
fn bare_value(c: char) -> bool {
	c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'
}
/// A venue ad hoc: the whole roster, tagging nobody.
impl From<VenueRef> for Strategy {
	fn from(at: VenueRef) -> Self {
		Self {
			at,
			predicate: None,
			tags: BTreeMap::new(),
			generic_over: BTreeMap::new(),
		}
	}
}

#[derive(Clone, Debug)]
enum StrategyTag {
	Given(Value),
	/// The value its group is bound to.
	Templated(String),
}
/// How a term's value is derived; which one is fixed by the type of what it reads.
#[derive(Clone, Debug)]
pub(crate) enum Signal {
	Bool,
	/// 1 for any text.
	Present,
	Number {
		min: f64,
		max: f64,
	},
	/// The fraction of the ages their birthday allows that falls inside `[lo, hi]`.
	Age {
		lo: f64,
		hi: f64,
	},
	Place(Near),
	Timestamp {
		decay: f64,
	},
	/// Distinct days with a line by them in their year files.
	Interactions,
	/// Their newest year-file line, in either direction.
	LastInteraction {
		decay: f64,
	},
	/// Their lines across every venue transcript, as `cold` has always ranked them.
	VenueActivity {
		decay: f64,
	},
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Refresh {
	/// Every handle of theirs answered a pull.
	Fetch,
	/// A model read what the pull fetched.
	Reasoning,
	Never,
}

/// 1 inside the radius, halving every `halving_km` beyond it.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Near {
	pub(crate) lat: f64,
	pub(crate) lon: f64,
	pub(crate) radius_km: f64,
	pub(crate) halving_km: f64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPurpose {
	path: PathBuf,
	#[serde(default, deserialize_with = "snake_keys")]
	tags: BTreeMap<String, RawTag>,
	#[serde(default)]
	procure: BTreeMap<String, RawStrategy>,
	rank: Vec<RawTerm>,
	stale_half_life: HalfLife,
	unanswered_half_life: HalfLife,
}

/// A list is a group; anything else is read as [`RawTyped`], afterwards, so its own errors survive.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawTag {
	Group(Vec<String>),
	Typed(serde_json::Value),
}
impl RawTag {
	fn typed(self, tag: &str) -> Result<TagType> {
		match self {
			Self::Group(values) => {
				if tag.is_empty() || !tag.chars().all(bare_group) {
					bail!("group `{tag}` is named outside [A-Za-z0-9_], so no `$` placeholder could name it");
				}
				if let Some(bad) = values.iter().find(|v| v.is_empty() || !v.chars().all(bare_value)) {
					bail!("group `{tag}` has value `{bad}`; a value is a bare word in [a-z0-9_-]");
				}
				let set: BTreeSet<String> = values.iter().cloned().collect();
				if set.is_empty() || set.len() != values.len() {
					bail!("group `{tag}` lists {values:?}, and a group is a non-empty list of distinct values");
				}
				Ok(TagType::Group(set))
			}
			Self::Typed(raw) => serde_json::from_value::<RawTyped>(raw).wrap_err_with(|| format!("tag `{tag}`"))?.typed(tag),
		}
	}
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTyped {
	#[serde(rename = "type")]
	kind: String,
	about: Option<String>,
	min: Option<f64>,
	max: Option<f64>,
}
impl RawTyped {
	fn typed(self, tag: &str) -> Result<TagType> {
		let Self { kind, about, min, max } = self;
		if kind != "number" && (min.is_some() || max.is_some()) {
			bail!("tag `{tag}` is a {kind}, and only a number takes bounds");
		}
		Ok(match kind.as_str() {
			"bool" => TagType::Bool { about },
			"text" => TagType::Text { about },
			"number" => {
				let (Some(min), Some(max)) = (min, max) else {
					bail!("tag `{tag}` is a number, so it declares `min` and `max` — it normalises within them");
				};
				if !(min.is_finite() && max.is_finite() && min < max) {
					bail!("tag `{tag}` has bounds [{min}, {max}], which hold no range");
				}
				TagType::Number { min, max, about }
			}
			"birthday" => TagType::Birthday { about },
			"place" | "timestamp" if about.is_some() => bail!("tag `{tag}` is a {kind}, which the extraction cannot fill, so it takes no `about`"),
			"place" => TagType::Place,
			"timestamp" => TagType::Timestamp,
			other => bail!("tag `{tag}` has type `{other}`; a type is one of bool, text, number, birthday, place, timestamp"),
		})
	}
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTerm {
	of: String,
	weight: f64,
	within: Option<[f64; 2]>,
	near: Option<Near>,
	decay: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStrategy {
	venue: String,
	/// May carry `$<group>` placeholders, as may a tag's value.
	#[serde(rename = "where")]
	predicate: Option<String>,
	#[serde(default, deserialize_with = "snake_keys")]
	tags: BTreeMap<String, Value>,
}
