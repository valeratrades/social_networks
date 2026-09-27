//! A window on a hidden sway workspace gets no frames, so chrome acks no input for it. A headless
//! output renders without anyone seeing it; the window is parked on it and put back afterwards.

use color_eyre::eyre::{Result, WrapErr, bail, ensure};
use serde_json::Value;

pub(super) struct Parked {
	con_id: u64,
	from: String,
	/// ours to unplug once the window is back
	created: Option<String>,
}
impl Parked {
	/// Moves the window titled `title` onto a headless output's workspace, creating the output if there is none.
	pub(super) fn park(title: &str) -> Result<Self> {
		let (con_id, from) = window(&swaymsg(&["-t", "get_tree"])?, title)?;
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
		swaymsg(&[&format!("[con_id={con_id}]"), "move", "container", "to", "workspace", &workspace])?;
		eprintln!("moved the scraped window from workspace {from} to {workspace} on {output}, which nobody sees; it goes back when the run ends");
		Ok(Self { con_id, from, created })
	}

	pub(super) fn restore(self) -> Result<()> {
		swaymsg(&[&format!("[con_id={}]", self.con_id), "move", "container", "to", "workspace", &self.from])?;
		if let Some(output) = self.created {
			swaymsg(&["output", &output, "unplug"])?;
		}
		Ok(())
	}
}

/// `(con_id, workspace)` of the one window whose title is the page title plus the browser's suffix.
fn window(tree: &Value, title: &str) -> Result<(u64, String)> {
	fn walk<'a>(n: &'a Value, ws: Option<&'a str>, title: &str, found: &mut Vec<(u64, String)>) {
		let ws = match n["type"].as_str() {
			Some("workspace") => n["name"].as_str(),
			_ => ws,
		};
		if n["name"].as_str().is_some_and(|name| name.strip_prefix(title).is_some_and(|rest| rest.starts_with(" - "))) && n["pid"].is_u64() {
			found.push((n["id"].as_u64().expect("nodes have ids"), ws.expect("a window sits on a workspace").to_string()));
		}
		for c in n["nodes"].as_array().into_iter().chain(n["floating_nodes"].as_array()).flatten() {
			walk(c, ws, title, found);
		}
	}
	let mut found = Vec::new();
	walk(tree, None, title, &mut found);
	match &found[..] {
		[one] => Ok(one.clone()),
		[] => bail!("no sway window titled `{title} - …`; the scraped tab must be the active one in its window"),
		many => bail!("{} sway windows titled `{title} - …`: {many:?}", many.len()),
	}
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
