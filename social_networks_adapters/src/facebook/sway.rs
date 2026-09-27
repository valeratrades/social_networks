//! A window on a hidden sway workspace gets no frames, so chrome acks no input for it. A headless
//! output renders without anyone seeing it: the window is parked there while the user is elsewhere,
//! and follows them home when they focus its workspace.

use color_eyre::eyre::{Result, WrapErr, bail, ensure, eyre};
use serde_json::Value;

pub(super) struct Window {
	con_id: u64,
	home: Place,
	at: At,
}
impl Window {
	/// The one window whose title is the page title plus the browser's suffix, at home where it sits.
	pub(super) fn find(title: &str) -> Result<Self> {
		let (con_id, home) = find(&swaymsg(&["-t", "get_tree"])?, title)?;
		Ok(Self { con_id, home, at: At::Home })
	}

	pub(super) fn parked(&self) -> bool {
		matches!(self.at, At::Parked { .. })
	}

	/// Onto a headless output's workspace, creating the output if there is none. Home is where it was
	/// just before, which the user may have moved it to since it was found.
	pub(super) fn park(&mut self) -> Result<()> {
		assert!(!self.parked(), "a parked window is not parked again");
		let tree = swaymsg(&["-t", "get_tree"])?;
		self.home = places(&tree, &|n| n["id"].as_u64() == Some(self.con_id))
			.pop()
			.ok_or_else(|| eyre!("the scraped window (con {}) is gone", self.con_id))?
			.1;
		let headless = |outputs: &Value| {
			outputs.as_array().expect("get_outputs is a list").iter().find_map(|o| {
				let name = o["name"].as_str().expect("outputs are named");
				name.starts_with("HEADLESS-")
					.then(|| (name.to_string(), o["current_workspace"].as_str().expect("an active output shows a workspace").to_string()))
			})
		};
		let (created, (output, workspace)) = match headless(&swaymsg(&["-t", "get_outputs"])?) {
			Some(found) => (None, found),
			None => {
				swaymsg(&["create_output"])?;
				let made = headless(&swaymsg(&["-t", "get_outputs"])?).expect("just created");
				(Some(made.0.clone()), made)
			}
		};
		swaymsg(&[&format!("[con_id={}]", self.con_id), "move", "container", "to", "workspace", &workspace])?;
		self.at = At::Parked { created };
		eprintln!(
			"moved the scraped window from workspace {} to {workspace} on {output}, which nobody sees; focusing {0} brings it back",
			self.home.workspace
		);
		Ok(())
	}

	/// Back where it was parked from. Sway destroys an empty unfocused workspace, and recreates one on
	/// the focused output unless told where it belongs.
	pub(super) fn home(&mut self) -> Result<()> {
		let At::Parked { created } = std::mem::replace(&mut self.at, At::Home) else { return Ok(()) };
		let Place { workspace, output } = &self.home;
		let workspaces = swaymsg(&["-t", "get_workspaces"])?;
		if !workspaces.as_array().expect("get_workspaces is a list").iter().any(|w| w["name"] == workspace.as_str()) {
			swaymsg(&["workspace", workspace, "output", output])?;
		}
		swaymsg(&[&format!("[con_id={}]", self.con_id), "move", "container", "to", "workspace", workspace])?;
		if let Some(created) = created {
			swaymsg(&["output", &created, "unplug"])?;
		}
		Ok(())
	}

	/// One line of `swaymsg -t subscribe -m '["workspace","window"]'`.
	pub(super) fn event(&mut self, line: &str) -> Result<()> {
		let e: Value = serde_json::from_str(line).wrap_err_with(|| format!("sway sent a non-JSON event: {line}"))?;
		if e["change"] == "close" && e["container"]["id"].as_u64() == Some(self.con_id) {
			bail!("the scraped window was closed");
		}
		if self.parked() && self.home_focused(&e) {
			self.home()?;
		}
		Ok(())
	}

	fn home_focused(&self, e: &Value) -> bool {
		e["change"] == "focus" && e["current"]["type"] == "workspace" && e["current"]["name"] == self.home.workspace.as_str()
	}
}

#[derive(Clone, Debug, PartialEq)]
struct Place {
	workspace: String,
	output: String,
}
enum At {
	Home,
	/// `created`: the headless output, ours to unplug once the window leaves it
	Parked {
		created: Option<String>,
	},
}

/// `swaymsg -t subscribe -m '["workspace","window"]'`, killed with the run.
pub(super) fn subscribe() -> Result<tokio::process::Child> {
	tokio::process::Command::new("swaymsg")
		.args(["--raw", "-t", "subscribe", "-m", r#"["workspace","window"]"#])
		.stdout(std::process::Stdio::piped())
		.stdin(std::process::Stdio::null())
		.kill_on_drop(true)
		.spawn()
		.wrap_err("swaymsg; is this sway?")
}

fn find(tree: &Value, title: &str) -> Result<(u64, Place)> {
	let titled = |n: &Value| n["name"].as_str().is_some_and(|name| name.strip_prefix(title).is_some_and(|rest| rest.starts_with(" - "))) && n["pid"].is_u64();
	let mut found = places(tree, &titled);
	match found.len() {
		1 => Ok(found.pop().expect("one")),
		0 => bail!("no sway window titled `{title} - …`; the scraped tab must be the active one in its window"),
		n => bail!("{n} sway windows titled `{title} - …`: {found:?}"),
	}
}

/// `(con_id, place)` of every node `pick` takes.
fn places(tree: &Value, pick: &dyn Fn(&Value) -> bool) -> Vec<(u64, Place)> {
	fn walk(n: &Value, output: Option<&str>, workspace: Option<&str>, pick: &dyn Fn(&Value) -> bool, found: &mut Vec<(u64, Place)>) {
		let (output, workspace) = match n["type"].as_str() {
			Some("output") => (n["name"].as_str(), None),
			Some("workspace") => (output, n["name"].as_str()),
			_ => (output, workspace),
		};
		if pick(n) {
			let place = Place {
				workspace: workspace.expect("a window sits on a workspace").to_string(),
				output: output.expect("a workspace sits on an output").to_string(),
			};
			found.push((n["id"].as_u64().expect("nodes have ids"), place));
		}
		for c in n["nodes"].as_array().into_iter().chain(n["floating_nodes"].as_array()).flatten() {
			walk(c, output, workspace, pick, found);
		}
	}
	let mut found = Vec::new();
	walk(tree, None, None, pick, &mut found);
	found
}

fn swaymsg(args: &[&str]) -> Result<Value> {
	let out = std::process::Command::new("swaymsg").arg("--raw").args(args).output().wrap_err("swaymsg; is this sway?")?;
	ensure!(out.status.success(), "swaymsg {args:?}: {}", String::from_utf8_lossy(&out.stdout));
	let v: Value = serde_json::from_slice(&out.stdout).wrap_err_with(|| format!("swaymsg {args:?} answered non-JSON"))?;
	if let Some(results) = v.as_array().filter(|a| a.first().is_some_and(|r| r.get("success").is_some())) {
		for r in results {
			ensure!(r["success"] == true, "swaymsg {args:?}: {r}");
		}
	}
	Ok(v)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn fixture(name: &str) -> String {
		std::fs::read_to_string(format!("{}/tests/fixtures/sway/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
	}

	#[test]
	fn the_window_is_found_by_title_with_its_workspace_and_output() {
		let tree: Value = serde_json::from_str(&fixture("get_tree.json")).unwrap();
		let (con_id, home) = find(&tree, "(20+) Facebook").unwrap();
		assert_eq!(
			(con_id, home),
			(
				21,
				Place {
					workspace: "3".into(),
					output: "DP-1".into()
				}
			)
		);
		assert!(find(&tree, "Inbox").unwrap_err().to_string().starts_with("2 sway windows"));
		assert!(find(&tree, "Facebook").unwrap_err().to_string().starts_with("no sway window"));
	}

	#[test]
	fn only_its_home_workspace_gaining_focus_brings_it_back() {
		let w = Window {
			con_id: 21,
			home: Place {
				workspace: "3".into(),
				output: "DP-1".into(),
			},
			at: At::Parked { created: None },
		};
		let event = |line: &str| serde_json::from_str::<Value>(line).unwrap();
		let focus = fixture("workspace_focus.json");
		assert!(w.home_focused(&event(&focus)));
		assert!(!w.home_focused(&event(&focus.replace(r#""name": "3""#, r#""name": "4""#))));
		assert!(!w.home_focused(&event(&focus.replace(r#""change": "focus""#, r#""change": "init""#))));
	}
}
