use std::time::Duration;

use jiff::SignedDuration;
use reqwest::{Client, ClientBuilder, RequestBuilder};

use super::Failure;

/// For what a platform's own API answers.
pub(super) const ANSWER: usize = 4 << 20;
/// A 429 that names no wait.
const WAIT: SignedDuration = SignedDuration::from_secs(60);

pub(super) fn client() -> ClientBuilder {
	provider();
	Client::builder()
		.connect_timeout(Duration::from_secs(5))
		.timeout(Duration::from_secs(15))
		.redirect(reqwest::redirect::Policy::none())
		.user_agent(concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION")))
}

pub(super) fn provider() {
	let _ = rustls::crypto::ring::default_provider().install_default(); // Err is "one is installed already", which is all that is needed
}

pub(super) struct Answer {
	pub status: u16,
	pub retry_after: Option<SignedDuration>,
	pub body: Vec<u8>,
}
impl Answer {
	pub async fn of(request: RequestBuilder, cap: usize) -> Result<Self, Failure> {
		let mut response = request.send().await?;
		let mut answer = Self {
			status: response.status().as_u16(),
			retry_after: response
				.headers()
				.get("retry-after")
				.and_then(|v| v.to_str().ok()?.parse().ok()) // the HTTP-date form is as good as no wait named
				.map(SignedDuration::from_secs),
			body: Vec::new(),
		};
		// an error here is an answer cut short, judged by its status: sent again, an accepted message would arrive twice
		while answer.body.len() < cap
			&& let Ok(Some(chunk)) = response.chunk().await
		{
			answer.body.extend_from_slice(&chunk[..chunk.len().min(cap - answer.body.len())]);
		}
		Ok(answer)
	}

	pub fn ok(&self) -> bool {
		(200..300).contains(&self.status)
	}

	pub fn json(&self) -> Option<serde_json::Value> {
		serde_json::from_slice(&self.body).ok() // an answer that is not JSON is judged by its status
	}

	pub fn why(&self, what: &str) -> String {
		let said: String = String::from_utf8_lossy(&self.body).chars().take(200).collect();
		format!("{what}: {} {}", self.status, said.trim()).trim_end().to_owned()
	}

	/// What the status alone says of an answer that is no success.
	pub fn failure(&self, what: &str) -> Failure {
		match self.status {
			429 => Failure::RetryAfter(self.retry_after.unwrap_or(WAIT).clamp(SignedDuration::from_secs(1), SignedDuration::from_hours(1))),
			408 | 500..=599 => Failure::Transient(self.why(what)),
			_ => Failure::Refused(self.why(what)),
		}
	}
}
