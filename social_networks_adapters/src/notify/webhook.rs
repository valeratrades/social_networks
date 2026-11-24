//! A signed JSON POST to an address a user typed.

use std::{
	net::{IpAddr, SocketAddr},
	sync::Arc,
};

use jiff::Timestamp;
use reqwest::{
	Client, Url,
	dns::{Addrs, Name, Resolve, Resolving},
};
use ring::hmac;

use super::{
	Failure,
	http::{Answer, client},
};

const ANSWER: usize = 8 << 10;

#[derive(Clone, Debug)]
pub struct Webhooks {
	http: Client,
	admits: fn(IpAddr) -> bool,
}
impl Webhooks {
	/// `admits`: [`public`], or a test's. An address it does not admit is never connected to, whatever name
	/// resolved to it.
	pub fn try_new(admits: fn(IpAddr) -> bool) -> Result<Self, Failure> {
		Ok(Self {
			http: client().no_proxy().dns_resolver(Arc::new(Guard { admits })).build()?,
			admits,
		})
	}

	/// With `x-signature: t=<unix seconds>,v1=<hex of HMAC-SHA256 over "<t>.<body>">`.
	pub async fn send(&self, to: &Url, secret: &[u8], body: &serde_json::Value) -> Result<(), Failure> {
		let refuse = |why: &str| Failure::Refused(format!("the webhook: {why}"));
		// the connector takes an address written as one without asking the resolver
		let literal = match to.host().ok_or_else(|| refuse("no host"))? {
			url::Host::Domain(_) => None,
			url::Host::Ipv4(ip) => Some(IpAddr::V4(ip)),
			url::Host::Ipv6(ip) => Some(IpAddr::V6(ip)),
		};
		match to.scheme() {
			"https" => {}
			"http" if literal.is_some_and(|ip| ip.is_loopback()) => {}
			_ => return Err(refuse("not https")),
		}
		if let Some(ip) = literal
			&& !(self.admits)(ip)
		{
			return Err(refuse(&NotAdmitted(ip).to_string()));
		}
		let body = body.to_string();
		let at = Timestamp::now().as_second();
		let tag = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, secret), format!("{at}.{body}").as_bytes());
		let v1: String = tag.as_ref().iter().map(|b| format!("{b:02x}")).collect();
		let request = self
			.http
			.post(to.clone())
			.header("content-type", "application/json")
			.header("x-signature", format!("t={at},v1={v1}"))
			.body(body);
		let answer = Answer::of(request, ANSWER).await?;
		match answer.status {
			_ if answer.ok() => Ok(()),
			410 => Err(Failure::Gone(answer.why("the webhook"))),
			300..400 => Err(refuse("redirects, which is not followed")),
			_ => Err(answer.failure("the webhook")),
		}
	}
}

/// Whether `ip` is an address of the internet at large.
pub fn public(ip: IpAddr) -> bool {
	match ip {
		IpAddr::V4(v4) => {
			let [a, b, c, _] = v4.octets();
			!(v4.is_private()
				|| v4.is_loopback()
				|| v4.is_link_local()
				|| v4.is_documentation()
				|| v4.is_multicast()
				|| a == 0 || a >= 240
				|| (a == 100 && (64..128).contains(&b))
				|| (a == 198 && (18..20).contains(&b))
				|| (a, b, c) == (192, 0, 0))
		}
		IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
			Some(v4) => public(IpAddr::V4(v4)),
			None => {
				let [a, b, ..] = v6.segments();
				// global unicast, less what carries a v4 address inside it and what is reserved
				(0x2000..0x4000).contains(&a) && a != 0x2002 && !(a == 0x2001 && (b < 0x200 || b == 0xdb8))
			}
		},
	}
}

#[derive(Debug, thiserror::Error)]
#[error("{0} is not a public address")]
pub(super) struct NotAdmitted(IpAddr);

struct Guard {
	admits: fn(IpAddr) -> bool,
}
impl Resolve for Guard {
	fn resolve(&self, name: Name) -> Resolving {
		let admits = self.admits;
		Box::pin(async move {
			let found: Vec<SocketAddr> = tokio::net::lookup_host((name.as_str(), 0)).await?.collect();
			if let Some(refused) = found.iter().find(|a| !admits(a.ip())) {
				return Err(NotAdmitted(refused.ip()).into());
			}
			Ok(Box::new(found.into_iter()) as Addrs)
		})
	}
}
