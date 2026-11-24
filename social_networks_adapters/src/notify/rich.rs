use std::{cmp::Reverse, collections::BTreeSet, ops::Range};

use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

const FORMATS: [&str; 10] = [
	"bold",
	"italic",
	"underline",
	"strikethrough",
	"code",
	"pre",
	"blockquote",
	"expandable_blockquote",
	"text_link",
	"url",
];

/// Text in Telegram's `MessageEntity` shape. What an entity does not mark renders as itself everywhere: no
/// character of `text` is markup.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Rich {
	pub text: String,
	pub entities: Vec<Entity>,
}
impl Rich {
	pub fn push(&mut self, s: &str) {
		self.text.push_str(s);
	}

	/// `s` as `code`; nothing when it is empty.
	pub fn code(&mut self, s: &str) {
		self.mark("code", s);
	}

	/// `url` as a link, which Telegram also previews.
	pub fn url(&mut self, url: &str) {
		self.mark("url", url);
	}

	//@codestyle::skip(swappable-args)
	fn mark(&mut self, kind: &str, s: &str) {
		if !s.is_empty() {
			self.entities.push(Entity {
				kind: kind.to_owned(),
				offset: self.units(),
				length: s.encode_utf16().count(),
				rest: Map::new(),
			});
		}
		self.text.push_str(s);
	}

	pub fn plain(&self) -> &str {
		&self.text
	}

	pub fn html(&self) -> String {
		self.marked(
			false,
			|e| {
				let tag = |t: &str| Some((format!("<{t}>"), format!("</{t}>")));
				match e.kind.as_str() {
					"bold" => tag("b"),
					"italic" => tag("i"),
					"underline" => tag("u"),
					"strikethrough" => tag("s"),
					"code" => tag("code"),
					"pre" => tag("pre"),
					"blockquote" | "expandable_blockquote" => tag("blockquote"),
					_ => self.target(e).map(|url| (format!("<a href=\"{}\">", escape_html(url.as_str())), "</a>".to_owned())),
				}
			},
			|s, under| match under.iter().any(|e| e.kind == "pre") {
				true => escape_html(s),
				false => escape_html(s).replace('\n', "<br>\n"),
			},
		)
	}

	/// Markdown, as discord reads it.
	pub fn discord(&self) -> String {
		self.marked(
			true,
			|e| {
				let around = |m: &str| Some((m.to_owned(), m.to_owned()));
				match e.kind.as_str() {
					"bold" => around("**"),
					"italic" => around("*"),
					"underline" => around("__"),
					"strikethrough" => around("~~"),
					"code" => around("`"),
					"pre" => Some(("```\n".to_owned(), "\n```".to_owned())),
					"blockquote" | "expandable_blockquote" => Some(("> ".to_owned(), String::new())),
					"url" => self.target(e).map(|_| ("<".to_owned(), ">".to_owned())),
					_ => self.target(e).map(|url| ("[".to_owned(), format!("]({})", discord_url(&url)))),
				}
			},
			|s, under| match () {
				() if under.iter().any(|e| e.verbatim()) => s.replace('`', "\u{2cb}"), // nothing escapes a backtick inside code
				() if under.iter().any(|e| e.kind == "url" && self.target(e).is_some()) && !s.contains(|c: char| c.is_whitespace() || "<>".contains(c)) => s.to_owned(),
				() => escape_discord(s),
			},
		)
	}

	/// The elements of slack's `rich_text` block; mrkdwn has no escape for `*`, `_`, `~` and the backtick.
	pub fn slack(&self) -> Vec<Value> {
		let mut blocks: Vec<(&str, Vec<Value>)> = Vec::new();
		for span in self.spans(false) {
			let on = |kind: &str| span.under.iter().any(|e| e.kind == kind);
			let block = match () {
				() if on("pre") => "rich_text_preformatted",
				() if span.under.iter().any(|e| e.quote()) => "rich_text_quote",
				() => "rich_text_section",
			};
			let mut style = Map::new();
			for (name, kind) in [("bold", "bold"), ("italic", "italic"), ("strike", "strikethrough"), ("code", "code")] {
				if on(kind) {
					style.insert(name.to_owned(), json!(true));
				}
			}
			let mut element = match span.under.iter().find_map(|e| self.target(e)) {
				Some(url) => json!({"type": "link", "url": url.as_str(), "text": span.text}),
				None => json!({"type": "text", "text": span.text}),
			};
			if !style.is_empty() {
				element["style"] = Value::Object(style);
			}
			match blocks.last_mut() {
				Some((last, elements)) if *last == block => elements.push(element),
				_ => blocks.push((block, vec![element])),
			}
		}
		blocks.into_iter().map(|(kind, elements)| json!({"type": kind, "elements": elements})).collect()
	}

	/// At most `max` UTF-16 units of it, the last one "…" when cut.
	pub(super) fn cut(&self, max: usize) -> Self {
		assert!(max > 0);
		if self.units() <= max {
			return self.clone();
		}
		let mut kept = Self::default();
		let mut units = 0;
		for c in self.text.chars() {
			if units + c.len_utf16() > max - 1 {
				break;
			}
			units += c.len_utf16();
			kept.text.push(c);
		}
		kept.text.push('…');
		kept.entities = self
			.entities
			.iter()
			.filter(|e| e.offset < units)
			.map(|e| Entity {
				length: e.length.min(units - e.offset),
				..e.clone()
			})
			.collect();
		kept
	}

	/// The longest rendering found that is at most `max` UTF-16 units.
	pub(super) fn fit(&self, max: usize, render: impl Fn(&Self) -> String) -> String {
		let mut room = max;
		loop {
			let out = render(&self.cut(room));
			match out.encode_utf16().count().checked_sub(max) {
				None | Some(0) => return out,
				Some(over) => room = room.saturating_sub(over).max(room.div_euclid(2)),
			}
		}
	}

	fn units(&self) -> usize {
		self.text.encode_utf16().count()
	}

	/// The byte at an offset in UTF-16 units: the next character's when inside one, the end when past it.
	fn byte(&self, offset: usize) -> usize {
		let mut units = 0;
		for (at, c) in self.text.char_indices() {
			if units >= offset {
				return at;
			}
			units += c.len_utf16();
		}
		self.text.len()
	}

	fn range(&self, e: &Entity) -> Range<usize> {
		self.byte(e.offset)..self.byte(e.offset.saturating_add(e.length))
	}

	fn target(&self, e: &Entity) -> Option<Url> {
		let url = match e.kind.as_str() {
			"text_link" => e.rest.get("url")?.as_str()?,
			"url" => &self.text[self.range(e)],
			_ => return None,
		};
		Url::parse(url).ok().filter(|u| matches!(u.scheme(), "http" | "https")) // what is no web address is only text
	}

	/// `lines`: a line break is a span of its own, under nothing but `pre`.
	fn spans(&self, lines: bool) -> Vec<Span<'_>> {
		let mut marks: Vec<(Range<usize>, &Entity)> = self
			.entities
			.iter()
			.filter(|e| FORMATS.contains(&e.kind.as_str()))
			.map(|e| (self.range(e), e))
			.filter(|(r, _)| !r.is_empty())
			.collect();
		marks.sort_by_key(|(r, e)| (!e.quote(), r.start, Reverse(r.end)));
		let mut cuts: BTreeSet<usize> = marks.iter().flat_map(|(r, _)| [r.start, r.end]).chain([0, self.text.len()]).collect();
		if lines {
			cuts.extend(self.text.match_indices('\n').flat_map(|(at, _)| [at, at + 1]));
		}
		let cuts: Vec<usize> = cuts.into_iter().collect();
		cuts.array_windows()
			.map(|&[from, to]| {
				let text = &self.text[from..to];
				let mut under: Vec<&Entity> = marks.iter().filter(|(r, _)| r.contains(&from)).map(|(_, e)| *e).collect();
				if lines && text == "\n" {
					under.retain(|e| e.kind == "pre");
				}
				if let Some(verbatim) = under.iter().copied().find(|e| e.verbatim()) {
					under.retain(|e| std::ptr::eq(*e, verbatim) || e.quote());
				}
				Span { text, under }
			})
			.collect()
	}

	fn marked(&self, lines: bool, mark: impl Fn(&Entity) -> Option<(String, String)>, escape: impl Fn(&str, &[&Entity]) -> String) -> String {
		let mut out = String::new();
		let mut open: Vec<(&Entity, String)> = Vec::new();
		let close = |out: &mut String, open: &mut Vec<(&Entity, String)>, keep: usize| {
			for (_, end) in open.drain(keep..).rev() {
				out.push_str(&end);
			}
		};
		for span in self.spans(lines) {
			let under: Vec<_> = span.under.iter().filter_map(|e| Some((*e, mark(e)?))).collect();
			let keep = open.iter().zip(&under).take_while(|((a, _), (b, _))| std::ptr::eq(*a, *b)).count();
			close(&mut out, &mut open, keep);
			for (e, (start, end)) in under.into_iter().skip(keep) {
				out.push_str(&start);
				open.push((e, end));
			}
			out.push_str(&escape(span.text, &span.under));
		}
		close(&mut out, &mut open, 0);
		out
	}
}

impl From<String> for Rich {
	fn from(text: String) -> Self {
		Self { text, entities: Vec::new() }
	}
}
impl From<&str> for Rich {
	fn from(text: &str) -> Self {
		text.to_owned().into()
	}
}

/// Offsets in UTF-16 code units. A kind nothing here renders is kept as it came.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Entity {
	#[serde(rename = "type")]
	pub kind: String,
	pub offset: usize,
	pub length: usize,
	#[serde(flatten)]
	pub rest: Map<String, Value>,
}
impl Entity {
	fn quote(&self) -> bool {
		self.kind.ends_with("blockquote")
	}

	fn verbatim(&self) -> bool {
		matches!(self.kind.as_str(), "code" | "pre")
	}
}

/// A run of text under one set of entities, outermost first.
struct Span<'a> {
	text: &'a str,
	under: Vec<&'a Entity>,
}

pub(super) fn escape_html(s: &str) -> String {
	s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// All punctuation, the `:` of a URL included: discord links one it sees.
pub(super) fn escape_discord(s: &str) -> String {
	s.chars().fold(String::with_capacity(s.len()), |mut out, c| {
		if c.is_ascii_punctuation() {
			out.push('\\');
		}
		out.push(c);
		out
	})
}

pub(super) fn discord_url(url: &Url) -> String {
	url.as_str().replace('(', "%28").replace(')', "%29")
}
