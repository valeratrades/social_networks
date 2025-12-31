use clickhouse::{Client, Row};
use color_eyre::eyre::Result;
use serde::Deserialize;
use tracing::info;

use crate::config::ClickHouseConfig;

#[derive(Deserialize, Row)]
struct CountRow {
	count: u64,
}

#[derive(Deserialize, Row)]
struct MaxVersionRow {
	max_version: u32,
}

const MIGRATIONS: &[&str] = &[
	// Migration 0: Create processed_emails table with message_id as primary key
	r#"
CREATE TABLE IF NOT EXISTS social_networks.processed_emails (
    message_id String,
    processed_at DateTime DEFAULT now(),
    from_email String,
    subject String,
    is_human UInt8
) ENGINE = MergeTree()
ORDER BY message_id
PRIMARY KEY message_id
"#,
];

pub struct Database {
	client: Client,
	url: String,
}

impl std::fmt::Debug for Database {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Database").finish()
	}
}

impl Clone for Database {
	fn clone(&self) -> Self {
		Self {
			client: self.client.clone(),
			url: self.url.clone(),
		}
	}
}

impl Database {
	pub fn new(config: &ClickHouseConfig) -> Self {
		tracing::debug!(
			"ClickHouse config: url='{}', database='{}', user='{}', password='{}'",
			config.url,
			config.database,
			config.user,
			if config.password.is_empty() { "<empty>" } else { "<set>" }
		);

		let client = Client::default()
			.with_url(&config.url)
			.with_database(&config.database)
			.with_user(&config.user)
			.with_password(&config.password);

		Self { client, url: config.url.clone() }
	}

	/// Run all pending migrations
	pub async fn migrate(&self) -> Result<()> {
		info!("Running database migrations...");

		// First ensure the database exists
		self.ensure_database_exists().await?;

		// Ensure migrations table exists
		self.ensure_migrations_table_exists().await?;

		// Get current migration version
		let current_version = self.get_migration_version().await?;
		info!("Current migration version: {}", current_version);
		info!("Total migrations available: {}", MIGRATIONS.len());

		// Apply pending migrations
		let mut applied = 0;
		for (idx, migration) in MIGRATIONS.iter().enumerate() {
			let version = idx as i32;
			if version > current_version {
				info!("Applying migration {}", version);
				self.client.query(migration).execute().await?;
				self.record_migration(version as u32).await?;
				applied += 1;
			}
		}

		if applied > 0 {
			info!("Applied {} migration(s)", applied);
		} else {
			info!("No new migrations to apply");
		}
		info!("Migrations complete");
		Ok(())
	}

	async fn ensure_database_exists(&self) -> Result<()> {
		// Use a client without database set to create the database
		let client = self.client.clone().with_database("");
		let query = "CREATE DATABASE IF NOT EXISTS social_networks";

		client.query(query).execute().await.map_err(|e| {
			color_eyre::eyre::eyre!(
				"Failed to connect to ClickHouse server.\n\
				\n\
				Possible issues:\n\
				  1. ClickHouse server is not running\n\
				  2. Wrong URL configured (currently: {})\n\
				  3. Network/firewall blocking connection\n\
				\n\
				To fix:\n\
				  - Start ClickHouse: sudo systemctl start clickhouse-server\n\
				  - Check status: sudo systemctl status clickhouse-server\n\
				  - Verify URL in config file under [clickhouse] section\n\
				\n\
				Original error: {:#}",
				self.url,
				e
			)
		})?;
		Ok(())
	}

	async fn ensure_migrations_table_exists(&self) -> Result<()> {
		let query = r#"
CREATE TABLE IF NOT EXISTS social_networks.migrations (
    version UInt32,
    applied_at DateTime DEFAULT now()
) ENGINE = MergeTree()
ORDER BY version
PRIMARY KEY version
"#;
		self.client.query(query).execute().await?;
		Ok(())
	}

	async fn get_migration_version(&self) -> Result<i32> {
		// Check if there are any migrations recorded
		let count_query = "SELECT count() as count FROM social_networks.migrations";
		let count: u64 = match self.client.query(count_query).fetch_one::<CountRow>().await {
			Ok(row) => row.count,
			Err(_) => 0, // Table might not exist yet or no rows
		};

		if count == 0 {
			return Ok(-1);
		}

		// Get the latest migration version
		let version_query = "SELECT max(version) as max_version FROM social_networks.migrations";
		let row = self.client.query(version_query).fetch_one::<MaxVersionRow>().await?;

		Ok(row.max_version as i32)
	}

	async fn record_migration(&self, version: u32) -> Result<()> {
		let query = format!("INSERT INTO social_networks.migrations (version) VALUES ({})", version);
		self.client.query(&query).execute().await?;
		Ok(())
	}

	/// Check if an email message has been processed
	pub async fn is_email_processed(&self, message_id: &str) -> Result<bool> {
		let row = self
			.client
			.query("SELECT count() as count FROM social_networks.processed_emails WHERE message_id = ?")
			.bind(message_id)
			.fetch_one::<CountRow>()
			.await?;
		Ok(row.count > 0)
	}

	/// Mark an email as processed
	pub async fn mark_email_processed(&self, message_id: &str, from_email: &str, subject: &str, is_human: bool) -> Result<()> {
		self.client
			.query("INSERT INTO social_networks.processed_emails (message_id, from_email, subject, is_human) VALUES (?, ?, ?, ?)")
			.bind(message_id)
			.bind(from_email)
			.bind(subject)
			.bind(if is_human { 1u8 } else { 0u8 })
			.execute()
			.await?;
		Ok(())
	}
}
