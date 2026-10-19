//! What instagram's web profile answer reads as, over the shape captured from the live endpoint.

use jiff::Timestamp;
use social_networks_adapters::{
	instagram::{Instagram, newest_post},
	reach::{Profiles, Window},
};

fn fixture(name: &str) -> String {
	std::fs::read_to_string(format!("{}/tests/fixtures/instagram/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

#[test]
fn the_newest_post_is_the_latest_not_the_first() {
	let at = newest_post(200, &fixture("public_pinned_first.json")).unwrap().expect("three public posts");
	assert_eq!(at, Timestamp::from_second(1791993604).unwrap(), "a pinned post from 2018 leads the timeline");
}

#[test]
fn nothing_to_see_is_no_moment_not_an_old_one() {
	for body in ["private.json", "no_posts.json", "missing.json"] {
		assert_eq!(newest_post(200, &fixture(body)).unwrap(), None, "{body}");
	}
	assert_eq!(newest_post(404, "<!DOCTYPE html><title>Page Not Found</title>").unwrap(), None);
}

/// A wall or a reshaped payload read as silence would freeze the tag without a sound, and a request that
/// did not complete is not a no.
#[test]
fn a_refusal_or_a_reshape_is_an_error() {
	assert!(newest_post(401, &fixture("login_wall.json")).is_err());
	assert!(newest_post(429, "").is_err());
	assert!(newest_post(500, "").is_err());
	assert!(newest_post(200, &fixture("reshaped.json")).is_err());
	assert!(newest_post(200, "<html>").is_err());
}

#[tokio::test]
#[ignore = "reads instagram live"]
async fn a_public_profile_answers_over_the_mobile_host() {
	let at = Instagram::default().profile("instagram", Window::probe()).await.unwrap().active;
	assert!(at.expect("the account posts") > "2026-01-01T00:00:00Z".parse().unwrap(), "{at:?}");
}
