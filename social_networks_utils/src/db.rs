use color_eyre::eyre::{Result, WrapErr};
use libsql::Connection;
use tracing::info;

#[derive(Clone)]
pub struct Database {
	conn: Connection,
}

impl Database {
	pub async fn try_new() -> Result<Self> {
		let xdg_dirs = xdg::BaseDirectories::with_prefix("social_networks");
		let db_path = xdg_dirs.place_state_file("db.sqlite3")?;
		info!("Opening SQLite database at {}", db_path.display());

		let db = libsql::Builder::new_local(&db_path).build().await.wrap_err("failed to open SQLite database")?;
		let conn = db.connect().wrap_err("failed to get connection")?;

		conn.execute(
			"CREATE TABLE IF NOT EXISTS processed_emails (
                message_id   TEXT PRIMARY KEY,
                processed_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
                from_email   TEXT NOT NULL,
                subject      TEXT NOT NULL,
                action       TEXT NOT NULL
            )",
			(),
		)
		.await
		.wrap_err("failed to create processed_emails table")?;

		// Machine-only, so a full regeneration of a rolodex person file can never clobber it.
		conn.execute(
			"CREATE TABLE IF NOT EXISTS rolodex_checkpoints (
                person TEXT NOT NULL,
                source TEXT NOT NULL,
                cursor TEXT NOT NULL,
                PRIMARY KEY (person, source)
            )",
			(),
		)
		.await
		.wrap_err("failed to create rolodex_checkpoints table")?;

		let this = Self { conn };
		this.migrate_is_human_to_action().await?;
		Ok(this)
	}

	/// Last item seen for `person` on `source`. Opaque to the db: a discord snowflake and a
	/// telegram message id are both just the string the fetcher will compare against.
	pub async fn rolodex_checkpoint(&self, person: &str, source: &str) -> Result<Option<String>> {
		let mut rows = self
			.conn
			.query("SELECT cursor FROM rolodex_checkpoints WHERE person = ?1 AND source = ?2", [person, source])
			.await
			.wrap_err("failed to query rolodex_checkpoints")?;
		match rows.next().await.wrap_err("failed to read row")? {
			Some(row) => Ok(Some(row.get_str(0).wrap_err("failed to read cursor")?.to_string())),
			None => Ok(None),
		}
	}

	pub async fn set_rolodex_checkpoint(&self, person: &str, source: &str, cursor: &str) -> Result<()> {
		self.conn
			.execute(
				"INSERT OR REPLACE INTO rolodex_checkpoints (person, source, cursor) VALUES (?1, ?2, ?3)",
				libsql::params![person, source, cursor],
			)
			.await
			.wrap_err("failed to execute set_rolodex_checkpoint")?;
		Ok(())
	}

	/// Pre-`action` DBs carried a boolean `is_human`. Dropping the table instead would re-notify the whole unread inbox.
	async fn migrate_is_human_to_action(&self) -> Result<()> {
		let mut rows = self
			.conn
			.query("SELECT name FROM pragma_table_info('processed_emails') WHERE name IN ('is_human', 'action')", ())
			.await
			.wrap_err("failed to inspect processed_emails schema")?;
		let mut has_is_human = false;
		let mut has_action = false;
		while let Some(row) = rows.next().await.wrap_err("failed to read schema row")? {
			match row.get_str(0).wrap_err("failed to read column name")? {
				"is_human" => has_is_human = true,
				"action" => has_action = true,
				other => unreachable!("query filters to is_human/action, got {other}"),
			}
		}
		if !has_is_human {
			return Ok(());
		}
		info!("Migrating processed_emails.is_human -> action");
		if !has_action {
			self.conn
				.execute("ALTER TABLE processed_emails ADD COLUMN action TEXT NOT NULL DEFAULT 'discard'", ())
				.await
				.wrap_err("failed to add action column")?;
		}
		self.conn
			.execute("UPDATE processed_emails SET action = CASE is_human WHEN 1 THEN 'important' ELSE 'discard' END", ())
			.await
			.wrap_err("failed to backfill action column")?;
		self.conn
			.execute("ALTER TABLE processed_emails DROP COLUMN is_human", ())
			.await
			.wrap_err("failed to drop is_human column")?;
		Ok(())
	}

	pub async fn is_email_processed(&self, message_id: &str) -> Result<bool> {
		let mut rows = self
			.conn
			.query("SELECT 1 FROM processed_emails WHERE message_id = ?1 LIMIT 1", [message_id])
			.await
			.wrap_err("failed to query is_email_processed")?;
		Ok(rows.next().await.wrap_err("failed to read row")?.is_some())
	}

	pub async fn mark_email_processed(&self, message_id: &str, from_email: &str, subject: &str, action: &str) -> Result<()> {
		self.conn
			.execute(
				"INSERT OR IGNORE INTO processed_emails (message_id, from_email, subject, action) VALUES (?1, ?2, ?3, ?4)",
				libsql::params![message_id, from_email, subject, action],
			)
			.await
			.wrap_err("failed to execute mark_email_processed")?;
		Ok(())
	}
}

impl std::fmt::Debug for Database {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Database").finish()
	}
}
