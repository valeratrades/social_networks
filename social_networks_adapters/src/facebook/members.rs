use std::collections::{BTreeMap, BTreeSet};

use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use serde_json::Value;

use crate::reach::Member;

/// Everything the members page and its pagination said, one JSON document at a time. A member is a
/// `User` edge whose `group_membership` names the group, which keeps out every other person the
/// page happens to mention.
#[derive(Default)]
pub struct Listing {
	/// member id → (group id, member)
	members: BTreeMap<String, (String, Listed)>,
	groups: BTreeMap<String, String>,
	/// `has_next_page` of the full member list, as last seen
	pub(super) more: Option<bool>,
}
impl Listing {
	/// Takes an embedded `application/json` script or a `/api/graphql/` body, which streams several documents.
	pub fn absorb(&mut self, text: &str) -> Result<()> {
		for doc in serde_json::Deserializer::from_str(text.trim_start_matches("for (;;);")).into_iter::<Value>() {
			self.walk(&doc.wrap_err_with(|| format!("not JSON: {}", &text[..text.len().min(200)]))?);
		}
		Ok(())
	}

	pub(super) fn len(&self) -> usize {
		self.members.len()
	}

	/// The one group every member listed so far is a member of.
	pub(super) fn group(&self) -> Result<GroupRef> {
		let ids: BTreeSet<&String> = self.members.values().map(|(g, _)| g).collect();
		let id = match ids.into_iter().collect::<Vec<_>>()[..] {
			[id] => id.clone(),
			[] => bail!("the page listed no members"),
			ref many => bail!("members of several groups on one members page: {many:?}"),
		};
		let name = self.groups.get(&id).ok_or_else(|| eyre!("the page never named group {id}"))?.clone();
		Ok(GroupRef { id, name })
	}

	/// Members of `group`. Facebook's "Joined 59 minutes ago" is relative to the read, so it is kept
	/// as text in `bio` rather than as a join date.
	pub(super) fn rows(&self, group: &str) -> Vec<Member> {
		self.members
			.iter()
			.filter(|(_, (of, _))| of == group)
			.map(|(id, (_, m))| {
				let bio = [m.bio.as_deref(), m.joined.as_deref()].into_iter().flatten().collect::<Vec<_>>().join("\n");
				Member {
					handle: id.clone(),
					display: m.name.clone(),
					joined: None,
					lat: None,
					lon: None,
					zone: None,
					place: None,
					bio: (!bio.is_empty()).then_some(bio),
				}
			})
			.collect()
	}

	pub fn finish(self) -> Result<(GroupRef, Vec<Member>)> {
		let group = self.group()?;
		let rows = self.rows(&group.id);
		Ok((group, rows))
	}

	fn walk(&mut self, v: &Value) {
		match v {
			Value::Object(o) => {
				if o.get("__typename").and_then(Value::as_str) == Some("Group")
					&& let (Some(id), Some(name)) = (o.get("id").and_then(Value::as_str), o.get("name").and_then(Value::as_str))
				{
					self.groups.insert(id.to_string(), name.to_string());
				}
				if let Some(Value::Array(edges)) = o.get("edges") {
					edges.iter().filter_map(member).for_each(|(group, id, m)| self.insert(group, id, m));
				}
				if let Some(more) = o.get("new_members").and_then(|c| c.pointer("/page_info/has_next_page")).and_then(Value::as_bool) {
					self.more = Some(more);
				}
				o.values().for_each(|x| self.walk(x));
			}
			Value::Array(a) => a.iter().for_each(|x| self.walk(x)),
			_ => {}
		}
	}

	/// The same person shows up in several sections, not all of which say when they joined or what their bio is.
	fn insert(&mut self, group: String, id: String, m: Listed) {
		let (joined, bio) = self.members.get(&id).map_or((None, None), |(_, old)| (old.joined.clone(), old.bio.clone()));
		let entry = self.members.entry(id).or_insert((group, m.clone()));
		entry.1 = Listed {
			joined: m.joined.or(joined),
			bio: m.bio.or(bio),
			..m
		};
	}
}

#[derive(Clone, Debug, PartialEq)]
pub struct GroupRef {
	pub id: String,
	pub name: String,
}
#[derive(Clone)]
struct Listed {
	name: String,
	/// e.g. "Joined 59 minutes ago", relative to when the list was read
	joined: Option<String>,
	bio: Option<String>,
}


fn member(edge: &Value) -> Option<(String, String, Listed)> {
	let node = edge.get("node")?;
	if node.get("__typename")?.as_str()? != "User" {
		return None;
	}
	let group = node.pointer("/group_membership/associated_group/id")?.as_str()?;
	let s = |p: &str| node.pointer(p).and_then(Value::as_str).map(str::to_string);
	Some((
		group.to_string(),
		s("/id")?,
		Listed {
			name: s("/name")?,
			joined: edge.pointer("/join_status_text/text").and_then(Value::as_str).map(str::to_string),
			bio: s("/bio_text/text"),
		},
	))
}
