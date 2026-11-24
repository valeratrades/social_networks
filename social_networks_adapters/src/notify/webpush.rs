//! Web Push: RFC 8291's encryption under RFC 8292's VAPID.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jiff::{SignedDuration, Timestamp};
use p256::{
	PublicKey, SecretKey,
	ecdsa::{Signature, SigningKey, signature::Signer},
};
use reqwest::{Client, Url};
use ring::{
	aead, digest, hmac,
	rand::{SecureRandom, SystemRandom},
};
use serde::Serialize;
use serde_json::json;

use super::{
	Failure, Level, Rich,
	http::{ANSWER, Answer, client},
};

const RECORD: usize = 4096;
const HEADER: usize = 16 + 4 + 1 + 65;
/// What a record holds besides the header, the padding's delimiter and the tag.
const PAYLOAD: usize = RECORD - HEADER - 1 - 16;
const EXPIRES: SignedDuration = SignedDuration::from_hours(12);

/// The key push services know us by: a browser's subscription is to it and no other.
#[derive(Clone)]
pub struct Vapid {
	key: SigningKey,
	public: String,
}
impl Vapid {
	pub fn from_seed(seed: [u8; 32]) -> Self {
		let secret = scalar(seed);
		Self {
			public: URL_SAFE_NO_PAD.encode(secret.public_key().to_sec1_bytes()),
			key: SigningKey::from(secret),
		}
	}

	/// The `applicationServerKey`, base64url.
	pub fn public(&self) -> &str {
		&self.public
	}

	//@codestyle::skip(swappable-args)
	fn authorization(&self, audience: &str, subject: &str, now: Timestamp) -> String {
		let part = |v: serde_json::Value| URL_SAFE_NO_PAD.encode(v.to_string());
		let claims = json!({"aud": audience, "exp": (now + EXPIRES).as_second(), "sub": subject});
		let signed = format!("{}.{}", part(json!({"typ": "JWT", "alg": "ES256"})), part(claims));
		let signature: Signature = self.key.sign(signed.as_bytes());
		format!("vapid t={signed}.{}, k={}", URL_SAFE_NO_PAD.encode(signature.to_bytes()), self.public)
	}
}
impl std::fmt::Debug for Vapid {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Vapid").field("public", &self.public).finish_non_exhaustive()
	}
}

#[derive(Clone, Debug)]
pub struct Subscription {
	endpoint: Url,
	key: PublicKey,
	auth: [u8; 16],
}
impl Subscription {
	/// As the browser's `PushSubscription.toJSON()` gives them; the endpoint only of a push service we know.
	//@codestyle::skip(swappable-args)
	pub fn try_new(endpoint: &str, p256dh: &str, auth: &str) -> Result<Self, Failure> {
		let refuse = |why: &str| Failure::Refused(format!("the subscription: {why}"));
		let bytes = |s: &str| URL_SAFE_NO_PAD.decode(s.trim_end_matches('='));
		let endpoint = Url::parse(endpoint).map_err(|_| refuse("the endpoint is no URL"))?;
		let known = endpoint
			.host_str()
			.is_some_and(|h| matches!(h, "fcm.googleapis.com" | "updates.push.services.mozilla.com") || h.ends_with(".push.apple.com") || h.ends_with(".notify.windows.com"));
		if !known || endpoint.scheme() != "https" || endpoint.port().is_some() || !endpoint.username().is_empty() || endpoint.password().is_some() {
			return Err(refuse("not a push service's endpoint"));
		}
		Ok(Self {
			endpoint,
			key: bytes(p256dh)
				.ok()
				.and_then(|k| PublicKey::from_sec1_bytes(&k).ok())
				.ok_or_else(|| refuse("p256dh is no P-256 key"))?,
			auth: bytes(auth).ok().and_then(|a| a.try_into().ok()).ok_or_else(|| refuse("auth is not 16 bytes"))?,
		})
	}
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Payload {
	pub title: String,
	pub body: String,
	pub url: String,
	pub tag: String,
	pub level: Level,
}
impl Payload {
	/// The body cut to what one record holds.
	fn bytes(&self) -> Result<Vec<u8>, Failure> {
		let mut room = self.body.encode_utf16().count().max(1);
		loop {
			let cut = Self {
				body: Rich::from(self.body.as_str()).cut(room).text,
				..self.clone()
			};
			let out = serde_json::to_vec(&cut).expect("strings serialize");
			if out.len() <= PAYLOAD {
				return Ok(out);
			}
			if room == 1 {
				return Err(Failure::Refused("the push is over a record without its body".into()));
			}
			room = room.div_euclid(2);
		}
	}
}

#[derive(Clone, Debug)]
pub struct Pusher {
	http: Client,
	vapid: Vapid,
	subject: String,
	base: Option<Url>,
}
impl Pusher {
	/// `subject`: `mailto:` or `https:`, whom a push service writes to about us. `base`: a test's mock, asked
	/// in every endpoint's place.
	pub fn try_new(vapid: Vapid, subject: &str, base: Option<Url>) -> Result<Self, Failure> {
		if !(subject.starts_with("mailto:") || subject.starts_with("https://")) {
			return Err(Failure::Refused(format!("`{subject}` is no VAPID subject")));
		}
		Ok(Self {
			http: client().build()?,
			vapid,
			subject: subject.to_owned(),
			base,
		})
	}

	/// `topic`: at most 32 base64url characters; an undelivered push under the same one is replaced.
	pub async fn send(&self, to: &Subscription, payload: &Payload, ttl: SignedDuration, urgency: Level, topic: Option<&str>) -> Result<(), Failure> {
		let mut at = to.endpoint.clone();
		if let Some(base) = &self.base {
			at = base.clone();
			at.set_path(to.endpoint.path());
			at.set_query(to.endpoint.query());
		}
		let mut request = self
			.http
			.post(at)
			.header(
				"authorization",
				self.vapid.authorization(&to.endpoint.origin().ascii_serialization(), &self.subject, Timestamp::now()),
			)
			.header("content-encoding", "aes128gcm")
			.header("content-type", "application/octet-stream")
			.header("ttl", ttl.as_secs())
			.header(
				"urgency",
				match urgency {
					Level::Low => "low",
					Level::Normal => "normal",
					Level::High => "high",
				},
			);
		if let Some(topic) = topic {
			request = request.header("topic", topic);
		}
		let answer = Answer::of(request.body(sealed(to, payload.bytes()?)), ANSWER).await?;
		match answer.status {
			_ if answer.ok() => Ok(()),
			404 | 410 => Err(Failure::Gone(answer.why("the push service"))),
			401 | 403 => Err(Failure::Transient(answer.why("the push service refused our signature"))),
			_ => Err(answer.failure("the push service")),
		}
	}
}

fn sealed(to: &Subscription, plain: Vec<u8>) -> Vec<u8> {
	let (mut seed, mut salt) = ([0; 32], [0; 16]);
	SystemRandom::new()
		.fill(&mut seed)
		.and_then(|()| SystemRandom::new().fill(&mut salt))
		.expect("the OS random source");
	let ephemeral = scalar(seed);
	let ours = ephemeral.public_key().to_sec1_bytes();
	let shared = ephemeral.diffie_hellman(&to.key);

	let info = [b"WebPush: info\0".as_slice(), &to.key.to_sec1_bytes(), &ours].concat();
	let ikm = hkdf(&to.auth, shared.raw_secret_bytes(), &info);
	let cek = hkdf(&salt, &ikm, b"Content-Encoding: aes128gcm\0");
	let nonce = hkdf(&salt, &ikm, b"Content-Encoding: nonce\0");

	let mut record = plain;
	record.push(2); // the delimiter of the last record's padding
	let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek[..16]).expect("16 bytes is AES-128's key"));
	let nonce = aead::Nonce::try_assume_unique_for_key(&nonce[..12]).expect("12 bytes is GCM's nonce");
	key.seal_in_place_append_tag(nonce, aead::Aad::empty(), &mut record).expect("a record is under GCM's length");

	let size = u32::try_from(RECORD).expect("a constant under it");
	let id = u8::try_from(ours.len()).expect("an uncompressed point is 65 bytes");
	[salt.as_slice(), &size.to_be_bytes(), &[id], &ours, &record].concat()
}

/// HKDF-SHA256, to the one block every key here is cut from.
fn hkdf(salt: &[u8], ikm: &[u8], info: &[u8]) -> [u8; 32] {
	let mac = |key: &[u8], data: &[u8]| hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data);
	let prk = mac(salt, ikm);
	mac(prk.as_ref(), &[info, &[1]].concat()).as_ref().try_into().expect("SHA-256's length")
}

/// The bytes as a secret key, hashed on while they are none: 1 in 2^32 is past the curve's order.
fn scalar(mut bytes: [u8; 32]) -> SecretKey {
	loop {
		//LOOP: each round fails with probability 2^-32
		match SecretKey::from_slice(&bytes) {
			Ok(key) => return key,
			Err(_) => bytes = digest::digest(&digest::SHA256, &bytes).as_ref().try_into().expect("SHA-256's length"),
		}
	}
}
