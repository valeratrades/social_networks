//! Telling one address one thing: the axis with nothing to read and no session of ours in it.

pub mod discord;
pub mod email;
mod http;
mod rich;
pub mod slack;
pub mod telegram;
pub mod webhook;
pub mod webpush;

use jiff::SignedDuration;
use reqwest::Url;
pub use rich::{Entity, Rich};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
	Low,
	Normal,
	High,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Message {
	/// An email's subject, a discord or slack heading; telegram has no place for one.
	pub title: Option<String>,
	pub text: Rich,
	pub level: Level,
	pub links: Vec<Link>,
	/// Telegram only.
	pub buttons: Vec<Button>,
	/// Not on slack: an incoming webhook takes no files.
	pub photo: Option<Photo>,
	/// Email only.
	pub unsubscribe: Option<Url>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Link {
	pub label: String,
	pub url: Url,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Button {
	pub label: String,
	/// At most 64 bytes.
	pub data: String,
}

#[derive(Clone, Eq, PartialEq)]
pub struct Photo {
	pub name: String,
	pub mime: String,
	pub bytes: Vec<u8>,
}
impl Photo {
	fn part(&self, name: String) -> Result<reqwest::multipart::Part, Failure> {
		Ok(reqwest::multipart::Part::bytes(self.bytes.clone()).file_name(name).mime_str(&self.mime)?)
	}
}

impl std::fmt::Debug for Photo {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Photo")
			.field("name", &self.name)
			.field("mime", &self.mime)
			.field("bytes", &self.bytes.len())
			.finish()
	}
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum Failure {
	#[error("rate limited, retry after {}s", .0.as_secs())]
	RetryAfter(SignedDuration),
	/// Worth another try, ours to fix included: a revoked token, a login that stopped working.
	#[error("{0}")]
	Transient(String),
	/// The address is dead.
	#[error("{0}")]
	Gone(String),
	/// No try of the same message at the same address will do better.
	#[error("{0}")]
	Refused(String),
}
impl From<reqwest::Error> for Failure {
	fn from(e: reqwest::Error) -> Self {
		let e = e.without_url(); // the URL carries the token
		let causes = || std::iter::successors(std::error::Error::source(&e), |s| s.source());
		let why = causes().fold(e.to_string(), |why, s| format!("{why}: {s}"));
		match e.is_builder() || causes().any(|s| s.is::<webhook::NotAdmitted>()) {
			true => Self::Refused(why),
			false => Self::Transient(why),
		}
	}
}
