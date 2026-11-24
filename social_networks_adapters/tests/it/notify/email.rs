//! A relay's answers, and the mail it is handed.

use jiff::SignedDuration;
use social_networks_adapters::notify::{
	Failure, Level,
	email::{Mailer, Smtp},
};
use tokio::{
	io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
	net::TcpListener,
	task::JoinHandle,
};

use super::{message, photo};

fn smtp(host: &str, port: u16) -> Smtp {
	Smtp {
		host: host.into(),
		port,
		username: "u".into(),
		password: "p".to_owned().into(),
		from: "Panel <alerts@panel.example>".into(),
	}
}

/// One session's mail, `auth` and `rcpt` being the answers to those commands.
async fn relay(auth: &'static str, rcpt: &'static str) -> (Mailer, JoinHandle<String>) {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let mailer = Mailer::try_new(smtp("127.0.0.1", listener.local_addr().unwrap().port())).unwrap();
	let session = tokio::spawn(async move {
		let (stream, _) = listener.accept().await.unwrap();
		let mut stream = BufReader::new(stream);
		stream.write_all(b"220 hi\r\n").await.unwrap();
		let mut mail = String::new();
		let mut line = String::new();
		let mut data = false;
		while stream.read_line(&mut line).await.unwrap() > 0 {
			let answer = match line.split_whitespace().next() {
				Some(".") if data => {
					data = false;
					"250 ok"
				}
				_ if data => {
					mail.push_str(&line);
					line.clear();
					continue;
				}
				Some("EHLO") => "250-mock\r\n250 AUTH PLAIN",
				Some("AUTH") => auth,
				Some("RCPT") => rcpt,
				Some("DATA") => {
					data = true;
					"354 go"
				}
				_ => "250 ok",
			};
			stream.write_all(format!("{answer}\r\n").as_bytes()).await.unwrap();
			let done = answer == "250 ok" && !mail.is_empty();
			line.clear();
			if done {
				break;
			}
		}
		mail
	});
	(mailer, session)
}

async fn refused(auth: &'static str, rcpt: &'static str) -> Failure {
	let (mailer, _session) = relay(auth, rcpt).await;
	mailer.send("a@b.example", &message("hi", Level::Normal)).await.unwrap_err()
}

#[tokio::test]
async fn answers_are_classified() {
	let signed_in = "235 ok";
	assert!(matches!(refused("535 5.7.8 bad credentials", "").await, Failure::Transient(why) if why.contains("sign-in")));
	assert!(matches!(refused(signed_in, "550 5.1.1 no such user").await, Failure::Gone(_)));
	assert!(matches!(refused(signed_in, "550 5.7.1 rejected by policy, see 15.1.10").await, Failure::Refused(_)));
	assert_eq!(refused(signed_in, "450 4.2.1 slow down").await, Failure::RetryAfter(SignedDuration::from_secs(60)));
	assert!(matches!(refused(signed_in, "451 4.3.0 later").await, Failure::Transient(_)));
}

#[test]
fn nothing_goes_in_the_clear_past_loopback() {
	assert!(matches!(Mailer::try_new(smtp("relay.example", 25)), Err(Failure::Refused(_))));
}

#[tokio::test]
async fn the_mail_is_whole() {
	let (mailer, session) = relay("235 ok", "250 ok").await;
	let mut sent = message("<b>hi</b>", Level::Normal);
	sent.title = Some("New lead\r\nBcc: x@y.example".into());
	sent.photo = Some(photo());
	sent.unsubscribe = Some("https://panel.example/u?t=1".parse().unwrap());
	mailer.send("a@b.example", &sent).await.unwrap();
	let mail = session.await.unwrap();
	for part in [
		"Subject: New lead  Bcc: x@y.example\r\n",
		"List-Unsubscribe: <https://panel.example/u?t=1>\r\n",
		"List-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n",
		"multipart/alternative",
		"multipart/related",
		"Content-ID: <photo>",
		"&lt;b&gt;hi&lt;/b&gt;",
		"@panel.example>",
	] {
		assert!(mail.contains(part), "no {part} in\n{mail}");
	}
}
