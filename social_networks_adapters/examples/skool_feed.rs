//! `SKOOL_EMAIL=… SKOOL_PASSWORD=… cargo r -p social_networks_adapters --example skool_feed -- <group>`
//!
//! Two questions `Venue::posts` cannot be written correctly without: whether the group feed pages,
//! and where a post's replies actually live. Prints the id set of the first few feed pages, then
//! opens the newest post and dumps the shape of whatever hangs under it.

use color_eyre::eyre::{Result, eyre};
use social_networks_adapters::skool::{Skool, SkoolCredentials};

#[tokio::main]
async fn main() -> Result<()> {
	tracing_subscriber::fmt().with_env_filter("info").init();
	let group = std::env::args().nth(1).ok_or_else(|| eyre!("usage: skool_feed <group>"))?;
	let creds = SkoolCredentials {
		email: std::env::var("SKOOL_EMAIL")?,
		password: std::env::var("SKOOL_PASSWORD")?,
	};
	let mut session = Skool::try_new(Some(creds))?;

	let mut first_of_page = Vec::new();
	for p in 1..=4 {
		let payload = session.page(&format!("/{group}?p={p}")).await?;
		let props = payload.pointer("/props/pageProps").ok_or_else(|| eyre!("no pageProps"))?;
		let trees = props.get("postTrees").and_then(|v| v.as_array()).ok_or_else(|| eyre!("no postTrees"))?;
		let ids: Vec<&str> = trees.iter().filter_map(|n| n.pointer("/post/id")?.as_str()).collect();
		println!(
			"p={p}: page={} total={} posts={} first={:?} last={:?}",
			props.get("page").unwrap_or(&serde_json::Value::Null),
			props.get("total").unwrap_or(&serde_json::Value::Null),
			ids.len(),
			ids.first(),
			ids.last()
		);
		first_of_page.push(ids.first().map(|s| s.to_string()));
	}
	println!("distinct first ids across pages: {first_of_page:?}");

	// the newest post, opened on its own route — which is where a reply would have to be
	let payload = session.page(&format!("/{group}")).await?;
	let newest = payload.pointer("/props/pageProps/postTrees/0").ok_or_else(|| eyre!("no postTrees"))?.clone();
	println!("\nfeed node keys: {:?}", newest.as_object().map(|o| o.keys().collect::<Vec<_>>()));
	let name = newest.pointer("/post/name").and_then(|v| v.as_str()).ok_or_else(|| eyre!("no post name"))?;
	println!("comments claimed by the feed: {}", newest.pointer("/post/metadata/comments").unwrap_or(&serde_json::Value::Null));

	let post = session.page(&format!("/{group}/{name}")).await?;
	let props = post.pointer("/props/pageProps").ok_or_else(|| eyre!("no pageProps on the post route"))?;
	println!("post route pageProps keys: {:?}", props.as_object().map(|o| o.keys().collect::<Vec<_>>()));
	let tree = props.pointer("/postTree").ok_or_else(|| eyre!("no postTree on the post route"))?;
	println!("postTree keys: {:?}", tree.as_object().map(|o| o.keys().collect::<Vec<_>>()));
	for (key, value) in tree.as_object().into_iter().flatten() {
		if let serde_json::Value::Array(list) = value {
			println!("\n{key}: {} entries", list.len());
			if let Some(first) = list.first() {
				println!("[0] keys = {:?}", first.as_object().map(|o| o.keys().collect::<Vec<_>>()));
				println!("[0] = {first:#}");
			}
		}
	}
	Ok(())
}
