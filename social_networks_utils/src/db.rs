use std::{
	collections::hash_map::Entry,
	path::{Path, PathBuf},
	sync::Mutex,
	time::Duration,
};

use color_eyre::eyre::{Result, WrapErr};
use grammers_session::{
	BoxFuture, Session, SessionData,
	types::{ChannelState, DcOption, PeerId, PeerInfo, UpdateState, UpdatesState},
};
use jiff::Timestamp;
use libsql::Connection;
use serde::{Serialize, de::DeserializeOwned};
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
		conn.busy_timeout(Duration::from_secs(30)).wrap_err("failed to set busy_timeout")?; // every daemon and the litestream replicator share the file
		conn.query("PRAGMA journal_mode = WAL", ()).await.wrap_err("failed to enable WAL")?;

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
		conn.execute(
			"CREATE TABLE IF NOT EXISTS twitter_schedule_attempts (
                attempted_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
                tweet_id     TEXT,
                error        TEXT
            )",
			(),
		)
		.await
		.wrap_err("failed to create twitter_schedule_attempts table")?;
		conn.execute_batch(
			"CREATE TABLE IF NOT EXISTS state (
                key        TEXT PRIMARY KEY,
                value      TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS telegram_session (
                name          TEXT PRIMARY KEY,
                home_dc       INTEGER NOT NULL,
                updates_state TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS telegram_dc_option (
                session TEXT NOT NULL,
                id      INTEGER NOT NULL,
                value   TEXT NOT NULL,
                PRIMARY KEY (session, id)
            );
            CREATE TABLE IF NOT EXISTS telegram_peer (
                session TEXT NOT NULL,
                id      TEXT NOT NULL,
                value   TEXT NOT NULL,
                PRIMARY KEY (session, id)
            );",
		)
		.await
		.wrap_err("failed to create state tables")?;

		let this = Self { conn };
		this.migrate_is_human_to_action().await?;
		this.refuse_unscoped_email_ids(&db_path).await?;
		Ok(this)
	}

	/// Ids are `<account>/<id>` since multi-account; which account wrote the older rows is known only to the operator.
	async fn refuse_unscoped_email_ids(&self, db_path: &Path) -> Result<()> {
		let mut rows = self
			.conn
			.query("SELECT count(*) FROM processed_emails WHERE instr(message_id, '/') = 0", ())
			.await
			.wrap_err("failed to count unscoped email ids")?;
		let row = rows.next().await.wrap_err("failed to read count")?.expect("count(*) always yields a row");
		let unscoped: i64 = row.get(0).wrap_err("failed to read count")?;
		if unscoped > 0 {
			color_eyre::eyre::bail!(
				"{unscoped} processed_emails rows predate per-account ids. Claim them for the account that wrote them:\n  sqlite3 {} \"UPDATE processed_emails SET message_id = '<account>/' || message_id WHERE instr(message_id, '/') = 0\"",
				db_path.display()
			);
		}
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

	/// A daemon's cursor or cache. `<key>.json` in the state dir predates the table: it is taken in once, keeping its mtime, and removed.
	pub async fn state<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
		Ok(match self.state_row(key).await? {
			Some((value, _)) => Some(serde_json::from_str(&value).wrap_err_with(|| format!("state `{key}` does not parse"))?),
			None => None,
		})
	}

	/// When `key` was last written.
	pub async fn state_updated_at(&self, key: &str) -> Result<Option<Timestamp>> {
		Ok(self.state_row(key).await?.map(|(_, at)| at))
	}

	pub async fn set_state<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
		self.put_state(key, &serde_json::to_string(value)?, Timestamp::now()).await
	}

	async fn put_state(&self, key: &str, value: &str, at: Timestamp) -> Result<()> {
		self.conn
			.execute(
				"INSERT INTO state (key, value, updated_at) VALUES (?1, ?2, ?3) ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
				libsql::params![key, value, at.to_string()],
			)
			.await
			.wrap_err_with(|| format!("failed to write state `{key}`"))?;
		Ok(())
	}

	async fn state_row(&self, key: &str) -> Result<Option<(String, Timestamp)>> {
		let mut rows = self
			.conn
			.query("SELECT value, updated_at FROM state WHERE key = ?1", [key])
			.await
			.wrap_err("failed to query state")?;
		if let Some(row) = rows.next().await.wrap_err("failed to read state row")? {
			let at = row.get_str(1).wrap_err("failed to read updated_at")?;
			return Ok(Some((
				row.get_str(0).wrap_err("failed to read value")?.to_owned(),
				at.parse().wrap_err_with(|| format!("updated_at `{at}` is not a timestamp"))?,
			)));
		}
		let Some(legacy) = legacy_state_file(&format!("{key}.json")) else {
			return Ok(None);
		};
		let value = std::fs::read_to_string(&legacy).wrap_err_with(|| format!("reading {}", legacy.display()))?;
		let at = Timestamp::try_from(legacy.metadata()?.modified()?)?;
		info!("Importing {} into the db", legacy.display());
		self.put_state(key, &value, at).await?;
		std::fs::remove_file(&legacy)?;
		Ok(Some((value, at)))
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

	/// Crash mid-post leaves the row with neither outcome, and it still counts as an attempt.
	pub async fn last_twitter_schedule_attempt(&self) -> Result<Option<Timestamp>> {
		let mut rows = self
			.conn
			.query("SELECT attempted_at FROM twitter_schedule_attempts ORDER BY rowid DESC LIMIT 1", ())
			.await
			.wrap_err("failed to query last twitter_schedule attempt")?;
		let Some(row) = rows.next().await.wrap_err("failed to read row")? else {
			return Ok(None);
		};
		let at = row.get_str(0).wrap_err("failed to read attempted_at")?;
		Ok(Some(at.parse().wrap_err_with(|| format!("attempted_at `{at}` is not a timestamp"))?))
	}

	/// Recorded before the post is sent, so a restart never re-sends one whose outcome was lost.
	pub async fn begin_twitter_schedule_attempt(&self) -> Result<i64> {
		self.conn
			.execute("INSERT INTO twitter_schedule_attempts DEFAULT VALUES", ())
			.await
			.wrap_err("failed to record twitter_schedule attempt")?;
		Ok(self.conn.last_insert_rowid())
	}

	pub async fn finish_twitter_schedule_attempt(&self, attempt: i64, outcome: Result<&str, &str>) -> Result<()> {
		let (tweet_id, error) = match outcome {
			Ok(tweet_id) => (Some(tweet_id), None),
			Err(error) => (None, Some(error)),
		};
		let updated = self
			.conn
			.execute(
				"UPDATE twitter_schedule_attempts SET tweet_id = ?2, error = ?3 WHERE rowid = ?1",
				libsql::params![attempt, tweet_id, error],
			)
			.await
			.wrap_err("failed to record twitter_schedule outcome")?;
		assert_eq!(updated, 1, "attempt {attempt} was begun on this db");
		Ok(())
	}

	async fn has_session(&self, name: &str) -> Result<bool> {
		let mut rows = self.conn.query("SELECT 1 FROM telegram_session WHERE name = ?1", [name]).await?;
		Ok(rows.next().await?.is_some())
	}

	/// `<name>.session` is grammers' own sqlite, from before sessions lived here. Its peers are not enumerable through `Session`; the dialog prefetch on connect re-caches them.
	async fn import_legacy_session(&self, name: &str) -> Result<bool> {
		let Some(file) = legacy_state_file(&format!("{name}.session")) else {
			return Ok(false);
		};
		info!("Importing {} into the db", file.display());
		let legacy = grammers_session::storages::SqliteSession::open(&file).await?;
		let mut data = SessionData {
			home_dc: legacy.home_dc_id()?,
			..Default::default()
		};
		for id in data.dc_options.keys().copied().collect::<Vec<_>>() {
			if let Some(option) = legacy.dc_option(id)? {
				data.dc_options.insert(id, option);
			}
		}
		data.updates_state = legacy.updates_state().await?;
		drop(legacy);
		self.write_session(name, &data).await?;
		for suffix in ["", "-wal", "-shm", "-journal"] {
			let f = PathBuf::from(format!("{}{suffix}", file.display()));
			if f.exists() {
				std::fs::remove_file(&f)?;
			}
		}
		Ok(true)
	}

	async fn write_session(&self, name: &str, data: &SessionData) -> Result<()> {
		let tx = self.conn.transaction().await?;
		tx.execute(
			"INSERT OR REPLACE INTO telegram_session (name, home_dc, updates_state) VALUES (?1, ?2, ?3)",
			libsql::params![name, data.home_dc, serde_json::to_string(&data.updates_state)?],
		)
		.await?;
		for option in data.dc_options.values() {
			tx.execute(
				"INSERT OR REPLACE INTO telegram_dc_option (session, id, value) VALUES (?1, ?2, ?3)",
				libsql::params![name, option.id, serde_json::to_string(option)?],
			)
			.await?;
		}
		for peer in data.peer_infos.values() {
			tx.execute(
				"INSERT OR REPLACE INTO telegram_peer (session, id, value) VALUES (?1, ?2, ?3)",
				libsql::params![name, serde_json::to_string(&peer.id())?, serde_json::to_string(peer)?],
			)
			.await?;
		}
		tx.commit().await?;
		Ok(())
	}

	async fn load_session(&self, name: &str) -> Result<SessionData> {
		let mut rows = self.conn.query("SELECT home_dc, updates_state FROM telegram_session WHERE name = ?1", [name]).await?;
		let row = rows.next().await?.expect("open() ensured the row");
		let mut data = SessionData {
			home_dc: row.get(0)?,
			dc_options: Default::default(),
			peer_infos: Default::default(),
			updates_state: serde_json::from_str(row.get_str(1)?)?,
		};
		let mut rows = self.conn.query("SELECT value FROM telegram_dc_option WHERE session = ?1", [name]).await?;
		while let Some(row) = rows.next().await? {
			let option: DcOption = serde_json::from_str(row.get_str(0)?)?;
			data.dc_options.insert(option.id, option);
		}
		let mut rows = self.conn.query("SELECT value FROM telegram_peer WHERE session = ?1", [name]).await?;
		while let Some(row) = rows.next().await? {
			let peer: PeerInfo = serde_json::from_str(row.get_str(0)?)?;
			data.peer_infos.insert(peer.id(), peer);
		}
		Ok(data)
	}
}

impl std::fmt::Debug for Database {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Database").finish()
	}
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
	#[error(transparent)]
	Sql(#[from] libsql::Error),
	#[error(transparent)]
	Json(#[from] serde_json::Error),
}
/// A telegram session as rows of the db, so it travels with the rest of the state. Served from memory, written through.
pub struct DbSession {
	db: Database,
	name: String,
	data: Mutex<SessionData>,
}
impl DbSession {
	/// `seed_from` is the session `name` starts as when it has none: the auth key travels with it, so no login and no new device.
	pub async fn open(db: Database, name: &str, seed_from: Option<&str>) -> Result<Self> {
		if !db.has_session(name).await? && !db.import_legacy_session(name).await? {
			match seed_from {
				Some(seed) if db.has_session(seed).await? || db.import_legacy_session(seed).await? => {
					info!("Seeding telegram session {name} from {seed}");
					let tx = db.conn.transaction().await?;
					for table in ["telegram_dc_option", "telegram_peer"] {
						tx.execute(
							&format!("INSERT INTO {table} (session, id, value) SELECT ?1, id, value FROM {table} WHERE session = ?2"),
							[name, seed],
						)
						.await?;
					}
					tx.execute(
						"INSERT INTO telegram_session (name, home_dc, updates_state) SELECT ?1, home_dc, updates_state FROM telegram_session WHERE name = ?2",
						[name, seed],
					)
					.await?;
					tx.commit().await?;
				}
				_ => {
					info!("No telegram session {name}, starting a fresh one");
					db.write_session(name, &SessionData::default()).await?;
				}
			}
		}
		let data = db.load_session(name).await?;
		Ok(Self {
			db,
			name: name.to_owned(),
			data: Mutex::new(data),
		})
	}

	fn data(&self) -> std::sync::MutexGuard<'_, SessionData> {
		self.data.lock().expect("no panic while holding the session lock")
	}
}

fn legacy_state_file(name: &str) -> Option<PathBuf> {
	xdg::BaseDirectories::with_prefix("social_networks").get_state_file(name).filter(|p| p.exists()) // xdg 3 does not check existence
}

impl Session for DbSession {
	type Error = SessionError;

	fn home_dc_id(&self) -> Result<i32, SessionError> {
		Ok(self.data().home_dc)
	}

	fn set_home_dc_id(&self, dc_id: i32) -> BoxFuture<'_, Result<(), SessionError>> {
		Box::pin(async move {
			self.data().home_dc = dc_id;
			self.db
				.conn
				.execute("UPDATE telegram_session SET home_dc = ?2 WHERE name = ?1", libsql::params![self.name.as_str(), dc_id])
				.await?;
			Ok(())
		})
	}

	fn dc_option(&self, dc_id: i32) -> Result<Option<DcOption>, SessionError> {
		Ok(self.data().dc_options.get(&dc_id).cloned())
	}

	fn set_dc_option(&self, dc_option: &DcOption) -> BoxFuture<'_, Result<(), SessionError>> {
		let dc_option = dc_option.clone();
		Box::pin(async move {
			let value = serde_json::to_string(&dc_option)?;
			self.data().dc_options.insert(dc_option.id, dc_option.clone());
			self.db
				.conn
				.execute(
					"INSERT OR REPLACE INTO telegram_dc_option (session, id, value) VALUES (?1, ?2, ?3)",
					libsql::params![self.name.as_str(), dc_option.id, value],
				)
				.await?;
			Ok(())
		})
	}

	fn peer(&self, peer: PeerId) -> BoxFuture<'_, Result<Option<PeerInfo>, SessionError>> {
		Box::pin(async move { Ok(self.data().peer_infos.get(&peer).cloned()) })
	}

	fn cache_peer(&self, peer: &PeerInfo) -> BoxFuture<'_, Result<(), SessionError>> {
		let peer = peer.clone();
		Box::pin(async move {
			// Written only when the merge changed something: every update re-caches the peers it names.
			let merged = match self.data().peer_infos.entry(peer.id()) {
				Entry::Vacant(v) => Some(v.insert(peer).clone()),
				Entry::Occupied(mut o) => {
					let before = o.get().clone();
					assert!(o.get_mut().extend_info(&peer), "entries are keyed by the id extend_info matches on");
					(*o.get() != before).then(|| o.get().clone())
				}
			};
			if let Some(merged) = merged {
				self.db
					.conn
					.execute(
						"INSERT OR REPLACE INTO telegram_peer (session, id, value) VALUES (?1, ?2, ?3)",
						libsql::params![self.name.as_str(), serde_json::to_string(&merged.id())?, serde_json::to_string(&merged)?],
					)
					.await?;
			}
			Ok(())
		})
	}

	fn updates_state(&self) -> BoxFuture<'_, Result<UpdatesState, SessionError>> {
		Box::pin(async move { Ok(self.data().updates_state.clone()) })
	}

	fn set_update_state(&self, update: UpdateState) -> BoxFuture<'_, Result<(), SessionError>> {
		Box::pin(async move {
			let value = {
				let mut data = self.data();
				let state = &mut data.updates_state;
				match update {
					UpdateState::All(all) => *state = all,
					UpdateState::Primary { pts, date, seq } => {
						state.pts = pts;
						state.date = date;
						state.seq = seq;
					}
					UpdateState::Secondary { qts } => state.qts = qts,
					UpdateState::Channel { id, pts } => {
						state.channels.retain(|c| c.id != id);
						state.channels.push(ChannelState { id, pts });
					}
				}
				serde_json::to_string(state)?
			};
			self.db
				.conn
				.execute("UPDATE telegram_session SET updates_state = ?2 WHERE name = ?1", libsql::params![self.name.as_str(), value])
				.await?;
			Ok(())
		})
	}
}
