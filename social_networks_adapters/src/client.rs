use std::convert::Infallible;

use thiserror::Error;
use tracing::error;

/// A long-running social-network surface.
///
/// `listen` runs forever in the happy path and returns only when the surface has hit an
/// error class it does not know how to recover from in-process. The caller (the binary)
/// fires a `v_notify` alert and exits — we do not silently retry past unknown failures.
#[trait_variant::make(Send)]
pub trait Client {
	/// Surface name used in alerts, e.g. `"discord_dms"`, `"twitter_schedule"`.
	fn surface(&self) -> &'static str;

	/// Run forever. Recoverable errors are handled inside this call (sleep + reconnect
	/// with backoff). Anything that escapes is treated as terminal by the orchestrator.
	async fn listen(&mut self) -> Result<Infallible, AdapterError>;
}

#[derive(Debug, Error)]
pub enum AdapterError {
	/// The surface's credentials are no longer valid for this process. The orchestrator
	/// must alert and exit; retrying cannot help.
	#[error("{surface}: auth error: {detail}")]
	Auth { surface: &'static str, detail: String },

	/// An error class the adapter has not classified as recoverable. By policy we treat
	/// this the same as `Auth` (alert + exit) — better to bail loudly than retry into a
	/// silently broken state. New variants get added only when an adapter learns to
	/// recover from them on the fly.
	#[error("{surface}: unhandled error: {detail}")]
	Unhandled { surface: &'static str, detail: String },
}

/// Send a max-importance (`error`) notification via the `v_notify` CLI.
/// Failures to spawn `v_notify` are logged but never escalated: a broken alerting path
/// must not also kill the surfaces that are still working.
pub async fn alert(err: &AdapterError) {
	let text = format!("[social_networks] {err}");
	error!("{text}");
	match tokio::process::Command::new("v_notify").args(["-l", "error"]).arg(&text).status().await {
		Ok(s) if s.success() => {}
		Ok(s) => error!("v_notify exited with {s}"),
		Err(e) => error!("failed to spawn v_notify: {e}"),
	}
}
