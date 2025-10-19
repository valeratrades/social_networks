use clap::Args;
use color_eyre::eyre::Result;

use crate::config::AppConfig;

#[derive(Args)]
pub struct YoutubeArgs {}

pub fn main(_config: AppConfig, _args: YoutubeArgs) -> Result<()> {
	println!("Hello, world!");
	Ok(())
}
