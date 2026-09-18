//! Who wrote last. Dialogs come back newest-first, so the first private ones whose last message is
//! incoming are the people waiting on an answer — which `rolodex` has no command for.
//!
//! `PHONE_NUMBER_FR=... TELEGRAM_API_HASH=... cargo r -p social_networks_adapters --example telegram_recent [count]`
use color_eyre::eyre::eyre;
use futures::future::{Either, select};
use social_networks_utils::telegram_utils::{self, ConnectionConfig, TelegramConnection};

#[tokio::main]
async fn main() -> color_eyre::eyre::Result<()> {
	color_eyre::install()?;
	tracing_subscriber::fmt().with_max_level(tracing::Level::WARN).init();

	let want: usize = std::env::args().nth(1).unwrap_or("10".into()).parse()?;

	let TelegramConnection { client, mut runner, .. } = telegram_utils::connect(ConnectionConfig {
		username: "@valeratrades",
		phone: &std::env::var("PHONE_NUMBER_FR")?,
		api_id: 19721916,
		api_hash: &std::env::var("TELEGRAM_API_HASH")?,
		session_suffix: "_rolodex",
		seed_from: None,
	})
	.await?;

	let list = async {
		let mut dialogs = client.iter_dialogs();
		let mut found = 0usize;
		//LOOP: newest-first over the dialog list, stopped by `want` or by its end
		while let Some(dialog) = dialogs.next().await? {
			if found >= want {
				break;
			}
			let peer = dialog.peer();
			if peer.id().kind() != grammers_session::types::PeerKind::User {
				continue; // this axis is people, not groups or channels
			}
			let Some(message) = dialog.last_message.as_ref() else { continue };
			if message.outgoing() {
				continue;
			}
			found += 1;
			let text = message.text().chars().take(120).collect::<String>().replace('\n', " ");
			println!(
				"{}\t@{}\t{}\t{}\t{text}",
				peer.id(),
				peer.username().unwrap_or("-"),
				message.date(),
				peer.name().unwrap_or_default()
			);
		}
		Ok::<(), color_eyre::Report>(())
	};

	// Every RPC above and below is answered by the runner; nothing progresses unless it is polled.
	match select(std::pin::pin!(list), runner.as_mut()).await {
		Either::Left((result, _)) => result,
		Either::Right(((), _)) => Err(eyre!("MTProto runner exited")),
	}
}
