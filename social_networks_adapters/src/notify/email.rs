//! Mail, through a relay we sign in to.

use std::{net::IpAddr, time::Duration};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jiff::SignedDuration;
use lettre::{
	AsyncSmtpTransport, AsyncTransport, Tokio1Executor,
	message::{
		Attachment, Mailbox, MultiPart, SinglePart,
		header::{ContentType, HeaderName, HeaderValue},
	},
	transport::smtp::{
		authentication::Credentials,
		client::{Tls, TlsParameters},
	},
};
use ring::rand::{SecureRandom, SystemRandom};
use zeroize::Zeroizing;

use super::{Failure, Message, Photo, Rich, http, rich::escape_html};

const TOTAL: Duration = Duration::from_secs(15);
const SUBJECT: usize = 200;
/// What RFC 3463 says of the recipient's address itself.
const DEAD: [&str; 6] = ["5.1.1", "5.1.2", "5.1.3", "5.1.6", "5.1.10", "5.2.1"];

const CID: &str = "photo";

pub struct Smtp {
	pub host: String,
	/// 465 is TLS, 587 STARTTLS; any other is plain, and only to loopback.
	pub port: u16,
	pub username: String,
	pub password: Zeroizing<String>,
	/// An address, with or without a name: `Name <a@b.c>`.
	pub from: String,
}

#[derive(Clone)]
pub struct Mailer {
	transport: AsyncSmtpTransport<Tokio1Executor>,
	from: Mailbox,
}
impl Mailer {
	pub fn try_new(smtp: Smtp) -> Result<Self, Failure> {
		http::provider();
		let from = smtp.from.parse().map_err(|e| Failure::Refused(format!("`{}` is no address to send from: {e}", smtp.from)))?;
		let parameters = || TlsParameters::new(smtp.host.clone()).map_err(|e| Failure::Refused(format!("TLS to {}: {e}", smtp.host)));
		let tls = match smtp.port {
			465 => Tls::Wrapper(parameters()?),
			587 => Tls::Required(parameters()?),
			_ if smtp.host == "localhost" || smtp.host.parse().is_ok_and(|ip: IpAddr| ip.is_loopback()) => Tls::None,
			port => return Err(Failure::Refused(format!("{}:{port} would be sent to in the clear", smtp.host))),
		};
		let transport = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&smtp.host)
			.port(smtp.port)
			.tls(tls)
			.timeout(Some(TOTAL))
			.credentials(Credentials::new(smtp.username, smtp.password.to_string()))
			.build();
		Ok(Self { transport, from })
	}

	pub async fn send(&self, to: &str, message: &Message) -> Result<(), Failure> {
		let mail = self.mail(to, message)?;
		match tokio::time::timeout(TOTAL, self.transport.send(mail)).await {
			Ok(sent) => sent.map(drop).map_err(classified),
			Err(_) => Err(Failure::Transient(format!("the relay took over {}s", TOTAL.as_secs()))),
		}
	}

	fn mail(&self, to: &str, message: &Message) -> Result<lettre::Message, Failure> {
		let title = message.title.as_deref().or_else(|| message.text.plain().lines().next()).unwrap_or_default(); // a photo alone has no subject
		let subject: String = title.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
		let subject = Rich::from(subject).cut(SUBJECT).text;

		let mut id = [0; 16];
		SystemRandom::new().fill(&mut id).expect("the OS random source");
		let mut mail = lettre::Message::builder()
			.from(self.from.clone())
			.to(to.parse().map_err(|e| Failure::Refused(format!("`{to}` is not an address: {e}")))?)
			.message_id(Some(format!("<{}@{}>", URL_SAFE_NO_PAD.encode(id), self.from.email.domain())))
			.subject(&subject)
			.raw_header(HeaderValue::new(HeaderName::new_from_ascii_str("Auto-Submitted"), "auto-generated".to_owned()));
		if let Some(url) = &message.unsubscribe {
			mail = mail
				.raw_header(HeaderValue::new(HeaderName::new_from_ascii_str("List-Unsubscribe"), format!("<{url}>")))
				.raw_header(HeaderValue::new(HeaderName::new_from_ascii_str("List-Unsubscribe-Post"), "List-Unsubscribe=One-Click".to_owned()));
		}

		let plain = message.links.iter().fold(message.text.plain().to_owned(), |text, l| format!("{text}\n\n{}: {}", l.label, l.url));
		let alternative = MultiPart::alternative().singlepart(SinglePart::plain(plain));
		let html = SinglePart::html(html(&subject, message));
		mail.multipart(match &message.photo {
			None => alternative.singlepart(html),
			Some(Photo { name, mime, bytes }) => {
				let mime = ContentType::parse(mime).map_err(|e| Failure::Refused(format!("`{mime}`: {e}")))?;
				let photo = Attachment::new_inline_with_name(CID.to_owned(), name.clone()).body(bytes.clone(), mime);
				alternative.multipart(MultiPart::related().singlepart(html).singlepart(photo))
			}
		})
		.map_err(|e| Failure::Refused(e.to_string()))
	}
}
impl std::fmt::Debug for Mailer {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Mailer").field("from", &self.from).finish_non_exhaustive()
	}
}

fn html(subject: &str, message: &Message) -> String {
	let title = message.title.as_deref().map(|t| format!("<h1>{}</h1>", escape_html(t))).unwrap_or_default();
	let photo = message.photo.as_ref().map(|_| format!("<p><img src=\"cid:{CID}\" alt=\"\"></p>")).unwrap_or_default();
	let links: String = message
		.links
		.iter()
		.map(|l| format!("<a class=\"open\" href=\"{}\">{}</a> ", escape_html(l.url.as_str()), escape_html(&l.label)))
		.collect();
	let unsubscribe = message
		.unsubscribe
		.as_ref()
		.map(|u| format!("<p class=\"foot\"><a href=\"{}\">Unsubscribe</a></p>", escape_html(u.as_str())))
		.unwrap_or_default();
	format!(
		r#"<!doctype html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width"><meta name="color-scheme" content="light dark"><meta name="supported-color-schemes" content="light dark">
<title>{subject}</title>
<style>
:root {{ color-scheme: light dark; }}
body {{ margin: 0; padding: 24px; font: 16px/1.5 -apple-system, "Segoe UI", Roboto, Helvetica, Arial, sans-serif; color: #1a1a1a; background: #ffffff; }}
main {{ display: block; max-width: 560px; margin: 0 auto; }}
h1 {{ font-size: 20px; line-height: 1.3; margin: 0 0 16px; }}
img {{ max-width: 100%; height: auto; border-radius: 8px; }}
blockquote {{ margin: 0; padding-left: 12px; border-left: 3px solid #d0d0d0; }}
.open {{ display: inline-block; margin: 8px 8px 0 0; padding: 10px 18px; border-radius: 8px; background: #1f6feb; color: #ffffff; text-decoration: none; font-weight: 600; }}
.foot, .foot a {{ margin-top: 32px; font-size: 13px; color: #777777; }}
@media (prefers-color-scheme: dark) {{ body {{ color: #e8e8e8; background: #161616; }} blockquote {{ border-color: #444444; }} .foot, .foot a {{ color: #999999; }} }}
</style></head>
<body><main>{title}<div>{text}</div>{photo}<p>{links}</p>{unsubscribe}</main></body></html>
"#,
		subject = escape_html(subject),
		text = message.text.html(),
	)
}

fn classified(e: lettre::transport::smtp::Error) -> Failure {
	let why = e.to_string();
	match e.status().map(u16::from) {
		Some(421 | 450 | 452 | 454) => Failure::RetryAfter(SignedDuration::from_secs(60)),
		// the relay's say about us, not about the address
		Some(530 | 534 | 535 | 538) => Failure::Transient(format!("the relay refused our sign-in: {why}")),
		Some(400..500) | None => Failure::Transient(why),
		Some(_) if why.split(|c: char| !(c.is_ascii_digit() || c == '.')).any(|t| DEAD.contains(&t)) => Failure::Gone(why),
		Some(_) => Failure::Refused(why),
	}
}
