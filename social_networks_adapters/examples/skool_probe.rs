//! `SKOOL_EMAIL=… SKOOL_PASSWORD=… cargo r -p social_networks_adapters --example skool_probe -- <group> [members]`
//!
//! Answers the things about skool that cannot be checked without an account: that the login form
//! submits headlessly, that the cookies it leaves behind carry a plain `reqwest` request, and what a
//! member-visible `postTrees[]` and roster payload actually look like.
//!
//! The roster key is what `Venue::members` reads, and this is how it is settled: `members` dumps
//! every `pageProps` key the member page carries and the first entry under each one that is a list.

use color_eyre::eyre::{Result, eyre};
use social_networks_adapters::skool::{Skool, SkoolCredentials};

#[tokio::main]
async fn main() -> Result<()> {
	tracing_subscriber::fmt().with_env_filter("info").init();
	let group = std::env::args().nth(1).ok_or_else(|| eyre!("usage: skool_probe <group> [members]"))?;
	let roster = std::env::args().nth(2).as_deref() == Some("members");
	let creds = SkoolCredentials {
		email: std::env::var("SKOOL_EMAIL")?,
		password: std::env::var("SKOOL_PASSWORD")?,
	};

	let mut session = Skool::try_new(Some(creds))?;
	let path = match roster {
		true => format!("/{group}/-/members"),
		false => format!("/{group}"),
	};
	let payload = session.page(&path).await?;
	println!("served route: {}", payload["page"]);

	let props = payload.pointer("/props/pageProps").ok_or_else(|| eyre!("no pageProps on the served page"))?;
	let object = props.as_object().ok_or_else(|| eyre!("pageProps is not an object"))?;
	println!("pageProps keys: {:?}", object.keys().collect::<Vec<_>>());

	if roster {
		for (key, value) in object {
			let Some(first) = value.as_array().and_then(|a| a.first()) else { continue };
			println!("\n{key}[0]: {first:#}");
		}
		return Ok(());
	}

	let posts = props.get("postTrees").and_then(|v| v.as_array()).ok_or_else(|| eyre!("no postTrees on the served page"))?;
	println!("postTrees: {}", posts.len());
	let Some(node) = posts.first() else { return Ok(()) };
	println!("node keys: {:?}", node.as_object().map(|o| o.keys().collect::<Vec<_>>()));
	println!("node: {node:#}");
	Ok(())
}
