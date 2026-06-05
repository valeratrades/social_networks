#![allow(unused_features)]
#![feature(default_field_values)]
pub mod client;
pub mod discord;
pub mod dm_event;
pub mod email;
pub mod telegram_channel_watch;
pub mod telegram_dms;
pub mod telegram_notifier;
pub mod twitter;
pub mod twitter_schedule;
pub mod youtube;

pub use client::{AdapterError, Client, alert};
pub use discord::DiscordDms;
pub use dm_event::DmEvent;
pub use email::EmailMonitor;
pub use telegram_channel_watch::TelegramChannelWatch;
pub use telegram_dms::TelegramDms;
pub use twitter::TwitterMonitor;
pub use twitter_schedule::TwitterSchedule;
pub use youtube::YoutubeMonitor;
