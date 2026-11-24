//! What a browser makes of a push, by the receiving half of RFC 8291, itself held to the RFC's example.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jiff::{SignedDuration, Timestamp};
use p256::{
	PublicKey, SecretKey,
	ecdsa::{Signature, VerifyingKey, signature::Verifier},
};
use ring::{aead, hkdf};
use serde_json::Value;
use social_networks_adapters::notify::{
	Failure, Level,
	webpush::{Payload, Pusher, Subscription, Vapid},
};

use super::Mock;

const UA_PRIVATE: &str = "q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94";
const UA_PUBLIC: &str = "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
const AUTH: &str = "BTBZMqHH6r4Tts7J_aSIgg";
const FCM: &str = "https://fcm.googleapis.com/fcm/send/abc";

struct Len(usize);
impl hkdf::KeyType for Len {
	fn len(&self) -> usize {
		self.0
	}
}

fn expand<const N: usize>(salt: &[u8], ikm: &[u8], info: &[u8]) -> [u8; N] {
	let mut out = [0; N];
	let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(ikm);
	prk.expand(&[info], Len(N)).unwrap().fill(&mut out).unwrap();
	out
}

fn opened(body: &[u8]) -> Vec<u8> {
	let d = |s: &str| URL_SAFE_NO_PAD.decode(s).unwrap();
	let (salt, rest) = body.split_at(16);
	let (size, rest) = rest.split_at(4);
	assert_eq!(size, 4096u32.to_be_bytes());
	assert!(body.len() <= 4096);
	assert!(rest[0] == 65);
	let (theirs, record) = rest[1..].split_at(65);
	let shared = SecretKey::from_slice(&d(UA_PRIVATE)).unwrap().diffie_hellman(&PublicKey::from_sec1_bytes(theirs).unwrap());
	let ikm: [u8; 32] = expand(&d(AUTH), shared.raw_secret_bytes(), &[b"WebPush: info\0".as_slice(), &d(UA_PUBLIC), theirs].concat());
	let cek: [u8; 16] = expand(salt, &ikm, b"Content-Encoding: aes128gcm\0");
	let nonce: [u8; 12] = expand(salt, &ikm, b"Content-Encoding: nonce\0");
	let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek).unwrap());
	let mut record = record.to_vec();
	let plain = key.open_in_place(aead::Nonce::assume_unique_for_key(nonce), aead::Aad::empty(), &mut record).unwrap();
	plain.strip_suffix(&[2]).expect("the last record's delimiter").to_vec()
}

#[test]
fn the_rfcs_example_opens() {
	let body = "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN";
	assert_eq!(opened(&URL_SAFE_NO_PAD.decode(body).unwrap()), b"When I grow up, I want to be a watermelon");
}

fn payload(body: String) -> Payload {
	Payload {
		title: "New lead".into(),
		body,
		url: "https://panel.example/leads/1".into(),
		tag: "lead-1".into(),
		level: Level::High,
	}
}

async fn pushed(mock: &Mock, body: String) -> Result<(), Failure> {
	let pusher = Pusher::try_new(Vapid::from_seed([7; 32]), "mailto:a@b.example", Some(mock.base.parse().unwrap())).unwrap();
	let to = Subscription::try_new(FCM, UA_PUBLIC, AUTH).unwrap();
	pusher.send(&to, &payload(body), SignedDuration::from_mins(5), Level::High, Some("lead-1")).await
}

#[tokio::test]
async fn a_push_opens_and_its_signature_verifies() {
	let mut mock = Mock::answering(201, "", "").await;
	pushed(&mock, "Olga, +7".into()).await.unwrap();
	let request = mock.got().await;
	let plain: Value = serde_json::from_slice(&opened(&request.body)).unwrap();
	assert_eq!(plain, serde_json::to_value(payload("Olga, +7".into())).unwrap());
	assert!(request.head.starts_with("POST /fcm/send/abc "));
	assert_eq!(["ttl", "urgency", "topic", "content-encoding"].map(|h| request.header(h)), ["300", "high", "lead-1", "aes128gcm"]);

	let (token, key) = request.header("authorization").strip_prefix("vapid t=").unwrap().split_once(", k=").unwrap();
	assert_eq!(key, Vapid::from_seed([7; 32]).public());
	let (signed, signature) = token.rsplit_once('.').unwrap();
	let d = |s: &str| URL_SAFE_NO_PAD.decode(s).unwrap();
	VerifyingKey::from_sec1_bytes(&d(key))
		.unwrap()
		.verify(signed.as_bytes(), &Signature::from_slice(&d(signature)).unwrap())
		.unwrap();
	let claims: Value = serde_json::from_slice(&d(signed.split_once('.').unwrap().1)).unwrap();
	assert_eq!((&claims["aud"], &claims["sub"]), (&"https://fcm.googleapis.com".into(), &"mailto:a@b.example".into()));
	let left = claims["exp"].as_i64().unwrap() - Timestamp::now().as_second();
	assert!((1..=86_400).contains(&left), "a push service refuses one good for over a day");

	pushed(&mock, "ё".repeat(5000)).await.unwrap();
	let plain: Payload = {
		let plain: Value = serde_json::from_slice(&opened(&mock.got().await.body)).unwrap();
		payload(plain["body"].as_str().unwrap().to_owned())
	};
	assert!(plain.body.ends_with('…'));
}

#[test]
fn a_seed_past_the_curves_order_is_still_a_key() {
	assert_eq!(Vapid::from_seed([0xff; 32]).public(), Vapid::from_seed([0xff; 32]).public());
	assert_ne!(Vapid::from_seed([0; 32]).public(), Vapid::from_seed([0xff; 32]).public());
}

#[tokio::test]
async fn answers_are_classified() {
	let answered = async |status, headers: &str| pushed(&Mock::answering(status, headers, "").await, String::new()).await.unwrap_err();
	assert!(matches!(answered(410, "").await, Failure::Gone(_)));
	assert!(matches!(answered(404, "").await, Failure::Gone(_)));
	assert!(matches!(answered(403, "").await, Failure::Transient(_)), "our signature, not the subscription");
	assert_eq!(answered(429, "retry-after: 9\r\n").await, Failure::RetryAfter(SignedDuration::from_secs(9)));
	assert!(matches!(answered(413, "").await, Failure::Refused(_)));
}

#[test]
fn only_a_push_services_endpoint_is_subscribed() {
	for endpoint in [
		"http://fcm.googleapis.com/x",
		"https://fcm.googleapis.com:8443/x",
		"https://fcm.googleapis.com.evil.example/x",
		"https://evilpush.apple.com/x",
		"https://127.0.0.1/x",
	] {
		assert!(matches!(Subscription::try_new(endpoint, UA_PUBLIC, AUTH), Err(Failure::Refused(_))), "{endpoint}");
	}
	for endpoint in [
		"https://web.push.apple.com/x",
		"https://wns2-par02p.notify.windows.com/w/",
		"https://updates.push.services.mozilla.com/wpush/v2/x",
	] {
		Subscription::try_new(endpoint, UA_PUBLIC, AUTH).unwrap();
	}
	assert!(Subscription::try_new(FCM, AUTH, AUTH).is_err());
	assert!(Subscription::try_new(FCM, UA_PUBLIC, UA_PUBLIC).is_err());
}
