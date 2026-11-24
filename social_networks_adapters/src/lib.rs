#![allow(unused_features)]
#![feature(default_field_values)]
#[cfg(feature = "full")]
pub mod behaviour;
#[cfg(feature = "full")]
pub mod breaker;
#[cfg(feature = "full")]
pub mod client;
#[cfg(feature = "full")]
pub mod discord;
#[cfg(feature = "full")]
pub mod dm_event;
#[cfg(feature = "full")]
pub mod email;
#[cfg(feature = "full")]
pub mod facebook;
#[cfg(feature = "full")]
pub mod github;
#[cfg(feature = "full")]
pub mod instagram;
#[cfg(feature = "full")]
pub mod linkedin;
#[cfg(feature = "full")]
pub mod llm;
#[cfg(feature = "full")]
mod nominatim;
#[cfg(feature = "notify")]
pub mod notify;
#[cfg(feature = "full")]
pub mod reach;
#[cfg(feature = "full")]
pub mod skool;
#[cfg(feature = "full")]
pub mod telegram_channel_watch;
#[cfg(feature = "full")]
pub mod telegram_dms;
#[cfg(feature = "full")]
pub mod telegram_notifier;
#[cfg(feature = "full")]
pub mod twitter;
#[cfg(feature = "full")]
pub mod twitter_schedule;
#[cfg(feature = "full")]
pub mod youtube;

#[cfg(feature = "full")]
pub use client::{AdapterError, Client, alert, install_panic_alert};
#[cfg(feature = "full")]
pub use discord::DiscordDms;
#[cfg(feature = "full")]
pub use dm_event::DmEvent;
#[cfg(feature = "full")]
pub use email::EmailMonitor;
#[cfg(feature = "full")]
pub use skool::SkoolDms;
#[cfg(feature = "full")]
pub use telegram_channel_watch::TelegramChannelWatch;
#[cfg(feature = "full")]
pub use telegram_dms::TelegramDms;
#[cfg(feature = "full")]
pub use twitter::TwitterMonitor;
#[cfg(feature = "full")]
pub use twitter_schedule::TwitterSchedule;
#[cfg(feature = "full")]
pub use youtube::YoutubeMonitor;

/// With the capture's paths, which `browser_manipulation` keeps in the diagnostic's help rather than the message.
#[cfg(feature = "full")]
fn browser_failure(e: browser_manipulation::Error) -> color_eyre::Report {
	let help = miette::Diagnostic::help(&e).map(|h| h.to_string());
	let report = color_eyre::Report::new(e);
	match help {
		Some(help) => color_eyre::Section::note(report, help),
		None => report,
	}
}
