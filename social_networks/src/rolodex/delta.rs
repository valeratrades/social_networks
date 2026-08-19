use std::collections::BTreeMap;

use color_eyre::eyre::{Result, WrapErr};
use serde::Deserialize;

use super::{
	person::{LogEntry, Person},
	sources::{Activity, Msg},
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

pub async fn extract(delta: &Delta<'_>) -> Result<Extraction> {
	let prompt = prompt(delta);
	let response = ask_llm::Client::default()
		.model(ask_llm::Model::Medium)
		.force_json()
		.ask(&prompt)
		.await
		.map_err(|e| color_eyre::eyre::eyre!("{e:#}"))
		.wrap_err("extraction call failed")?;
	serde_json::from_str(&response.text).wrap_err_with(|| format!("extraction did not return the requested shape:\n{}", response.text))
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
