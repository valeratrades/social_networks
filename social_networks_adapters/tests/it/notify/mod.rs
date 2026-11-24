mod chat;
mod email;
mod rich;
mod webhook;
mod webpush;

use social_networks_adapters::notify::{Level, Link, Message, Photo, Rich};
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	net::TcpListener,
	sync::mpsc::{UnboundedReceiver, unbounded_channel},
	task::JoinSet,
};

struct Request {
	head: String,
	body: Vec<u8>,
}
impl Request {
	fn header(&self, name: &str) -> &str {
		self.head
			.lines()
			.find_map(|l| l.split_once(": ").filter(|(n, _)| n.eq_ignore_ascii_case(name)))
			.unwrap_or_else(|| panic!("no {name} in {}", self.head))
			.1
	}

	fn json(&self) -> serde_json::Value {
		serde_json::from_slice(&self.body).unwrap()
	}
}

/// Answers every request alike, and hands over what each said.
struct Mock {
	base: String,
	got: UnboundedReceiver<Request>,
	serving: JoinSet<()>,
}
impl Mock {
	async fn answering(status: u16, headers: &str, body: &str) -> Self {
		let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
		let base = format!("http://{}", listener.local_addr().unwrap());
		let answer = format!("HTTP/1.1 {status} X\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
		let (tx, got) = unbounded_channel();
		let mut serving = JoinSet::new();
		serving.spawn(async move {
			loop {
				let (mut stream, _) = listener.accept().await.unwrap();
				let mut raw = Vec::new();
				let (head, body) = loop {
					assert_ne!(stream.read_buf(&mut raw).await.unwrap(), 0, "a request cut short");
					let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
					let head = String::from_utf8(raw[..end].to_vec()).unwrap();
					let length: usize = head
						.lines()
						.find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length: ").map(|n| n.parse().unwrap()))
						.unwrap_or(0); // a GET has none
					if raw.len() >= end + 4 + length {
						break (head, raw[end + 4..].to_vec());
					}
				};
				stream.write_all(answer.as_bytes()).await.unwrap();
				if tx.send(Request { head, body }).is_err() {
					return;
				}
			}
		});
		Self { base, got, serving }
	}

	async fn got(&mut self) -> Request {
		assert!(self.serving.try_join_next().is_none(), "the mock panicked");
		self.got.recv().await.unwrap()
	}
}

fn message(text: impl Into<Rich>, level: Level) -> Message {
	Message {
		title: None,
		text: text.into(),
		level,
		links: vec![Link {
			label: "Open".into(),
			url: "https://panel.example/a?b=(c)".parse().unwrap(),
		}],
		buttons: Vec::new(),
		photo: None,
		unsubscribe: None,
	}
}

fn photo() -> Photo {
	Photo {
		name: "a b.png".into(),
		mime: "image/png".into(),
		bytes: b"\x89PNG".to_vec(),
	}
}
