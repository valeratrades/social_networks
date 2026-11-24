//! A user's own address: signed, and never one of ours.

use std::net::IpAddr;

use ring::hmac;
use serde_json::json;
use social_networks_adapters::notify::{
	Failure,
	webhook::{Webhooks, public},
};

use super::Mock;

#[tokio::test]
async fn a_post_is_signed_over_its_time_and_body() {
	let mut mock = Mock::answering(204, "", "").await;
	let hooks = Webhooks::try_new(|_| true).unwrap();
	hooks.send(&mock.base.parse().unwrap(), b"secret", &json!({"a": 1})).await.unwrap();
	let request = mock.got().await;
	let (t, v1) = request.header("x-signature").split_once(",v1=").unwrap();
	let signed = [t.strip_prefix("t=").unwrap().as_bytes(), b".", &request.body].concat();
	let tag = (0..v1.len()).step_by(2).map(|i| u8::from_str_radix(&v1[i..i + 2], 16).unwrap()).collect::<Vec<_>>();
	hmac::verify(&hmac::Key::new(hmac::HMAC_SHA256, b"secret"), &signed, &tag).unwrap();
	assert_eq!(request.json(), json!({"a": 1}));
}

#[tokio::test]
async fn answers_are_classified() {
	let answered = async |status, headers: &str| {
		let mock = Mock::answering(status, headers, &"x".repeat(100_000)).await;
		Webhooks::try_new(|_| true).unwrap().send(&mock.base.parse().unwrap(), b"", &json!({})).await
	};
	assert_eq!(answered(200, "").await, Ok(()));
	assert!(matches!(answered(410, "").await, Err(Failure::Gone(_))));
	assert!(matches!(answered(302, "location: http://127.0.0.1:1/\r\n").await, Err(Failure::Refused(_))));
	assert!(matches!(answered(400, "").await, Err(Failure::Refused(_))));
	assert!(matches!(answered(503, "").await, Err(Failure::Transient(_))));
	assert!(matches!(answered(429, "retry-after: 5\r\n").await, Err(Failure::RetryAfter(_))));
}

#[tokio::test]
async fn only_public_addresses_are_connected_to() {
	let mut mock = Mock::answering(200, "", "").await;
	let port = mock.base.rsplit(':').next().unwrap().to_owned();
	let hooks = Webhooks::try_new(public).unwrap();
	for url in [
		mock.base.clone(),
		format!("https://127.0.0.1:{port}/"),
		format!("https://localhost:{port}/"), // through the resolver
		format!("https://2130706433:{port}/"),
		format!("https://0x7f.1:{port}/"),
		format!("https://[::1]:{port}/"),
		format!("https://[::ffff:127.0.0.1]:{port}/"),
		"https://169.254.169.254/latest".to_owned(),
		"http://example.com/".to_owned(),
		"ftp://example.com/".to_owned(),
	] {
		let refused = hooks.send(&url.parse().unwrap(), b"", &json!({})).await;
		assert!(matches!(refused, Err(Failure::Refused(_))), "{url}: {refused:?}");
	}
	assert!(mock.got.try_recv().is_err());
}

#[test]
fn public_is_the_internet_at_large() {
	for ip in [
		"0.0.0.0", "10.0.0.1", "100.64.0.1", "127.0.0.1", "169.254.1.1", "172.16.0.1", "192.0.0.1", "192.168.1.1", "198.18.0.1", "203.0.113.1", "224.0.0.1", "255.255.255.255", "::", "::1",
		"::7f00:1", "::ffff:10.0.0.1", "64:ff9b::a00:1", "2001::1", "2001:db8::1", "2002:a00:1::1", "fc00::1", "fe80::1", "ff02::1",
	] {
		assert!(!public(ip.parse::<IpAddr>().unwrap()), "{ip}");
	}
	for ip in ["1.1.1.1", "100.128.0.1", "2606:4700:4700::1111", "::ffff:1.1.1.1"] {
		assert!(public(ip.parse::<IpAddr>().unwrap()), "{ip}");
	}
}
