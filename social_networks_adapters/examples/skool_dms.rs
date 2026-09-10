//! `SKOOL_EMAIL=… SKOOL_PASSWORD=… cargo r -p social_networks_adapters --example skool_dms`
//!
//! Runs the chat poller and prints what it would hand the `dms` daemon. The first poll seeds and
//! says nothing, exactly as it does under the daemon, so the way to see an event here is to have
//! somebody write while this is up.

use color_eyre::eyre::Result;
use social_networks_adapters::{
	Client, DmEvent,
	skool::{SkoolCredentials, SkoolDms},
};

#[tokio::main]
async fn main() -> Result<()> {
	tracing_subscriber::fmt().with_env_filter("info").init();
	let creds = SkoolCredentials {
		email: std::env::var("SKOOL_EMAIL")?,
		password: std::env::var("SKOOL_PASSWORD")?,
	};
	let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
	let mut dms = SkoolDms::try_new(creds, tx)?;

	let drain = async {
		while let Some(DmEvent::Message { sender, text, chat_id, .. }) = rx.recv().await {
			println!("[{chat_id}] {sender}: {text}");
		}
	};
	tokio::select! {
		e = dms.listen() => Err(e.unwrap_err().into()),
		() = drain => unreachable!("the poller holds the sender for as long as it runs"),
	}
}
