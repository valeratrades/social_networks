//! Nothing a user wrote becomes a mention, a link or markup, and what an entity marks does.

use serde_json::json;
use social_networks_adapters::notify::{Entity, Rich};

const HOSTILE: &str = "@everyone <@1> <!here> **b** _i_ `c` ~s~ [x](https://evil.example) https://evil.example <script>&\"'\n# h\n> q\n- l\n1. n";

fn entity(kind: &str, offset: usize, length: usize) -> Entity {
	serde_json::from_value(json!({"type": kind, "offset": offset, "length": length})).unwrap()
}

#[test]
fn a_users_text_is_only_text() {
	let rich = Rich::from(HOSTILE);
	assert_eq!(rich.plain(), HOSTILE);
	let html = rich.html().replace("<br>", "");
	assert!(!html.contains(['<', '>', '"', '\'']), "{html}");
	let discord = rich.discord();
	let bare = discord.char_indices().find(|(at, c)| c.is_ascii_punctuation() && *c != '\\' && !discord[..*at].ends_with('\\'));
	assert_eq!(bare, None, "{discord}");
	assert_eq!(rich.slack(), [json!({"type": "rich_text_section", "elements": [{"type": "text", "text": HOSTILE}]})]);
}

#[test]
fn offsets_count_utf16_and_nest() {
	let mut rich = Rich::from("😀 bold code");
	rich.entities = vec![entity("bold", 3, 9), entity("code", 8, 4)];
	assert_eq!(rich.html(), "😀 <b>bold </b><code>code</code>", "nothing formats what is verbatim");
	assert_eq!(rich.discord(), "😀 **bold **`code`");
	rich.entities[1].kind = "italic".into();
	assert_eq!(rich.html(), "😀 <b>bold <i>code</i></b>");
}

#[test]
fn a_code_span_cannot_be_left() {
	let mut rich = Rich::default();
	rich.code("a`@everyone```b");
	assert_eq!(rich.discord(), "`aˋ@everyoneˋˋˋb`");
	assert_eq!(rich.html(), "<code>a`@everyone```b</code>");
}

#[test]
fn links_are_the_web_ones_a_caller_marked() {
	let mut rich = Rich::from("see ");
	rich.url("https://a.example/(x)");
	assert_eq!(rich.html(), "see <a href=\"https://a.example/(x)\">https://a.example/(x)</a>");
	assert_eq!(rich.discord(), "see <https://a.example/(x)>");

	let linked = |url: &str| Rich {
		text: "here".into(),
		entities: vec![serde_json::from_value(json!({"type": "text_link", "offset": 0, "length": 4, "url": url})).unwrap()],
	};
	assert_eq!(linked("https://a.example/(x)").discord(), "[here](https://a.example/%28x%29)");
	assert_eq!(linked("https://a.example").slack()[0]["elements"][0]["type"], "link");
	for script in ["javascript:alert(1)", "tg://user?id=1", "\"><script>"] {
		assert_eq!((linked(script).html(), linked(script).discord()), ("here".into(), "here".into()), "{script}");
	}
}

#[test]
fn a_quote_and_a_line_break_hold_over_lines() {
	let mut rich = Rich::from("one\ntwo");
	rich.entities = vec![entity("bold", 0, 7), entity("blockquote", 0, 7)];
	assert_eq!(rich.discord(), "> **one**\n> **two**");
	assert_eq!(rich.slack()[0]["type"], "rich_text_quote");
}

#[test]
fn an_unknown_entity_is_kept_and_one_out_of_range_is_harmless() {
	let wire = json!({"text": "😀x", "entities": [{"type": "custom_emoji", "offset": 0, "length": 2, "custom_emoji_id": "5"}, {"type": "bold", "offset": 1, "length": usize::MAX}]});
	let rich: Rich = serde_json::from_value(wire.clone()).unwrap();
	assert_eq!(serde_json::to_value(&rich).unwrap(), wire);
	assert_eq!(rich.html(), "😀<b>x</b>");
}
