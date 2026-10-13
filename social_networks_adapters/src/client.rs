use std::convert::Infallible;

use color_eyre::eyre::Report;
use reqwest::StatusCode;
use thiserror::Error;
use tracing::error;

use crate::telegram_notifier::TelegramNotifier;

/// A long-running social-network surface.
///
/// `listen` runs forever in the happy path; it retries only errors classified `Transient` at
/// the site that raised them, and returns on anything else. The caller (the binary) fires a
/// `v_notify` alert and exits.
#[trait_variant::make(Send)]
pub trait Client {
	/// Surface name used in alerts, e.g. `"discord_dms"`, `"twitter_schedule"`.
	fn surface(&self) -> &'static str;

	async fn listen(&mut self) -> Result<Infallible, AdapterError>;
}

#[derive(Debug, Error)]
pub enum AdapterError {
	/// The surface's credentials are no longer valid for this process; retrying cannot help.
	#[error("{surface}: auth error: {detail}")]
	Auth { surface: &'static str, detail: String },

	/// Any error not explicitly classified transient: retrying an unknown failure is how a dead
	/// key gets hammered for weeks.
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
/// Report any panic to `v_notify` before the process unwinds, then defer to the
/// default hook (backtrace to stderr). Covers panics the graceful `alert` path
/// misses: inside `listen`, `dms::run`, `unreachable!`, and spawned tasks.
pub fn install_panic_alert(surface: &'static str) {
	let default = std::panic::take_hook();
	std::panic::set_hook(Box::new(move |info| {
		let text = format!("[social_networks] {surface} panicked: {info}");
		error!("{text}");
		// sync spawn: we are already unwinding, cannot await. A broken v_notify
		// must not preempt the default hook, so we only log its failure.
		match std::process::Command::new("v_notify").args(["-l", "error"]).arg(&text).status() {
			Ok(s) if s.success() => {}
			Ok(s) => error!("v_notify exited with {s}"),
			Err(e) => error!("failed to spawn v_notify: {e}"),
		}
		default(info);
	}));
}
/// An error as the polling adapters classify it at the site that raised it. Anything converted
/// without a look is `Unhandled`.
#[derive(Debug)]
pub(crate) enum Fault {
	Transient(String),
	/// A known condition that lasts until something outside us changes: waited out where it is raised.
	Degraded(String),
	Auth(String),
	Unhandled(String),
}
impl Fault {
	pub(crate) fn transient(e: impl Into<Report>) -> Self {
		Fault::Transient(format!("{:#}", e.into()))
	}

	/// The detail to retry on, or the error that ends `listen`.
	pub(crate) fn retry(self, surface: &'static str) -> Result<String, AdapterError> {
		match self {
			Fault::Transient(detail) => Ok(detail),
			Fault::Degraded(detail) => unreachable!("degraded is waited out at the site that raised it: {detail}"),
			Fault::Auth(detail) => Err(AdapterError::Auth { surface, detail }),
			Fault::Unhandled(detail) => Err(AdapterError::Unhandled { surface, detail }),
		}
	}
}

impl<E: Into<Report>> From<E> for Fault {
	fn from(e: E) -> Self {
		Fault::Unhandled(format!("{:#}", e.into()))
	}
}

/// Pass a successful response through; classify a failed one by its status, `degraded` being the ones this surface knows to persist.
pub(crate) async fn ok_or_classify(response: reqwest::Response, op: &str, degraded: &[u16]) -> Result<reqwest::Response, Fault> {
	let status = response.status();
	if status.is_success() {
		return Ok(response);
	}
	let body = response.text().await.map_err(Fault::transient)?;
	Err(http_fault(status, format!("{op}: {status}: {body}"), degraded))
}

fn http_fault(status: StatusCode, detail: String, degraded: &[u16]) -> Fault {
	match status.as_u16() {
		code if degraded.contains(&code) => Fault::Degraded(detail),
		401 | 403 => Fault::Auth(detail),
		429 | 500..=599 => Fault::Transient(detail),
		_ => Fault::Unhandled(detail),
	}
}

/// A surface waiting out a [`Fault::Degraded`]: reported when it begins and when it ends, never in between.
#[derive(Default)]
pub(crate) struct Degradation(Option<String>);
impl Degradation {
	/// What the last cycle found: `None` is a cycle the condition did not show in.
	pub(crate) async fn observe(&mut self, notifier: &TelegramNotifier, surface: &str, now: Option<String>) {
		match (&self.0, &now) {
			(None, Some(detail)) => notifier.report_recoverable(surface, &format!("degraded, waiting it out: {detail}")).await,
			(Some(_), None) => notifier.report_recoverable(surface, "recovered").await,
			_ => {}
		}
		self.0 = now;
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn statuses_classify_per_surface() {
		let class = |code: u16, degraded: &[u16]| match http_fault(StatusCode::from_u16(code).unwrap(), String::new(), degraded) {
			Fault::Transient(_) => "transient",
			Fault::Degraded(_) => "degraded",
			Fault::Auth(_) => "auth",
			Fault::Unhandled(_) => "unhandled",
		};
		for (code, degraded, expected) in [
			(503, &[][..], "transient"),
			(429, &[], "transient"),
			(401, &[], "auth"),
			(402, &[], "unhandled"),
			(404, &[], "unhandled"),
			(402, crate::twitter::DEGRADED, "degraded"),
			(404, crate::twitter::DEGRADED, "unhandled"),
			(404, crate::youtube::DEGRADED, "degraded"),
			(402, crate::youtube::DEGRADED, "unhandled"),
			(503, crate::youtube::DEGRADED, "transient"),
		] {
			assert_eq!(class(code, degraded), expected, "{code} with degraded {degraded:?}");
		}
	}
}
