use std::collections::BTreeMap;

use color_eyre::eyre::{Result, WrapErr, bail};
use jiff::{Timestamp, civil::Date, tz::TimeZone};
use serde::Deserialize;
use social_networks_adapters::{
	llm::LlmConfig,
	reach::{Author, INITIAL_ITEMS, Item, Kind, Source},
};
use social_networks_reach::{
	history::Unreasoned,
	person::{Birthday, LogEntry, Person, Value},
	purpose::{Purpose, TagType},
};
use strum::IntoEnumIterator as _;

/// Something new about a person. Only constructible when there is something new, so there is no
/// "should we call the LLM?" branch anywhere to get wrong — and nothing here names `pull`, so a live
/// DM can build one just as well.
pub struct Delta<'a> {
	person: &'a Person,
	/// [`Kind::Direct`], oldest-first.
	new_messages: Vec<Item>,
	/// Everything they did where anyone could see it: a venue post, a release. Held to a far higher
	/// bar than a DM, and the prompt says so.
	new_public: Vec<Item>,
	changed_sources: BTreeMap<String, String>,
	/// What is already on record, when a judgement is owed that no extraction has made yet: a tag
	/// added to the vocabulary after the person was, which nothing new would otherwise ever ask about.
	record: Option<Vec<String>>,
}

impl<'a> Delta<'a> {
	/// Only the newest [`INITIAL_ITEMS`] of each half reach the prompt. The archive keeps the rest; a
	/// conversation the model cannot hold in one read is not one it summarises better for trying.
	pub fn new(person: &'a Person, fetched_sources: &BTreeMap<String, String>, items: Vec<Item>, record: Option<Vec<String>>) -> Option<Self> {
		let (mut new_messages, mut new_public): (Vec<Item>, Vec<Item>) = items.into_iter().partition(|item| item.kind == Kind::Direct);
		for half in [&mut new_messages, &mut new_public] {
			if half.len() > INITIAL_ITEMS {
				half.drain(..half.len() - INITIAL_ITEMS);
			}
		}
		let changed_sources: BTreeMap<String, String> = fetched_sources
			.iter()
			.filter(|(key, value)| person.sources.get(*key) != Some(value))
			.map(|(key, value)| (key.clone(), value.clone()))
			.collect();
		(!changed_sources.is_empty() || !new_messages.is_empty() || !new_public.is_empty() || record.is_some()).then_some(Self {
			person,
			new_messages,
			new_public,
			changed_sources,
			record,
		})
	}
}

impl From<Delta<'_>> for Unreasoned {
	fn from(delta: Delta<'_>) -> Self {
		Self {
			items: delta.new_messages.into_iter().chain(delta.new_public).collect(),
			sources: delta.changed_sources,
		}
	}
}

pub struct Extraction {
	pub summary: String,
	pub new_log_entries: Vec<LogEntry>,
	/// Every tag carrying an `about`, regenerated whole the way `summary` is — a birthday aside, which
	/// is a proposal [`Person::weigh`] takes only when it is better evidence. `None` is "nothing
	/// supports a value", and is kept as such.
	pub tags: BTreeMap<String, Option<Value>>,
}
pub async fn extract(delta: &Delta<'_>, purpose: &Purpose, llm_config: &LlmConfig) -> Result<Extraction> {
	let asked: BTreeMap<&str, (&TagType, &str)> = purpose.tags.iter().filter_map(|(tag, kind)| kind.about().map(|about| (tag.as_str(), (kind, about)))).collect();
	let prompt = prompt(delta, &asked);
	let response = llm(llm_config).ask(&prompt).await.wrap_err("extraction call failed")?;
	let Response { summary, new_log_entries, tags } = serde_json::from_str(&response.text).wrap_err_with(|| format!("extraction did not return the requested shape:\n{}", response.text))?;
	let tags = tags.unwrap_or_default();
	if !tags.keys().map(String::as_str).eq(asked.keys().copied()) {
		bail!(
			"extraction was asked for the tags [{}] and returned [{}]",
			asked.keys().copied().collect::<Vec<_>>().join(", "),
			tags.keys().cloned().collect::<Vec<_>>().join(", ")
		);
	}
	let tags = tags
		.into_iter()
		.map(|(tag, raw)| {
			let value = match raw {
				None => None,
				Some(raw) => Some(match asked[tag.as_str()].0 {
					TagType::Birthday { .. } => serde_json::from_value::<Stated>(raw)?.birthday()?,
					_ => serde_json::from_value::<Value>(raw)?,
				}),
			};
			if let Some(value) = &value {
				purpose.check(&tag, Some(value)).wrap_err("extraction returned a tag value of the wrong type")?;
			}
			Ok((tag, value))
		})
		.collect::<Result<_>>()
		.wrap_err_with(|| format!("extraction returned tags of the wrong shape:\n{}", response.text))?;
	Ok(Extraction { summary, new_log_entries, tags })
}
/// A handle stated in the conversation is a source nobody is looking for. What it finds is fetched
/// by the *next* pull, the same cadence discord's connected accounts already run on.
///
/// Skipped when every [`Source`] is already covered — the set of possible additions is empty.
pub async fn discover_handles(delta: &Delta<'_>, llm_config: &LlmConfig) -> Result<Vec<(String, String)>> {
	if Source::iter().all(|source| delta.person.handles.contains_key(source.as_ref())) {
		return Ok(Vec::new());
	}
	let prompt = discovery_prompt(delta);
	let response = llm(llm_config).ask(&prompt).await.wrap_err("handle discovery call failed")?;
	let discovered: Discovered = serde_json::from_str(&response.text).wrap_err_with(|| format!("handle discovery did not return the requested shape:\n{}", response.text))?;
	Ok(discovered
		.handles
		.into_iter()
		// `FromStr` is the only thing that makes a handle fetchable, so anything else is noise
		.filter(|h| h.platform.to_lowercase().parse::<Source>().is_ok())
		.map(|h| (h.platform.to_lowercase(), h.handle.trim().trim_start_matches('@').to_string()))
		.filter(|(_, handle)| !handle.is_empty())
		.collect())
}
#[derive(Deserialize)]
struct Response {
	summary: String,
	new_log_entries: Vec<LogEntry>,
	tags: Option<BTreeMap<String, Option<serde_json::Value>>>,
}

/// A birthday as the extraction reports it: the age or the birth year somebody stated, and the day
/// of the message that stated it — `null` for a platform text, which carries no date.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Stated {
	age: Option<i16>,
	born: Option<i16>,
	said_on: Option<Date>,
}
impl Stated {
	fn birthday(self) -> Result<Value> {
		let (min, max) = match (self.age, self.born) {
			(Some(age), None) => {
				// an undated text states their age as of when it is read
				let year = self.said_on.unwrap_or_else(|| Timestamp::now().to_zoned(TimeZone::UTC).date()).year();
				(year - age - 1, year - age)
			}
			(None, Some(born)) => (born, born),
			_ => bail!("a stated birthday carries exactly one of `age` and `born`"),
		};
		Ok(Value::Birthday(Birthday::Rough { min, max, as_of: self.said_on }))
	}
}
fn llm(llm_config: &LlmConfig) -> ask_llm::Client {
	ask_llm::Client::new(llm_config.into()).model(ask_llm::Model::Fast).force_json()
}

#[derive(Debug, Deserialize)]
struct Discovered {
	handles: Vec<DiscoveredHandle>,
}
#[derive(Debug, Deserialize)]
struct DiscoveredHandle {
	platform: String,
	handle: String,
}

fn discovery_prompt(delta: &Delta<'_>) -> String {
	let mut p = String::from(
		"You look for one thing: an account handle this person stated or linked outright, on one of \
		 the platforms listed below as missing, so that their feed can be read later.\n\n\
		 Record one when this person gives it themselves — `my github is X`, `github.com/X`, a \
		 profile URL they paste as their own, an @name they name as theirs. Record every such handle \
		 you see on a missing platform.\n\n\
		 Do not record anything else. Never guess a handle from a display name, a nickname or an \
		 email. Never infer one platform's handle from another's. Never take a handle belonging to \
		 somebody else, one I stated about myself, or one on a platform not listed as missing. A \
		 wrong handle pulls a stranger's data into this person's file, which costs far more than \
		 missing one — when nobody stated a handle, {\"handles\": []} is the right and expected answer.\n\n\
		 Respond with JSON only: {\"handles\": [{\"platform\": string, \"handle\": string}]}\n\
		 `platform` is one of the platforms listed below as missing; `handle` is the bare username, \
		 without an @ or a URL around it.\n\n",
	);

	p.push_str(&format!("## Person\n{}\n\n", delta.person.name));

	p.push_str("## Sources\n`pull` can fetch these platforms, given a handle:\n");
	for source in Source::iter() {
		match delta.person.handles.get(source.as_ref()) {
			Some(handle) => p.push_str(&format!("- {} = \"{handle}\" (have)\n", source.as_ref())),
			None => p.push_str(&format!("- {} — missing\n", source.as_ref())),
		}
	}

	if !delta.changed_sources.is_empty() {
		p.push_str("\n## Platform texts\n");
		for (key, value) in &delta.changed_sources {
			p.push_str(&format!("### {key}\n{value}\n"));
		}
	}

	if !delta.new_messages.is_empty() {
		p.push_str("\n## Direct messages (oldest first)\n");
		for message in &delta.new_messages {
			p.push_str(&format!("- [{}] {}\n", who(delta, message), message.text));
		}
	}

	p
}

/// The transcript slot: a DM file has two participants, and an item is either theirs or mine.
fn who<'a>(delta: &'a Delta<'_>, item: &Item) -> &'a str {
	match item.author {
		Author::Me => "me",
		Author::Handle(_) => &delta.person.name,
	}
}

fn day(item: &Item) -> jiff::civil::Date {
	item.at.to_zoned(TimeZone::UTC).date()
}

fn prompt(delta: &Delta<'_>, asked: &BTreeMap<&str, (&TagType, &str)>) -> String {
	let mut p = String::from(
		"You maintain the record kept on one person. Fold the new information below into it.\n\n\
		 Keep only significant facts: accomplishments, milestones, stable preferences, roles, \
		 relationships, and things worth remembering months from now. Discard small talk, logistics, \
		 moods, and anything already covered.\n\n\
		 Respond with JSON only: {\"summary\": string, \"new_log_entries\": [{\"date\": \"YYYY-MM-DD\", \
		 \"text\": string, \"source\": string or null}]}\n\
		 `summary` is the full rewritten summary, a few sentences at most, carrying everything still \
		 true. `new_log_entries` holds only entries not already in the log; copy `date` and `source` \
		 from the message a fact came from, and use null for `source` when it came from a changed \
		 platform text. Return an empty `new_log_entries` if nothing is worth recording.\n\n\
		 Never copy a secret into an entry. Passwords, API keys, tokens, private keys, seed phrases \
		 and card numbers are to be referred to, never reproduced: write `shared his login` and not \
		 the login. The file is plain text on disk and outlives the conversation.\n\n",
	);
	if !asked.is_empty() {
		p.push_str(
			"Also respond with `tags`: an object holding every tag listed under Tags below, each set to \
			 its value as of everything you now know, or null when nothing supports one. Like the \
			 summary, it is rewritten whole — carry a current value forward unless something \
			 contradicts it, and never guess: a gap costs less than a wrong value.\n\n",
		);
	}
	if asked.values().any(|(kind, _)| matches!(kind, TagType::Birthday { .. })) {
		p.push_str(
			"A birthday is the exception: it is never carried forward. Report only the newest statement \
			 of their age or birth year that you see, with `said_on` the date of the message it is in, \
			 or null when it is in a platform text; null when nothing states one.\n\n",
		);
	}

	p.push_str(&format!("## Person\n{}\n\n", delta.person.name));
	p.push_str(&format!(
		"## Current summary\n{}\n\n",
		if delta.person.summary.is_empty() { "(none)" } else { &delta.person.summary }
	));

	p.push_str("## Current log\n");
	if delta.person.log.is_empty() {
		p.push_str("(empty)\n");
	}
	for entry in &delta.person.log {
		p.push_str(&format!("- {} {}\n", entry.date, entry.text));
	}

	if !asked.is_empty() {
		p.push_str("\n## Tags\n");
		for (tag, (kind, about)) in asked {
			let shape = match kind {
				TagType::Bool { .. } => "true or false".to_string(),
				TagType::Text { .. } => "a few words, or null".to_string(),
				TagType::Number { min, max, .. } => format!("a number from {min} to {max}"),
				TagType::Birthday { .. } => "{\"age\": number, \"said_on\": \"YYYY-MM-DD\" or null} or {\"born\": year, \"said_on\": …}".to_string(),
				TagType::Place | TagType::Timestamp | TagType::Group(_) => unreachable!("a purpose refuses an `about` on a {kind} at load"),
			};
			let now = match delta.person.tags.get(*tag) {
				None => "never judged".to_string(),
				Some(None) => "null".to_string(),
				Some(Some(Value::Bool(b))) => b.to_string(),
				Some(Some(Value::Number(n))) => n.to_string(),
				Some(Some(v @ (Value::Birthday(_) | Value::Text(_)))) => v.nix(),
				Some(Some(v @ (Value::Place { .. } | Value::Timestamp(_)))) => unreachable!("`{tag}` = {} was typed against the purpose at load", v.nix()),
			};
			p.push_str(&format!("- `{tag}` ({shape}): {about}. Now: {now}\n"));
		}
	}

	if let Some(record) = &delta.record {
		p.push_str(
			"\n## Their record so far (oldest first)\n\
			 Already folded into the summary and log above, and here only so that every tag can be \
			 judged. Draw no log entries from it.\n",
		);
		for line in record {
			p.push_str(&format!("{line}\n"));
		}
	}

	if !delta.changed_sources.is_empty() {
		p.push_str("\n## Changed platform texts\n");
		for (key, value) in &delta.changed_sources {
			p.push_str(&format!("### {key}\n{value}\n"));
		}
	}

	if !delta.new_messages.is_empty() {
		p.push_str("\n## New direct messages (oldest first)\n");
		for message in &delta.new_messages {
			let source = message.permalink.as_deref().unwrap_or("null");
			p.push_str(&format!("- [{} | {} | source={source}] {}\n", day(message), who(delta, message), message.text));
		}
	}

	if !delta.new_public.is_empty() {
		p.push_str(
			"\n## New public activity (oldest first)\n\
			 Apply a far higher bar here than to the messages above. A public feed is mostly routine \
			 churn, and a record full of `pushed to his own repo again` is worse than an empty one. \
			 Record an entry only for something the person would themselves bring up months later: a \
			 new project of theirs, a release, a first contribution to a project that is not theirs, \
			 or a star that marks a real and durable shift in what they work on. Never record ordinary \
			 pushes to work already covered by the summary, and never record a star or fork on its own \
			 unless it clearly means something. When in doubt, record nothing — missing an entry costs \
			 far less than adding one that is not worth remembering.\n",
		);
		for activity in &delta.new_public {
			p.push_str(&format!("- [{} | source={}] {}\n", day(activity), activity.permalink.as_deref().unwrap_or("null"), activity.text));
		}
	}

	p
}
