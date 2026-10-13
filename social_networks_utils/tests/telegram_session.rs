//! A minted key opens only under the session name, and so on the host, it was minted for.
use grammers_session::{Session as _, SessionData};
use social_networks_utils::db::{Database, DbSession};

#[tokio::test]
async fn minted_key_opens_only_on_its_host() {
	let state = std::env::temp_dir().join(format!("social_networks_test_{}", std::process::id()));
	// SAFETY: set before anything in this test binary reads the environment from another thread
	unsafe { std::env::set_var("XDG_STATE_HOME", &state) };

	let mut dc = SessionData::default().dc_options.remove(&4).expect("primary DCs are known statically");
	dc.auth_key = Some([7; 256]);
	let minted = serde_json::json!({ "@user_channel_watch@rpi5": { "home_dc": 4, "dc_options": [dc] } }).to_string();

	let check = async |name: &str, key: Option<[u8; 256]>, home_dc: i32| {
		let session = DbSession::open(Database::try_new().await.unwrap(), name, Some(&minted)).await.unwrap();
		assert_eq!(session.dc_option(4).unwrap().unwrap().auth_key, key, "{name}");
		assert_eq!(session.home_dc_id().unwrap(), home_dc, "{name}");
	};
	check("@user_channel_watch@rpi5", Some([7; 256]), 4).await;
	check("@user_channel_watch@evinvest-fallback", None, SessionData::default().home_dc).await;
	check("@user@rpi5", None, SessionData::default().home_dc).await;

	std::fs::remove_dir_all(&state).unwrap();
}
