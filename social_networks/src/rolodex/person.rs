use std::{
	collections::BTreeMap,
	path::{Path, PathBuf},
};

use color_eyre::eyre::{Result, WrapErr, bail};
use serde::Deserialize;

/// A person file. The file is a rendered view of this struct — [`render`] regenerates it whole,
/// so comments and hand formatting do not survive a `pull`.
///
/// `deny_unknown_fields` because the alternative is a misspelled or unwrapped attribute reading as
/// an empty person, which `pull` then reports as "nothing new" forever.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Person {
	/// File stem. Not in the file itself.
	#[serde(skip)]
	pub name: String,
	/// Platform → handle. `discord` and `telegram` are the two `pull` knows how to fetch; the rest
	/// come from discord's connected accounts and are there for a human to read.
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
}
impl Person {
	pub fn skeleton(name: &str) -> Self {
		Self {
			name: name.to_string(),
			..Default::default()
		}
	}

	pub fn path(&self, dir: &Path) -> PathBuf {
		dir.join(format!("{}.nix", self.name))
	}

	/// Match on the file stem and on every handle, so `pull dev_ardi` finds the person whose
	/// discord handle that is without anyone having to know what their file is called.
	pub fn matches(&self, pattern: &str) -> bool {
		let pattern = pattern.to_lowercase();
		self.name.to_lowercase().contains(&pattern) || self.handles.values().any(|h| h.to_lowercase().contains(&pattern))
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

	/// `''` blocks always end in a newline, so trailing whitespace cannot survive a write. Stripping
	/// it up front is what makes `render -> nix eval -> Person` an identity, and therefore what stops
	/// an unchanged `discord:note` from reading as changed on every pull.
	fn normalize(&mut self) {
		self.summary = self.summary.trim_end().to_string();
		for value in self.sources.values_mut() {
			*value = value.trim_end().to_string();
		}
	}

	pub fn write(&self, dir: &Path) -> Result<()> {
		std::fs::create_dir_all(dir).wrap_err_with(|| format!("failed to create {}", dir.display()))?;
		let path = self.path(dir);
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

/// Evaluate every `*.nix` in `dir` in one nix process, keyed by file stem.
pub fn load_dir(dir: &Path) -> Result<BTreeMap<String, Person>> {
	if !dir.exists() {
		return Ok(BTreeMap::new());
	}
	// `/. + <string>` rather than a bare path literal: a configured path may carry a trailing slash
	// or a space, neither of which a nix path literal accepts.
	let expr = format!(
		r#"let d = /. + {dir}; in builtins.listToAttrs (map (n: {{ name = builtins.substring 0 (builtins.stringLength n - 4) n; value = import (d + "/${{n}}"); }}) (builtins.filter (n: builtins.match ".*\\.nix" n != null) (builtins.attrNames (builtins.readDir d))))"#,
		dir = nix_dq(&dir.display().to_string())
	);
	let raw: BTreeMap<String, Person> = serde_json::from_slice(&nix_eval(&["--expr", &expr])?).wrap_err_with(|| format!("a person file in {} is not a person", dir.display()))?;
	Ok(raw
		.into_iter()
		.map(|(name, mut person)| {
			person.name = name.clone();
			person.normalize();
			(name, person)
		})
		.collect())
}

pub fn load_one(path: &Path) -> Result<Person> {
	let mut person: Person = serde_json::from_slice(&nix_eval(&["--file", &path.display().to_string()])?).wrap_err_with(|| format!("{} is not a person", path.display()))?;
	person.name = path.file_stem().expect("a path we built from a stem").to_string_lossy().into_owned();
	person.normalize();
	Ok(person)
}

pub fn render(person: &Person) -> String {
	let mut s = String::from("{\n");

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

	s.push_str("}\n");
	s
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

		let person = Person {
			name: "ardi".to_string(),
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
		};

		let dir = std::env::temp_dir().join("social_networks_rolodex_render_test");
		let _ = std::fs::remove_dir_all(&dir);
		person.write(&dir).unwrap();
		assert_eq!(load_one(&person.path(&dir)).unwrap(), person);
		std::fs::remove_dir_all(&dir).unwrap();
	}
}
