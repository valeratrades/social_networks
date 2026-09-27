use std::collections::BTreeMap;

use color_eyre::eyre::{Result, WrapErr};
use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
	pub id: String,
	pub name: String,
	/// the lines under the name, e.g. "Works at … · Lives in Lyon, France"
	pub snippets: Vec<String>,
}

/// Everything a people search page and its pagination said, one JSON document at a time.
#[derive(Debug, Default)]
pub struct Results {
	pub hits: BTreeMap<String, Hit>,
	/// (page id, name) of the City filter in force, as last seen
	pub city: Option<(String, String)>,
	/// `has_next_page` of the result list, as last seen
	pub more: Option<bool>,
}
impl Results {
	/// Takes an embedded `application/json` script or a `/api/graphql/` body, which streams several documents.
	pub fn absorb(&mut self, text: &str) -> Result<()> {
		for doc in serde_json::Deserializer::from_str(text.trim_start_matches("for (;;);")).into_iter::<Value>() {
			self.walk(&doc.wrap_err_with(|| format!("not JSON: {}", &text[..text.len().min(200)]))?);
		}
		Ok(())
	}

	fn walk(&mut self, v: &Value) {
		match v {
			Value::Object(o) => {
				if let Some(hit) = hit(v) {
					self.hits.insert(hit.id.clone(), hit);
				}
				if let Some(more) = o.get("serpResponse").and_then(|r| r.pointer("/results/page_info/has_next_page")).and_then(Value::as_bool) {
					self.more = Some(more);
				}
				if let (Some(text), Some(value)) = (
					v.pointer("/current_value/text").and_then(Value::as_str),
					v.pointer("/current_value/value").and_then(Value::as_str),
				) {
					#[derive(Deserialize)]
					struct Filter {
						name: String,
						args: String,
					}
					let f: Filter = serde_json::from_str(value).expect("a filter value is `{name, args}` JSON");
					if f.name == "users_location" {
						self.city = Some((f.args, text.to_string()));
					}
				}
				o.values().for_each(|x| self.walk(x));
			}
			Value::Array(a) => a.iter().for_each(|x| self.walk(x)),
			_ => {}
		}
	}
}

fn hit(view_model: &Value) -> Option<Hit> {
	if view_model.get("__typename")?.as_str()? != "SearchProfileViewModel" {
		return None;
	}
	let profile = view_model.get("profile")?;
	if profile.get("__typename")?.as_str()? != "User" {
		return None;
	}
	let s = |p: &str| profile.pointer(p).and_then(Value::as_str).map(str::to_string);
	let text = |v: &Value| v.get("text").and_then(Value::as_str).map(str::to_string);
	let snippets = ["prominent_snippet_text_with_entities", "primary_snippet_text_with_entities"]
		.iter()
		.filter_map(|k| text(view_model.get(*k)?))
		.chain(
			view_model
				.get("description_snippets_text_with_entities")
				.and_then(Value::as_array)
				.into_iter()
				.flatten()
				.filter_map(text),
		)
		.collect();
	Some(Hit {
		id: s("/id")?,
		name: s("/name")?,
		snippets,
	})
}
