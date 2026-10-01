//! What stops a send loop before its recipient has to. Every message to a person is admitted here first.

use std::collections::BTreeMap;

use color_eyre::eyre::{Result, bail, eyre};
use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;
use social_networks_utils::db::Database;
use tracing::warn;
use v_utils::Timeframe;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct CircuitBreakers {
	pub per_recipient: PerRecipient,
	/// Surface → its cap across every recipient. A surface without one is uncapped for `dm`, and refused by `send`.
	pub per_surface: BTreeMap<String, PerSurface>,
}

/// A send that would be the `max + 1`th to one recipient within `window` refuses every send to them for `timeout`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct PerRecipient {
	pub max: usize = 10,
	pub window: Timeframe = Timeframe(10 * 60 * 1000),
	pub timeout: Timeframe = Timeframe(60 * 60 * 1000),
}

/// At most `max` sends over the surface within `window`. Refuses, never trips: a full budget is the plan working.
#[derive(Clone, Debug, Deserialize)]
pub struct PerSurface {
	pub max: usize,
	pub window: Timeframe,
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
		let (surface, _) = recipient.split_once(':').unwrap_or_else(|| panic!("`{recipient}` is not `<surface>:<address>`"));
		if let Some(PerSurface { max, window }) = self.per_surface.get(surface)
			&& self.remaining(db, surface).await? == 0
		{
			bail!("{surface} is out of budget: {max} sends within {window}");
		}
		db.record_send(recipient, now).await
	}

	pub async fn remaining(&self, db: &Database, surface: &str) -> Result<usize> {
		let PerSurface { max, window } = self.per_surface.get(surface).ok_or_else(|| eyre!("no `circuit_breakers.per_surface.{surface}` budget"))?;
		Ok(max.saturating_sub(db.sends_since(surface, Timestamp::now() - signed(window)).await?))
	}
}

fn signed(tf: &Timeframe) -> SignedDuration {
	SignedDuration::try_from(tf.duration()).expect("a Timeframe is milliseconds, always in SignedDuration range")
}
