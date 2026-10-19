//! The About tab, which facebook splits into `directory_*` sections; every page of it lists the
//! sections that profile has.

use color_eyre::eyre::{Result, WrapErr, eyre};
use jiff::Timestamp;
use serde_json::Value;

use crate::reach::Profile;

/// What one profile page embeds: an About section, or the main page, which carries `posted` and no fields.
#[derive(Debug, Default)]
pub struct Section {
	fields: Vec<Field>,
	/// e.g. `directory_work`: the sections this profile has
	pub present: Vec<String>,
	/// The newest of the person's own stories in the page's first render; stories about them, shared by them or commented on by others are not theirs to date
	pub posted: Option<Timestamp>,
}
impl Section {
	/// From the page's embedded `application/json` scripts, concatenated.
	pub fn parse(scripts: &str) -> Result<Self> {
		let mut s = Self::default();
		for doc in serde_json::Deserializer::from_str(scripts).into_iter::<Value>() {
			s.walk(&doc.wrap_err("an embedded script is not JSON")?)?;
		}
		Ok(s)
	}

	fn walk(&mut self, v: &Value) -> Result<()> {
		match v {
			Value::Object(o) => {
				if let (Some(kind), Some(text)) = (o.get("field_type").and_then(Value::as_str), v.pointer("/title/text").and_then(Value::as_str)) {
					let field = Field {
						kind: kind.to_string(),
						text: text.to_string(),
						subtitle: v.pointer("/list_item_groups/0/list_items/0/text/text").and_then(Value::as_str).map(str::to_string),
						link: o.get("link_url").and_then(Value::as_str).map(unwrap_redirect),
					};
					if !self.fields.contains(&field) {
						self.fields.push(field); // each field is embedded twice: as a node and inside its own renderer
					}
				}
				if let Some(Value::Array(nodes)) = v.pointer("/all_collections/nodes") {
					for url in nodes.iter().filter_map(|n| n.get("url")?.as_str()) {
						let name = url.rsplit(['/', '=']).next().expect("rsplit yields ≥1").to_string();
						if !self.present.contains(&name) {
							self.present.push(name);
						}
					}
				}
				if let (Some(owner), Some(Value::Array(edges))) = (o.get("id").and_then(Value::as_str), v.pointer("/timeline_list_feed_units/edges")) {
					for node in edges.iter().filter_map(|e| e.get("node")) {
						let theirs = node
							.get("actors")
							.and_then(Value::as_array)
							.is_some_and(|a| a.iter().any(|a| a.get("id").and_then(Value::as_str) == Some(owner)));
						if theirs {
							let at = node
								.get("creation_time")
								.and_then(Value::as_i64)
								.ok_or_else(|| eyre!("a story of {owner}'s on their timeline carries no creation_time"))?;
							self.posted = self.posted.max(Some(Timestamp::from_second(at)?));
						}
					}
				}
				o.values().try_for_each(|x| self.walk(x))
			}
			Value::Array(a) => a.iter().try_for_each(|x| self.walk(x)),
			_ => Ok(()),
		}
	}

	fn texts(&self, kind: &str) -> String {
		self.fields.iter().filter(|f| f.kind == kind).map(|f| f.text.as_str()).collect::<Vec<_>>().join("\n")
	}
}

/// What the sections state, `lives_in` aside: that one is a place, which takes a geocoder. Current city
/// and hometown are kept apart, since only the former says where someone lives.
pub fn stated(personal: &Section, work: &Section, education: &Section, contact: &Section) -> Profile {
	let mut profile = Profile::default();
	for (key, section, kind) in [
		("facebook:lives_in", personal, "current_city"),
		("facebook:hometown", personal, "hometown"),
		("facebook:birthday", personal, "birthday"),
		("facebook:work", work, "work"),
		("facebook:education", education, "education"),
	] {
		profile.state(key, Some(&section.texts(kind)));
	}
	// facebook prints "September 25, 2002", and "September 25" when the year is hidden
	profile.born = jiff::civil::Date::strptime("%B %d, %Y", personal.texts("birthday")).ok();
	// an account is a link with the platform named under it; a website carries no such line
	for field in &contact.fields {
		let (Some(link), Some(platform)) = (&field.link, &field.subtitle) else { continue };
		let path = link.split(['?', '#']).next().expect("a split yields at least one piece").trim_end_matches('/');
		if path.matches('/').count() > 2 {
			profile
				.handles
				.insert(platform.to_lowercase(), path.rsplit('/').next().expect("a split yields at least one piece").to_string());
		}
	}
	profile
}
/// One `profile_field`.
#[derive(Clone, Debug, PartialEq)]
struct Field {
	/// facebook's `field_type`, e.g. `current_city`
	kind: String,
	text: String,
	/// the line under it, e.g. "Instagram" for a `screenname`
	subtitle: Option<String>,
	link: Option<String>,
}

/// `l.facebook.com/l.php?u=<target>&h=<token>` → `<target>`; the token expires, the target does not.
fn unwrap_redirect(url: &str) -> String {
	let parsed = reqwest::Url::parse(url).expect("facebook links are absolute");
	match parsed.host_str() == Some("l.facebook.com") {
		true => parsed.query_pairs().find(|(k, _)| k == "u").expect("l.php always carries `u`").1.into_owned(),
		false => url.to_string(),
	}
}
