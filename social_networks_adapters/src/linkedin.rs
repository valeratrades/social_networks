//! Logged out, so no credentials of any kind — and therefore no messages and no post feed, only the
//! one fact no other source states: where a person works now. Linkedin authwalls anonymous views
//! after a handful, so the checkpoint is the date of the last success rather than an item id, and a
//! profile read inside [`REFRESH_DAYS`] is skipped: the wall turns into a queue that drains over
//! successive runs instead of a failure to design around.
//!
//! Through `curl` rather than an http client, because linkedin answers on the TLS handshake as much
//! as on the request: reqwest gets `999` where a curl carrying byte-identical headers gets the page.

use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use jiff::{Timestamp, tz::TimeZone};

use crate::reach::{Profile, Profiles, Window};

/// How long a fetched profile is taken as still current. The anonymous view budget is a handful of
/// profiles before the authwall, so a run has to touch a few people rather than all of them — which
/// a headline that changes twice a year can afford.
const REFRESH_DAYS: i32 = 30;
const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

#[derive(Default)]
pub struct Linkedin;

impl Profiles for Linkedin {
	async fn profile(&mut self, handle: &str, window: Window) -> Result<Profile> {
		let today = Timestamp::now().to_zoned(TimeZone::UTC).date();
		if let Window::Above { after: Some(last), .. } = &window {
			let last: jiff::civil::Date = last.parse().wrap_err("a linkedin checkpoint is a date")?;
			// leaving `activity.newest` unset is what keeps the checkpoint where it is, so the skip is
			// not itself a success
			if last.until((jiff::Unit::Day, today))?.get_days() < REFRESH_DAYS {
				return Ok(Profile::default());
			}
		}

		// `%{stderr}` keeps the status code out of the body, so the wall is a code rather than a guess
		let out = std::process::Command::new("curl")
			.args([
				"-sL",
				"--max-time",
				"30",
				"-w",
				"%{stderr}%{http_code}",
				"-A",
				UA,
				&format!("https://www.linkedin.com/in/{handle}/"),
			])
			.output()
			.wrap_err("failed to run `curl`")?;
		if !out.status.success() {
			bail!("curl {}", out.status);
		}
		let code = String::from_utf8_lossy(&out.stderr);
		if code.trim() != "200" {
			bail!("linkedin `{handle}`: HTTP {} — `999` is the wall, and it lifts on its own", code.trim());
		}

		let person = person_node(&String::from_utf8_lossy(&out.stdout)).wrap_err_with(|| format!("linkedin `{handle}`"))?;
		let mut profile = Profile::default();
		profile.state("linkedin:headline", Some(&headline(&person)));
		profile.state("linkedin:about", person.get("description").and_then(|v| v.as_str()));
		profile.state("linkedin:name", person.get("name").and_then(|v| v.as_str()));
		profile.activity.newest = Some(today.to_string());
		Ok(profile)
	}
}

/// A public profile ships as an ld+json `@graph`; everything around it is obfuscated markup that
/// changes far more often than the schema does. An authwalled page has no `Person` in it — erroring
/// here rather than returning nothing is what keeps a wall distinguishable from an unchanged profile.
fn person_node(body: &str) -> Result<serde_json::Value> {
	const OPEN: &str = r#"<script type="application/ld+json">"#;
	for block in body.split(OPEN).skip(1) {
		let end = block.find("</script>").ok_or_else(|| eyre!("unterminated ld+json block"))?;
		let value: serde_json::Value = serde_json::from_str(&block[..end]).wrap_err("ld+json block is not json")?;
		let person = value
			.get("@graph")
			.and_then(|v| v.as_array())
			.into_iter()
			.flatten()
			.find(|n| n.get("@type").and_then(|v| v.as_str()) == Some("Person"));
		if let Some(person) = person {
			return Ok(person.clone());
		}
	}
	bail!("no ld+json Person: authwalled, or the profile is not public");
}

/// The current role only: the graph carries the whole position history, newest first, and the tail of
/// it is a CV rather than the one fact worth diffing. A title without a company is still worth having,
/// and so is the reverse.
fn headline(person: &serde_json::Value) -> String {
	/// A logged-out view withholds a value by starring it out rather than omitting the field, and how
	/// much it withholds varies with how much it has already served — but no real title carries a `*`.
	fn unmasked(value: Option<&serde_json::Value>) -> Option<&str> {
		value?.as_str().map(str::trim).filter(|v| !v.is_empty() && !v.contains('*'))
	}
	// ld+json spells one value and a list of them the same way
	let title = match person.get("jobTitle") {
		Some(serde_json::Value::Array(titles)) => unmasked(titles.first()),
		title => unmasked(title),
	};
	match (title, unmasked(person.pointer("/worksFor/0/name"))) {
		(Some(title), Some(org)) => format!("{title} at {org}"),
		(title, org) => title.or(org).unwrap_or_default().to_string(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The half that matters is the second: a parser that returns nothing on a wall or a reshaped page
	/// is indistinguishable from an unchanged profile, and would freeze the source without a sound.
	#[test]
	fn linkedin_wall_is_not_silence() {
		let public = r##"<html><head><script type="application/ld+json">{"@context":"http://schema.org","@graph":[{"@type":"WebPage","url":"https://www.linkedin.com/in/x"},{"@type":"Person","name":"X","jobTitle":["Staff Engineer","Intern"],"worksFor":[{"@type":"Organization","name":"Bar"},{"@type":"Organization","name":"Foo"}],"description":"Builds things."}]}</script></head><body></body></html>"##;
		let person = person_node(public).unwrap();
		assert_eq!(headline(&person), "Staff Engineer at Bar");
		assert_eq!(person.get("description").unwrap(), "Builds things.");

		let masked = serde_json::json!({"jobTitle": ["******** *** ***"], "worksFor": [{"name": "Bar"}]});
		assert_eq!(headline(&masked), "Bar");

		let walled = r##"<html><head><script type="application/ld+json">{"@context":"http://schema.org","@graph":[{"@type":"WebPage","url":"https://www.linkedin.com/authwall"}]}</script></head><body>Sign in</body></html>"##;
		assert!(person_node(walled).is_err());
		assert!(person_node("<html><body>Sign in</body></html>").is_err());
	}
}
