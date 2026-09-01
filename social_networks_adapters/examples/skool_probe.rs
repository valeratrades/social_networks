//! `SKOOL_EMAIL=… SKOOL_PASSWORD=… cargo r --example skool_probe -- <group>`
//!
//! Answers the three things about skool that cannot be checked without an account: that the login
//! form submits headlessly, that the cookies it leaves behind carry a plain `reqwest` request, and
//! what a member-visible `postTrees[]` actually looks like.

use color_eyre::eyre::{Result, eyre};
use social_networks_utils::skool::{Skool, SkoolCredentials};

#[tokio::main]
async fn main() -> Result<()> {
	tracing_subscriber::fmt().with_env_filter("info").init();
	let group = std::env::args().nth(1).ok_or_else(|| eyre!("usage: skool_probe <group>"))?;
	let creds = SkoolCredentials {
		email: std::env::var("SKOOL_EMAIL")?,
		password: std::env::var("SKOOL_PASSWORD")?,
	};

	let mut session = Skool::try_new(Some(creds))?;
	let payload = session.page(&format!("/{group}")).await?;
	println!("served route: {}", payload["page"]);

	let posts = payload
		.pointer("/props/pageProps/postTrees")
		.and_then(|v| v.as_array())
		.ok_or_else(|| eyre!("no postTrees on the served page"))?;
	println!("postTrees: {}", posts.len());
	let Some(node) = posts.first() else { return Ok(()) };
	println!("node keys: {:?}", node.as_object().map(|o| o.keys().collect::<Vec<_>>()));
	println!("post: {:#}", node.get("post").unwrap_or(node));
	Ok(())
}
