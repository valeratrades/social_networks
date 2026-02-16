use std::path::PathBuf;

use color_eyre::Result;
use colored::Colorize;

use crate::config::AppConfig;

const SIZE_THRESHOLD_GB: f64 = 10.0;
/// All services that can be run as systemd units
const SERVICES: &[(&str, &str)] = &[
	("social_networks_dms", "DMs (Discord + Telegram)"),
	("social_networks_email", "Email"),
	("social_networks_telegram_channel_watch", "Telegram Channel Watch"),
	("social_networks_twitter", "Twitter Monitor"),
	("social_networks_twitter_schedule", "Twitter Schedule"),
	("social_networks_youtube", "YouTube Monitor"),
];
pub fn main(config: AppConfig) -> Result<()> {
	println!("{}", "=== Social Networks Health Check ===\n".bold().cyan());

	check_services();
	check_env_vars(&config);
	check_directories();

	println!();
	Ok(())
}

/// Checks if a systemd service is running
fn is_service_running(service_name: &str) -> bool {
	std::process::Command::new("systemctl")
		.args(["is-active", "--quiet", service_name])
		.status()
		.is_ok_and(|s| s.success())
}

/// Gets the directory size in bytes
fn get_dir_size(path: &PathBuf) -> std::io::Result<u64> {
	let mut total = 0;
	if path.is_dir() {
		for entry in std::fs::read_dir(path)? {
			let entry = entry?;
			let path = entry.path();
			if path.is_dir() {
				total += get_dir_size(&path)?;
			} else {
				total += entry.metadata()?.len();
			}
		}
	} else if path.is_file() {
		total += std::fs::metadata(path)?.len();
	}
	Ok(total)
}

fn bytes_to_human(bytes: u64) -> String {
	const KB: f64 = 1024.0;
	const MB: f64 = KB * 1024.0;
	const GB: f64 = MB * 1024.0;

	let bytes_f = bytes as f64;
	if bytes_f >= GB {
		format!("{:.2} GB", bytes_f / GB)
	} else if bytes_f >= MB {
		format!("{:.2} MB", bytes_f / MB)
	} else if bytes_f >= KB {
		format!("{:.2} KB", bytes_f / KB)
	} else {
		format!("{bytes} B")
	}
}

fn status_icon(ok: bool) -> colored::ColoredString {
	if ok { "✓".green() } else { "✗".red() }
}

fn check_services() {
	println!("{}", "Services:".bold());
	for (service_name, display_name) in SERVICES {
		let running = is_service_running(service_name);
		println!("  {} {}", status_icon(running), display_name);
	}
}

/// Required environment variables for various features
fn check_env_vars(config: &AppConfig) {
	println!("\n{}", "Environment & Config:".bold());

	// Check core telegram config (required for notifications)
	let telegram_ok = !config.telegram.bot_token.is_empty();
	println!("  {} Telegram bot token", status_icon(telegram_ok));

	// Check Discord config if dms is configured
	let discord_ok = !config.dms.discord.user_token.is_empty();
	println!("  {} Discord user token", status_icon(discord_ok));

	// Check Twitter config
	let twitter_bearer_ok = !config.twitter.bearer_token.is_empty();
	println!("  {} Twitter bearer token", status_icon(twitter_bearer_ok));

	let twitter_oauth_ok = config.twitter.oauth.as_ref().is_some_and(|o| !o.api_key.is_empty());
	println!("  {} Twitter OAuth config", status_icon(twitter_oauth_ok));

	// Check Email config
	let email_ok = config.email.is_some();
	println!("  {} Email config", status_icon(email_ok));

	// Check ClickHouse
	let clickhouse_ok = !config.clickhouse.url.is_empty();
	println!("  {} ClickHouse URL", status_icon(clickhouse_ok));

	// Check CLAUDE_TOKEN for email processing
	let claude_token_ok = config.email.as_ref().is_some_and(|e| e.claude_token.is_some()) || std::env::var("CLAUDE_TOKEN").is_ok();
	println!("  {} Claude token (for email classification)", status_icon(claude_token_ok));
}

fn check_directories() {
	println!("\n{}", "Directory Sizes:".bold());

	let app_name = env!("CARGO_PKG_NAME");
	let xdg_dirs = xdg::BaseDirectories::with_prefix(app_name);

	// State directory
	if let Some(state_dir) = xdg_dirs.get_state_home() {
		check_directory_size(&state_dir, "State directory");
	}

	// Config directory
	if let Some(config_dir) = xdg_dirs.get_config_home() {
		check_directory_size(&config_dir, "Config directory");
	}

	// Common log locations
	let home = std::env::var("HOME").unwrap_or_default();
	let log_paths = [
		PathBuf::from(format!("{home}/.local/share/{app_name}/logs")),
		PathBuf::from(format!("/var/log/{app_name}")),
		PathBuf::from(format!("{home}/.cache/{app_name}")),
	];

	for path in &log_paths {
		if path.exists() {
			check_directory_size(path, &format!("{}", path.display()));
		}
	}

	// journald logs for our services
	check_journald_size();
}

fn check_directory_size(path: &PathBuf, name: &str) {
	match get_dir_size(path) {
		Ok(size) => {
			let size_gb = size as f64 / (1024.0 * 1024.0 * 1024.0);
			let alarming = size_gb >= SIZE_THRESHOLD_GB;
			let size_str = bytes_to_human(size);
			if alarming {
				println!("  {} {} ({})", status_icon(false), name, size_str.red());
			} else {
				println!("  {} {} ({})", status_icon(true), name, size_str);
			}
		}
		Err(_) => {
			println!("  {} {} (unable to read)", status_icon(true), name);
		}
	}
}

fn check_journald_size() {
	// Check total journald disk usage for our services
	let output = std::process::Command::new("journalctl").args(["--disk-usage"]).output();

	if let Ok(output) = output
		&& output.status.success()
	{
		let stdout = String::from_utf8_lossy(&output.stdout);
		// Parse "Archived and active journals take up X on disk."
		if let Some(size_part) = stdout.split("take up ").nth(1)
			&& let Some(size_str) = size_part.split(" on disk").next()
		{
			println!("  {} journald total ({})", status_icon(true), size_str.trim());
		}
	}
}
