//! Logged out, and reading one thing only: when the newest public post went up, as evidence that the
//! person was active no later than then. A lower bound, and silent where Instagram is: a private,
//! empty or missing account yields no moment at all, which is not the same as an old one.
//!
//! The web profile endpoint answers a browser user agent with `401 require_login`, but the same
//! endpoint on the mobile host answers the Instagram app's user agent with the public profile. Which
//! of the two Instagram serves is its to change; a refusal surfaces as that handle's failure.

use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use jiff::Timestamp;

use crate::reach::{Profile, Profiles, Window};

const UA: &str = "Instagram 275.0.0.27.98 Android (33/13; 420dpi; 1080x2400; samsung; SM-G991B; o1s; exynos2100; en_US; 458229237)";
const APP_ID: &str = "567067343352427";

#[derive(Default)]
pub struct Instagram {
	http: reqwest::Client,
}

impl Profiles for Instagram {
	async fn profile(&mut self, handle: &str, _window: Window) -> Result<Profile> {
		let response = self
			.http
			.get("https://i.instagram.com/api/v1/users/web_profile_info/")
			.query(&[("username", handle)])
			.header("user-agent", UA)
			.header("x-ig-app-id", APP_ID)
			.send()
			.await
			.wrap_err_with(|| format!("instagram `{handle}`"))?;
		let status = response.status().as_u16();
		let body = response.text().await.wrap_err_with(|| format!("instagram `{handle}`"))?;
		let mut profile = Profile::default();
		profile.active = newest_post(status, &body).wrap_err_with(|| format!("instagram `{handle}`"))?;
		Ok(profile)
	}
}

/// `None` is Instagram saying there is nothing to see: no such account, a private one, or no posts.
/// Everything else that is not a profile is an error, so a wall or a reshaped payload cannot pass
/// for an account that went quiet.
///
/// Pinned posts lead the timeline whatever their age, so the newest is the maximum rather than the first.
pub fn newest_post(status: u16, body: &str) -> Result<Option<Timestamp>> {
	match status {
		404 => return Ok(None),
		200 => {}
		_ => bail!("HTTP {status}: {}", body.chars().take(200).collect::<String>()),
	}
	let payload: serde_json::Value = serde_json::from_str(body).wrap_err("not json")?;
	let user = payload.pointer("/data/user").ok_or_else(|| eyre!("no `data.user` in the answer"))?;
	if user.is_null() {
		return Ok(None);
	}
	let private = user.get("is_private").and_then(|v| v.as_bool()).ok_or_else(|| eyre!("no `is_private` on the user"))?;
	let edges = user
		.pointer("/edge_owner_to_timeline_media/edges")
		.and_then(|v| v.as_array())
		.ok_or_else(|| eyre!("no timeline edges on the user"))?;
	if private {
		return Ok(None);
	}
	let mut newest: Option<i64> = None;
	for edge in edges {
		let at = edge
			.pointer("/node/taken_at_timestamp")
			.and_then(|v| v.as_i64())
			.ok_or_else(|| eyre!("a post without `taken_at_timestamp`: {edge}"))?;
		newest = newest.max(Some(at));
	}
	newest.map(|at| Timestamp::from_second(at).wrap_err("a post timestamp out of range")).transpose()
}
