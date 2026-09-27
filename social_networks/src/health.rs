use std::path::PathBuf;

use color_eyre::Result;
use colored::Colorize;

use crate::config::AppConfig;

const SIZE_THRESHOLD_GB: f64 = 10.0;
pub fn main(config: AppConfig) -> Result<()> {
	println!("{}", "=== Social Networks Health Check ===\n".bold().cyan());

	check_env_vars(&config);
	check_directories(&config);

	println!();
	Ok(())
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
	let email_ok = !config.email.is_empty();
	println!("  {} Email config", status_icon(email_ok));

	// Check SQLite db
	let db_ok = xdg::BaseDirectories::with_prefix(env!("CARGO_PKG_NAME")).get_state_file("db.sqlite3").is_some();
	println!("  {} SQLite database", status_icon(db_ok));

	check_skool_cookie();
}

/// Skool rotates the session about every 3.5 days and only a headless chromium can mint the next
/// one, so its age is the one number that says whether that path still works.
fn check_skool_cookie() {
	let Some(path) = xdg::BaseDirectories::with_prefix(env!("CARGO_PKG_NAME")).get_state_file("skool_cookies.json") else {
		println!("  {} Skool cookie (never minted)", status_icon(false));
		return;
	};
	match path.metadata().and_then(|m| m.modified()).map(|t| t.elapsed().unwrap_or_default()) {
		Ok(age) => println!("  {} Skool cookie ({:.1}d old)", status_icon(true), age.as_secs_f64() / 86_400.0),
		Err(_) => println!("  {} Skool cookie (never minted)", status_icon(false)),
	}
}

fn check_directories(config: &AppConfig) {
	println!("\n{}", "Directory Sizes:".bold());

	for purpose in config.purposes.iter() {
		check_directory_size(&purpose.path, &format!("Purpose `{}`", purpose.name));
	}
	if let Some(venues) = &config.venues {
		check_directory_size(venues, "Venues");
	}

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
