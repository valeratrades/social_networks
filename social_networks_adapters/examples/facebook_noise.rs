//! `Browsing::noise` for `<secs>` on the launched session, sending nothing; with the live config's pacing.
//! `nix develop -c cargo r -p social_networks_adapters --example facebook_noise -- <chrome> <secs>`

use std::{path::PathBuf, time::Duration};

use color_eyre::eyre::{Result, bail};
use social_networks_adapters::{
	facebook::{self, FacebookConfig},
	reach::Browsing as _,
};

#[tokio::main]
async fn main() -> Result<()> {
	color_eyre::install()?;
	tracing_subscriber::fmt().with_env_filter("info").init();
	let args: Vec<String> = std::env::args().skip(1).collect();
	let [chrome, secs] = &args[..] else { bail!("usage: facebook_noise <chrome> <secs>") };
	let behaviour = serde_json::json!({
		"active_hours": [0, 24],
		"burst_min": 40,
		"break_min": 10,
		"noise_share": 0.05,
		"load": { "per_hour": 150, "per_day": 1500, "dwell_secs": 2, "spread": 0.5 },
		"scroll": { "per_hour": 600, "per_day": 4000, "dwell_secs": 1.5, "spread": 0.5, "read_secs_per_item": 0.1 },
	});
	let config: FacebookConfig = serde_json::from_value(serde_json::json!({
		"attached": { "cdp_port": 0, "user_id": "", "behaviour": behaviour },
		"launched": { "chrome_executable": PathBuf::from(chrome), "behaviour": behaviour },
	}))?;
	facebook::with_sender(&config, async |fb| fb.noise(Duration::from_secs(secs.parse()?)).await).await
}
