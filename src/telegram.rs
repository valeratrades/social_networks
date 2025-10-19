use clap::Args;
use color_eyre::eyre::Result;

use crate::config::AppConfig;

#[derive(Args)]
pub struct TelegramArgs {}

pub fn main(_config: AppConfig, _args: TelegramArgs) -> Result<()> {
	println!("Hello, world!");
	Ok(())
}
