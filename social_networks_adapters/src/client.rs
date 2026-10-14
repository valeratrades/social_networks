use std::convert::Infallible;

use color_eyre::eyre::Report;
use reqwest::StatusCode;
use thiserror::Error;
use tracing::error;

use crate::telegram_notifier::TelegramNotifier;

/// What a surface knows of its failures from the status and the body; `None` leaves it to the status alone.
pub(crate) type Known = fn(StatusCode, &str) -> Option<fn(String) -> Fault>;
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
/// Exits `EX_CONFIG`, which devops' observer answers by holding the deployment instead of restarting it: neither class is fixed by a restart.
impl v_utils::utils::SysexitCode for AdapterError {
	fn sysexit(&self) -> v_utils::utils::Sysexit {
		v_utils::utils::Sysexit::Config
	}
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

/// Pass a successful response through; classify a failed one by what the surface knows of it, else by its status.
pub(crate) async fn ok_or_classify(response: reqwest::Response, op: &str, known: Known) -> Result<reqwest::Response, Fault> {
	let status = response.status();
	if status.is_success() {
		return Ok(response);
	}
	let body = response.text().await.map_err(Fault::transient)?;
	Err(http_fault(status, &body, format!("{op}: {status}: {body}"), known))
}

fn http_fault(status: StatusCode, body: &str, detail: String, known: Known) -> Fault {
	if let Some(class) = known(status, body) {
		return class(detail);
	}
	match status.as_u16() {
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
	fn responses_classify_per_surface() {
		let google = |reason: &str| format!(r#"{{"error": {{"code": 403, "errors": [{{"reason": "{reason}"}}], "details": [{{"@type": "type.googleapis.com/google.rpc.Help"}}]}}}}"#);
		let class = |code: u16, body: &str, known: Known| match http_fault(StatusCode::from_u16(code).unwrap(), body, String::new(), known) {
			Fault::Transient(_) => "transient",
			Fault::Degraded(_) => "degraded",
			Fault::Auth(_) => "auth",
			Fault::Unhandled(_) => "unhandled",
		};
		let api_key_blocked = r#"{"error": {"code": 403, "errors": [{"reason": "forbidden"}], "details": [{"reason": "API_KEY_SERVICE_BLOCKED"}]}}"#;
		for (code, body, known, expected) in [
			(503, "", crate::twitter::known as Known, "transient"),
			(429, "", crate::twitter::known, "transient"),
			(401, "", crate::twitter::known, "auth"),
			(402, "", crate::twitter::known, "degraded"),
			(404, "", crate::twitter::known, "unhandled"),
			(404, "", crate::youtube::rss_known, "degraded"),
			(402, "", crate::youtube::rss_known, "unhandled"),
			(503, "", crate::youtube::rss_known, "transient"),
			(403, &google("quotaExceeded"), crate::youtube::api_known, "degraded"),
			(403, &google("rateLimitExceeded"), crate::youtube::api_known, "degraded"),
			(400, &google("keyInvalid"), crate::youtube::api_known, "auth"),
			(403, &google("accessNotConfigured"), crate::youtube::api_known, "auth"),
			(403, api_key_blocked, crate::youtube::api_known, "auth"),
			(404, &google("playlistNotFound"), crate::youtube::api_known, "unhandled"),
			(400, &google("invalidParameter"), crate::youtube::api_known, "unhandled"),
			(429, "", crate::youtube::api_known, "transient"),
			(502, "<html>bad gateway</html>", crate::youtube::api_known, "transient"),
			(404, "", crate::youtube::api_known, "unhandled"),
		] {
			assert_eq!(class(code, body, known), expected, "{code} {body}");
		}
	}
}
