use clickhouse::Client;
use color_eyre::eyre::Result;
use tracing::info;

use crate::config::ClickHouseConfig;

const MIGRATIONS: &[&str] = &[
	// Migration 0: Create processed_emails table
	r#"
CREATE TABLE IF NOT EXISTS social_networks.processed_emails (
    message_id String,
    processed_at DateTime DEFAULT now(),
    from_email String,
    subject String,
    is_human Bool
) ENGINE = MergeTree()
ORDER BY (processed_at, message_id)
PRIMARY KEY (processed_at, message_id)
"#,
];

pub struct Database {
	client: Client,
}

impl std::fmt::Debug for Database {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Database").finish()
	}
}

impl Clone for Database {
	fn clone(&self) -> Self {
		Self { client: self.client.clone() }
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

		Self { client }
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
		client.query(query).execute().await?;
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
		let count_query = "SELECT count() as cnt FROM social_networks.migrations";
		let count: u64 = self.client.query(count_query).fetch_one::<u64>().await?;

		if count == 0 {
			return Ok(-1);
		}

		// Get the latest migration version
		let version_query = "SELECT max(version) as version FROM social_networks.migrations";
		let version: u32 = self.client.query(version_query).fetch_one::<u32>().await?;

		Ok(version as i32)
	}

	async fn record_migration(&self, version: u32) -> Result<()> {
		let query = format!("INSERT INTO social_networks.migrations (version) VALUES ({})", version);
		self.client.query(&query).execute().await?;
		Ok(())
	}

	/// Check if an email message has been processed
	pub async fn is_email_processed(&self, message_id: &str) -> Result<bool> {
		let query = format!("SELECT count() as cnt FROM social_networks.processed_emails WHERE message_id = '{}'", message_id);
		let count: u64 = self.client.query(&query).fetch_one::<u64>().await?;
		Ok(count > 0)
	}

	/// Mark an email as processed
	pub async fn mark_email_processed(&self, message_id: &str, from_email: &str, subject: &str, is_human: bool) -> Result<()> {
		let query = format!(
			"INSERT INTO social_networks.processed_emails (message_id, from_email, subject, is_human) VALUES ('{}', '{}', '{}', {})",
			message_id.replace('\'', "''"),
			from_email.replace('\'', "''"),
			subject.replace('\'', "''"),
			if is_human { 1 } else { 0 }
		);
		self.client.query(&query).execute().await?;
		Ok(())
	}
}
