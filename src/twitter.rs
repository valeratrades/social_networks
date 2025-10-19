use clap::Args;
use color_eyre::eyre::Result;

use crate::config::AppConfig;

#[derive(Args)]
pub struct TwitterArgs {}

pub fn main(_config: AppConfig, _args: TwitterArgs) -> Result<()> {
	println!("Hello, world!");
	Ok(())
}
