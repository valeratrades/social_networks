//! Read-only check that `telegram_utils::connect` completes: it deadlocked for months because
//! the sender-pool runner was never polled while the handshake awaited RPCs.
//! `PHONE_NUMBER_FR=... TELEGRAM_API_HASH=... cargo r -p social_networks_adapters --example telegram_connect`
use social_networks_utils::telegram_utils::{self, ConnectionConfig};

#[tokio::main]
async fn main() -> color_eyre::eyre::Result<()> {
	color_eyre::install()?;
	tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();

	// Nothing here uses the returned client: reaching this point at all means the handshake
	// (is_authorized -> dialog prefetch -> stream_updates) got its RPCs answered.
	telegram_utils::connect(ConnectionConfig {
		username: "@valeratrades",
		phone: &std::env::var("PHONE_NUMBER_FR")?,
		api_id: 19721916,
		api_hash: &std::env::var("TELEGRAM_API_HASH")?,
		session_suffix: "_dm",
	})
	.await?;

	println!("handshake completed");
	Ok(())
}
