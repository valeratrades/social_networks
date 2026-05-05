use color_eyre::eyre::{Result, WrapErr};
use libsql::Connection;
use tracing::info;

#[derive(Clone)]
pub struct Database {
	conn: Connection,
}

impl Database {
	pub async fn try_new() -> Result<Self> {
		let app_name = env!("CARGO_PKG_NAME");
		let xdg_dirs = xdg::BaseDirectories::with_prefix(app_name);
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
                is_human     INTEGER NOT NULL
            )",
			(),
		)
		.await
		.wrap_err("failed to create processed_emails table")?;

		Ok(Self { conn })
	}

	pub async fn is_email_processed(&self, message_id: &str) -> Result<bool> {
		let mut rows = self
			.conn
			.query("SELECT 1 FROM processed_emails WHERE message_id = ?1 LIMIT 1", [message_id])
			.await
			.wrap_err("failed to query is_email_processed")?;
		Ok(rows.next().await.wrap_err("failed to read row")?.is_some())
	}

	pub async fn mark_email_processed(&self, message_id: &str, from_email: &str, subject: &str, is_human: bool) -> Result<()> {
		self.conn
			.execute(
				"INSERT OR IGNORE INTO processed_emails (message_id, from_email, subject, is_human) VALUES (?1, ?2, ?3, ?4)",
				libsql::params![message_id, from_email, subject, is_human as i64],
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
