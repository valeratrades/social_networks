//! What stops a send loop before its recipient has to. Every message to a person is admitted here first.

use color_eyre::eyre::{Result, bail};
use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;
use social_networks_utils::db::Database;
use tracing::warn;
use v_utils::Timeframe;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct CircuitBreakers {
	pub per_recipient: PerRecipient,
}

/// A send that would be the `max + 1`th to one recipient within `window` refuses every send to them for `timeout`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct PerRecipient {
	pub max: usize = 10,
	pub window: Timeframe = Timeframe(10 * 60 * 1000),
	pub timeout: Timeframe = Timeframe(60 * 60 * 1000),
}

impl CircuitBreakers {
	/// `recipient` is `<surface>:<address>`. Recorded before the send: one that errors may still have been delivered.
	pub async fn admit(&self, db: &Database, recipient: &str) -> Result<()> {
		let now = Timestamp::now();
		let key = format!("breaker/{recipient}");
		if let Some(until) = db.state::<Timestamp>(&key).await?
			&& until > now
		{
			bail!("circuit breaker on {recipient} is open until {until}");
		}
		let PerRecipient { max, window, timeout } = &self.per_recipient;
		if db.sends_since(recipient, now - signed(window)).await? >= *max {
			let until = now + signed(timeout);
			warn!("{max} sends to {recipient} within {window}; refusing sends to them until {until}");
			db.set_state(&key, &until).await?;
			bail!("circuit breaker on {recipient} tripped: {max} sends within {window}, open until {until}");
		}
		db.record_send(recipient, now).await
	}
}

fn signed(tf: &Timeframe) -> SignedDuration {
	SignedDuration::try_from(tf.duration()).expect("a Timeframe is milliseconds, always in SignedDuration range")
}
