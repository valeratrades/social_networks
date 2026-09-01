use std::collections::BTreeMap;

use color_eyre::eyre::{Result, WrapErr};
use serde::Deserialize;
use social_networks_adapters::llm::LlmConfig;
use strum::IntoEnumIterator as _;

use super::{
	person::{LogEntry, Person},
	sources::{Activity, Msg, Source},
};

/// Something new about a person. Only constructible when there is something new, so there is no
/// "should we call the LLM?" branch anywhere to get wrong — and nothing here names `pull`, so a live
/// DM can build one just as well.
pub struct Delta<'a> {
	person: &'a Person,
	new_messages: Vec<Msg>,
	new_activity: Vec<Activity>,
	changed_sources: BTreeMap<String, String>,
}

impl<'a> Delta<'a> {
	pub fn new(person: &'a Person, fetched_sources: &BTreeMap<String, String>, new_messages: Vec<Msg>, new_activity: Vec<Activity>) -> Option<Self> {
		let changed_sources: BTreeMap<String, String> = fetched_sources
			.iter()
			.filter(|(key, value)| person.sources.get(*key) != Some(value))
			.map(|(key, value)| (key.clone(), value.clone()))
			.collect();
		(!changed_sources.is_empty() || !new_messages.is_empty() || !new_activity.is_empty()).then_some(Self {
			person,
			new_messages,
			new_activity,
			changed_sources,
		})
	}
}

#[derive(Debug, Deserialize)]
pub struct Extraction {
	pub summary: String,
	pub new_log_entries: Vec<LogEntry>,
}
pub async fn extract(delta: &Delta<'_>, llm_config: &LlmConfig) -> Result<Extraction> {
	let prompt = prompt(delta);
	let response = llm(llm_config)
		.ask(&prompt)
		.await
		.map_err(|e| color_eyre::eyre::eyre!("{e:#}"))
		.wrap_err("extraction call failed")?;
	serde_json::from_str(&response.text).wrap_err_with(|| format!("extraction did not return the requested shape:\n{}", response.text))
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
	let response = llm(llm_config)
		.ask(&prompt)
		.await
		.map_err(|e| color_eyre::eyre::eyre!("{e:#}"))
		.wrap_err("handle discovery call failed")?;
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
fn llm(llm_config: &LlmConfig) -> ask_llm::Client {
	ask_llm::Client::new(llm_config.into()).model(ask_llm::Model::Slow).force_json()
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
			let who = if message.outgoing { "me" } else { &delta.person.name };
			p.push_str(&format!("- [{who}] {}\n", message.text));
		}
	}

	p
}

fn prompt(delta: &Delta<'_>) -> String {
	let mut p = String::from(
		"You maintain a personal rolodex entry. Fold the new information below into it.\n\n\
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

	if !delta.changed_sources.is_empty() {
		p.push_str("\n## Changed platform texts\n");
		for (key, value) in &delta.changed_sources {
			p.push_str(&format!("### {key}\n{value}\n"));
		}
	}

	if !delta.new_messages.is_empty() {
		p.push_str("\n## New direct messages (oldest first)\n");
		for message in &delta.new_messages {
			let who = if message.outgoing { "me" } else { &delta.person.name };
			let source = message.permalink.as_deref().unwrap_or("null");
			p.push_str(&format!("- [{} | {who} | source={source}] {}\n", message.date, message.text));
		}
	}

	if !delta.new_activity.is_empty() {
		p.push_str(
			"\n## New public activity (oldest first)\n\
			 Apply a far higher bar here than to the messages above. A public feed is mostly routine \
			 churn, and a rolodex full of `pushed to his own repo again` is worse than an empty one. \
			 Record an entry only for something the person would themselves bring up months later: a \
			 new project of theirs, a release, a first contribution to a project that is not theirs, \
			 or a star that marks a real and durable shift in what they work on. Never record ordinary \
			 pushes to work already covered by the summary, and never record a star or fork on its own \
			 unless it clearly means something. When in doubt, record nothing — missing an entry costs \
			 far less than adding one that is not worth remembering.\n",
		);
		for activity in &delta.new_activity {
			p.push_str(&format!("- [{} | source={}] {}\n", activity.date, activity.permalink, activity.text));
		}
	}

	p
}
