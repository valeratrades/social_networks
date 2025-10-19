use clap::Args;
use color_eyre::eyre::Result;

use crate::config::AppConfig;

#[derive(Args)]
pub struct DiscordArgs {}

pub fn main(_config: AppConfig, _args: DiscordArgs) -> Result<()> {
	println!("Hello, world!");
	Ok(())
}
