//! Connect on an existing session and dump Telegram's own service chat, which is where login
//! codes for new sessions arrive. Reaching the dump at all also proves `telegram_utils::connect`
//! completes: it deadlocked for months because the sender-pool runner was never polled while the
//! handshake awaited RPCs.
//!
//! `PHONE_NUMBER_FR=... TELEGRAM_API_HASH=... cargo r -p social_networks_adapters --example telegram_probe [session_suffix]`
use color_eyre::eyre::{bail, eyre};
use futures::future::{Either, select};
use grammers_session::{Session as _, types::PeerId};
use social_networks_utils::telegram_utils::{self, ConnectionConfig, TelegramConnection};

/// Telegram's own account, the sender of login codes.
const SERVICE_NOTIFICATIONS_ID: i64 = 777000;

#[tokio::main]
async fn main() -> color_eyre::eyre::Result<()> {
	color_eyre::install()?;
	tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();

	let TelegramConnection { client, mut runner, session, .. } = telegram_utils::connect(ConnectionConfig {
		username: "@valeratrades",
		phone: &std::env::var("PHONE_NUMBER_FR")?,
		api_id: 19721916,
		api_hash: &std::env::var("TELEGRAM_API_HASH")?,
		session_suffix: &std::env::args().nth(1).unwrap_or_default(),
		seed_from: None,
	})
	.await?;

	let peer_id = PeerId::user(SERVICE_NOTIFICATIONS_ID).expect("777000 is a valid user id");
	let Some(peer_ref) = session.peer_ref(peer_id).await? else {
		bail!("service notifications chat is not in the peer cache");
	};

	let dump = async {
		let mut messages = client.iter_messages(peer_ref);
		for _ in 0..5 {
			let Some(message) = messages.next().await? else { break };
			println!("[{}] {}", message.date(), message.text());
		}
		Ok::<(), color_eyre::Report>(())
	};

	// Every RPC above and below is answered by the runner; nothing progresses unless it is polled.
	match select(std::pin::pin!(dump), runner.as_mut()).await {
		Either::Left((result, _)) => result,
		Either::Right(((), _)) => return Err(eyre!("MTProto runner exited")),
	}
}
