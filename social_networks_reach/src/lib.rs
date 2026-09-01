#![feature(default_field_values)]
#![doc = include_str!("../README.md")]
pub mod history;
pub mod venue;

use std::{future::Future, path::PathBuf};

use color_eyre::eyre::{Result, eyre};
use futures::future::{Either, select};
use grammers_client::Client;
use social_networks_adapters::telegram_dms::TelegramConfig;
use social_networks_utils::telegram_utils::{self, ConnectionConfig, TelegramConnection};
use v_utils::macros::MyConfigPrimitives;

/// `[rolodex] path` is the directory of person files, and of the venue transcripts under
/// `venues/`. No default: a present-but-pathless section is a config mistake, not a request for a
/// guess.
#[derive(Clone, Debug, Default, MyConfigPrimitives)]
pub struct RolodexConfig {
	pub path: PathBuf,
}

/// The MTProto runner has to be polled alongside whatever uses the client, so every telegram read on
/// this axis is wrapped rather than owning a client of its own.
///
/// Its own session file, seeded from the `dms` daemon's on first use: same authorization, no write
/// contention with the daemon.
pub async fn with_telegram<T, F: Future<Output = Result<T>>>(config: &TelegramConfig, f: impl FnOnce(Client) -> F) -> Result<T> {
	let TelegramConnection { client, mut runner, .. } = telegram_utils::connect(ConnectionConfig {
		username: &config.username,
		phone: &config.phone,
		api_id: config.api_id,
		api_hash: &config.api_hash,
		session_suffix: "_rolodex",
		seed_from: Some("_dm"),
	})
	.await?;
	match select(std::pin::pin!(f(client)), runner.as_mut()).await {
		Either::Left((result, _)) => result,
		Either::Right(((), _)) => Err(eyre!("MTProto runner exited mid-call")),
	}
}
