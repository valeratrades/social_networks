use std::time::Duration;

use social_networks_adapters::breaker::{CircuitBreakers, PerRecipient};
use social_networks_utils::db::Database;
use v_utils::Timeframe;

#[tokio::test]
async fn a_recipient_over_the_limit_is_refused_until_the_timeout_ends() {
	let state = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("breaker_{}", std::process::id()));
	// SAFETY: the only test in this binary, set before anything reads the environment
	unsafe { std::env::set_var("XDG_STATE_HOME", &state) };
	let db = Database::try_new().await.unwrap();
	let breakers = CircuitBreakers {
		per_recipient: PerRecipient {
			max: 3,
			window: Timeframe(300),
			timeout: Timeframe(600),
		},
	};

	for _ in 0..3 {
		breakers.admit(&db, "email:a@x").await.unwrap();
	}
	assert!(breakers.admit(&db, "email:a@x").await.is_err(), "4th within the window trips");
	breakers.admit(&db, "email:b@x").await.unwrap();

	tokio::time::sleep(Duration::from_millis(400)).await;
	assert!(breakers.admit(&db, "email:a@x").await.is_err(), "window has passed, timeout has not");

	tokio::time::sleep(Duration::from_millis(300)).await;
	breakers.admit(&db, "email:a@x").await.unwrap();

	std::fs::remove_dir_all(&state).unwrap();
}
