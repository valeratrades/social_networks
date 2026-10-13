//! Mint telegram sessions for other hosts from the local authorized one, by QR login: each new
//! session exports a login token and the local one accepts it, so no login code is involved. Prints
//! the JSON `telegram.sessions` takes. Every minted session is a new device in Telegram's list.
//!
//! `PHONE_NUMBER_FR=... TELEGRAM_API_HASH=... cargo r -p social_networks_adapters --example telegram_mint -- <host>...`
use std::sync::Arc;

use color_eyre::eyre::{Result, bail};
use futures::future::{Either, select};
use grammers_client::{Client, SenderPool, client::PasswordToken, tl};
use grammers_session::{Session as _, SessionData, storages::MemorySession};
use social_networks_utils::telegram_utils::{self, ConnectionConfig, TelegramConnection};

const USERNAME: &str = "@valeratrades";
const API_ID: i32 = 19721916;
/// `dms` and `channel_watch` each hold a key of their own.
const SUFFIXES: [&str; 2] = ["", "_channel_watch"];

#[tokio::main]
async fn main() -> Result<()> {
	color_eyre::install()?;
	tracing_subscriber::fmt().with_max_level(tracing::Level::WARN).with_writer(std::io::stderr).init();

	let hosts: Vec<String> = std::env::args().skip(1).collect();
	if hosts.is_empty() {
		bail!("name the hosts to mint for, e.g. `rpi5 evinvest-fallback`");
	}
	let api_hash = std::env::var("TELEGRAM_API_HASH")?;
	let TelegramConnection { client: source, mut runner, .. } = telegram_utils::connect(ConnectionConfig {
		username: USERNAME,
		phone: &std::env::var("PHONE_NUMBER_FR")?,
		api_id: API_ID,
		api_hash: &api_hash,
		session_suffix: "_rolodex",
		sessions: None,
	})
	.await?;

	let mut minted = serde_json::Map::new();
	for host in &hosts {
		for suffix in SUFFIXES {
			let name = format!("{USERNAME}{suffix}@{host}");
			let session = Arc::new(MemorySession::from(SessionData::default()));
			let SenderPool { runner: target_runner, handle, .. } = SenderPool::new(Arc::clone(&session), API_ID);
			let target = Client::new(handle);
			let target_runner = std::pin::pin!(target_runner.run());
			match select(std::pin::pin!(log_in(&source, &target, &session, &api_hash)), select(runner.as_mut(), target_runner)).await {
				Either::Left((result, _)) => result?,
				Either::Right(_) => bail!("MTProto runner exited while minting {name}"),
			}
			let dc_options: Vec<_> = (1..=5)
				.filter_map(|id| session.dc_option(id).transpose())
				.filter(|o| o.as_ref().is_ok_and(|o| o.auth_key.is_some()))
				.collect::<Result<_, _>>()?;
			minted.insert(name.clone(), serde_json::json!({ "home_dc": session.home_dc_id()?, "dc_options": dc_options }));
			eprintln!("minted {name}");
		}
	}
	println!("{}", serde_json::Value::Object(minted));
	Ok(())
}

async fn log_in(source: &Client, target: &Client, session: &MemorySession, api_hash: &str) -> Result<()> {
	use tl::enums::auth::LoginToken;
	let export = || tl::functions::auth::ExportLoginToken {
		api_id: API_ID,
		api_hash: api_hash.to_owned(),
		except_ids: vec![],
	};
	let LoginToken::Token(token) = target.invoke(&export()).await? else {
		bail!("a fresh session is already logged in");
	};
	source.invoke(&tl::functions::auth::AcceptLoginToken { token: token.token }).await?;

	let taken = match target.invoke(&export()).await {
		Ok(LoginToken::Success(_)) => Ok(()),
		Ok(LoginToken::MigrateTo(to)) => {
			session.set_home_dc_id(to.dc_id).await?;
			match target.invoke_in_dc(to.dc_id, &tl::functions::auth::ImportLoginToken { token: to.token }).await {
				Ok(LoginToken::Success(_)) => Ok(()),
				Ok(other) => bail!("importing the token in DC {} gave {other:?}", to.dc_id),
				Err(e) => Err(e),
			}
		}
		Ok(LoginToken::Token(_)) => bail!("the accepted token was not taken"),
		Err(e) => Err(e),
	};
	match taken {
		Ok(()) => Ok(()),
		Err(e) if e.is("SESSION_PASSWORD_NEEDED") => {
			let tl::enums::account::Password::Password(password) = target.invoke(&tl::functions::account::GetPassword {}).await?;
			eprint!("2FA password: ");
			let mut entered = String::new();
			std::io::stdin().read_line(&mut entered)?;
			target.check_password(PasswordToken::new(password), entered.trim()).await?;
			Ok(())
		}
		Err(e) => Err(e.into()),
	}
}
