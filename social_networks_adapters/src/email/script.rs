//! Conversations the daemon holds on its own: a thread opened by a message a script's key matches is
//! answered toward that script's goal, and handed to a human once the goal is reached.

use std::{collections::BTreeMap, str::FromStr, sync::LazyLock};

use color_eyre::eyre::{Report, Result, WrapErr, bail, eyre};
use miette::{Diagnostic, GraphicalReportHandler, GraphicalTheme};
use regex::Regex;
use serde::Deserialize;

use super::EmailMessage;

/// `scripts` of an account, keyed by a regex over the From, Subject and body of a thread's first message.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(try_from = "BTreeMap<String, RawScript>")]
pub struct Scripts(Vec<Script>);
impl Scripts {
	pub(super) fn find(&self, thread: &[EmailMessage]) -> Result<Option<&Script>> {
		let root = thread.first().expect("a thread holds at least the message it was fetched for");
		let forward = is_forward(root);
		let mut hits = self
			.0
			.iter()
			.filter(|s| s.match_forwards || !forward)
			.filter(|s| s.key.is_match(&root.from) || s.key.is_match(&root.subject) || s.key.is_match(&root.body));
		let hit = hits.next();
		if let (Some(a), Some(b)) = (hit, hits.next()) {
			bail!("thread `{}` is opened by a message both script `{}` and `{}` claim", root.subject, a.name(), b.name());
		}
		Ok(hit)
	}
}
impl TryFrom<BTreeMap<String, RawScript>> for Scripts {
	/// Rendered, since serde keeps only the `Display` of what it is handed
	type Error = String;

	fn try_from(raw: BTreeMap<String, RawScript>) -> std::result::Result<Self, String> {
		raw.into_iter()
			.map(|(name, raw)| Script::try_new(name, raw))
			.collect::<std::result::Result<_, _>>()
			.map(Self)
			.map_err(|e| {
				let mut out = String::new();
				GraphicalReportHandler::new_themed(GraphicalTheme::unicode_nocolor())
					.render_report(&mut out, &e)
					.expect("fmt::Write into a String");
				out
			})
	}
}

// ponytail: subject prefix and the Gmail/Apple Mail body markers; other clients' forwards pass as originals
fn is_forward(message: &EmailMessage) -> bool {
	static SUBJECT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^\s*(fwd?|tr)\s*:").expect("literal"));
	SUBJECT.is_match(&message.subject) || message.body.contains("---------- Forwarded message ---------") || message.body.contains("Begin forwarded message:")
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScript {
	goal: Option<String>,
	methods: Option<String>,
	/// A forwarded message quotes someone else's From/Subject/body, which the key would otherwise match
	#[serde(default)]
	match_forwards: bool,
}

#[derive(Debug, Diagnostic, thiserror::Error)]
enum ScriptError {
	#[error("script `{name}` has no {}", missing.join(" and no "))]
	#[diagnostic(
		code(email::script::incomplete),
		help("a script is `\"<regex>\" = {{ goal = \"<what counts as done>\"; methods = \"<how to get there>\"; }};`")
	)]
	Incomplete { name: String, missing: Vec<&'static str> },
	#[error("script `{name}` is keyed by an invalid regex")]
	#[diagnostic(code(email::script::key), help("the key is a regex, matched against the From, Subject and body of the message that opened a thread"))]
	Key {
		name: String,
		#[source]
		source: regex::Error,
	},
}

#[derive(Clone, Debug)]
pub(super) struct Script {
	key: Regex,
	goal: String,
	methods: String,
	match_forwards: bool,
}
impl Script {
	fn try_new(name: String, raw: RawScript) -> std::result::Result<Self, ScriptError> {
		let given = |v: Option<String>| v.filter(|v| !v.trim().is_empty());
		let (goal, methods) = match (given(raw.goal), given(raw.methods)) {
			(Some(goal), Some(methods)) => (goal, methods),
			(goal, methods) => {
				let missing = [goal.is_none().then_some("goal"), methods.is_none().then_some("methods")].into_iter().flatten().collect();
				return Err(ScriptError::Incomplete { name, missing });
			}
		};
		let key = Regex::new(&name).map_err(|source| ScriptError::Key { name, source })?;
		Ok(Self {
			key,
			goal,
			methods,
			match_forwards: raw.match_forwards,
		})
	}

	pub(super) fn name(&self) -> &str {
		self.key.as_str()
	}

	pub(super) fn prompt(&self, account: &str, thread: &[EmailMessage]) -> String {
		let thread = thread.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n\n---\n\n");
		format!(
			r#"You write email as {account}, carrying on the thread below toward a goal.

Always:
- Keep it as concise as the task allows. Shorter is better: cut every sentence that doesn't move toward the goal.

Goal:
{goal}

Methods:
{methods}

Thread, oldest first:

{thread}

If the goal has been reached anywhere in the thread, answer with exactly `ACHIEVED` and nothing else.
Otherwise answer `REPLY` on the first line, and from the second line on the body of the next message {account} sends: plain text, no subject line, no quoted history."#,
			goal = self.goal,
			methods = self.methods,
		)
	}
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum Step {
	Achieved,
	Reply(String),
}
impl FromStr for Step {
	type Err = Report;

	fn from_str(answer: &str) -> Result<Self> {
		let answer = answer.trim();
		if answer == "ACHIEVED" {
			return Ok(Self::Achieved);
		}
		match answer.split_once('\n') {
			Some(("REPLY", body)) if !body.trim().is_empty() => Ok(Self::Reply(body.trim().to_owned())),
			_ => bail!("script answer is neither `ACHIEVED` nor `REPLY\\n<body>`:\n{answer}"),
		}
	}
}

/// The next message of a thread, as `account` sends it.
#[derive(Debug)]
pub(super) struct Reply {
	pub(super) from: String,
	pub(super) to: String,
	pub(super) subject: String,
	pub(super) in_reply_to: Option<String>,
	pub(super) references: Vec<String>,
	pub(super) body: String,
}
impl Reply {
	pub(super) fn try_new(account: &str, thread: &[EmailMessage], body: String) -> Result<Self> {
		let root = thread.first().expect("a thread holds at least the message it was fetched for");
		let latest = thread.last().expect("a thread holds at least the message it was fetched for");
		let counterpart = thread
			.iter()
			.rev()
			.find(|m| !m.from_address.eq_ignore_ascii_case(account))
			.ok_or_else(|| eyre!("thread `{}` holds only messages {account} sent", root.subject))?;
		let subject = root.subject.trim();
		let subject = match subject.get(..3) {
			Some(re) if re.eq_ignore_ascii_case("re:") => subject[3..].trim_start(),
			_ => subject,
		};
		Ok(Self {
			from: account.to_owned(),
			to: counterpart.reply_to.clone().unwrap_or_else(|| counterpart.from_address.clone()),
			subject: format!("Re: {subject}"),
			in_reply_to: latest.message_id.clone(),
			references: latest.references.iter().chain(&latest.message_id).cloned().collect(),
			body,
		})
	}
}
impl TryFrom<&Reply> for lettre::Message {
	type Error = Report;

	fn try_from(reply: &Reply) -> Result<Self> {
		let mut builder = lettre::Message::builder()
			.from(reply.from.parse().wrap_err_with(|| format!("account `{}` is not an address", reply.from))?)
			.to(reply.to.parse().wrap_err_with(|| format!("`{}` is not an address", reply.to))?)
			.subject(&reply.subject);
		if let Some(id) = &reply.in_reply_to {
			builder = builder.in_reply_to(format!("<{id}>"));
		}
		if !reply.references.is_empty() {
			builder = builder.references(reply.references.iter().map(|id| format!("<{id}>")).collect::<Vec<_>>().join(" "));
		}
		builder.body(reply.body.clone()).wrap_err("building the reply")
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn incomplete_or_malformed_scripts_are_refused_by_name() {
		let refused = [
			r#"{ "Google Business": { "goal": "a call" } }"#,
			r#"{ "Google Business": { "goal": "", "methods": null } }"#,
			r#"{ "Google (Business": { "goal": "a call", "methods": "ask" } }"#,
		]
		.map(|config| serde_json::from_str::<Scripts>(config).expect_err("config was accepted").to_string());
		insta::assert_snapshot!(refused.join("\n"), @r#"
		email::script::incomplete

		  × script `Google Business` has no methods
		  help: a script is `"<regex>" = { goal = "<what counts as done>"; methods = "<how to get there>"; };`

		email::script::incomplete

		  × script `Google Business` has no goal and no methods
		  help: a script is `"<regex>" = { goal = "<what counts as done>"; methods = "<how to get there>"; };`

		email::script::key

		  × script `Google (Business` is keyed by an invalid regex
		  ╰─▶ regex parse error:
		          Google (Business
		                 ^
		      error: unclosed group
		  help: the key is a regex, matched against the From, Subject and body of the message that opened a thread
		"#);
	}

	const THREAD: [&str; 3] = [
		"From: Me <me@gmail.com>\r\nTo: Google <support@google.com>\r\nSubject: Google Business Profile verification\r\nDate: Mon, 1 Sep 2026 10:00:00 +0000\r\nMessage-ID: <a@me>\r\n\r\nMy profile is stuck in verification.\r\n",
		"From: Google Support <support@google.com>\r\nReply-To: case-42@google.com\r\nTo: me@gmail.com\r\nSubject: Re: Google Business Profile verification\r\nDate: Tue, 2 Sep 2026 10:00:00 +0000\r\nMessage-ID: <b@google>\r\nIn-Reply-To: <a@me>\r\nReferences: <a@me>\r\n\r\nPlease send a video of your storefront.\r\n> My profile is stuck in verification.\r\n",
		"From: Me <me@gmail.com>\r\nTo: case-42@google.com\r\nSubject: Re: Google Business Profile verification\r\nDate: Wed, 3 Sep 2026 10:00:00 +0000\r\nMessage-ID: <c@me>\r\nIn-Reply-To: <b@google>\r\nReferences: <a@me> <b@google>\r\n\r\nSent. Can we do a live call?\r\n",
	];

	fn thread() -> Vec<EmailMessage> {
		THREAD
			.iter()
			.enumerate()
			.map(|(i, raw)| EmailMessage::parse(format!("me@gmail.com/{i}"), raw.as_bytes()).unwrap())
			.collect()
	}

	#[test]
	fn reply_threads_under_the_latest_and_answers_the_counterpart() {
		let reply = Reply::try_new("me@gmail.com", &thread(), "Following up.".into()).unwrap();
		assert_eq!(reply.to, "case-42@google.com");
		assert_eq!(reply.subject, "Re: Google Business Profile verification");
		assert_eq!(reply.in_reply_to.as_deref(), Some("c@me"));
		assert_eq!(reply.references, ["a@me", "b@google", "c@me"]);
		assert!(!thread()[1].body.contains("stuck"), "quoted history is dropped");
		lettre::Message::try_from(&reply).unwrap();
	}

	#[test]
	fn script_is_chosen_by_the_message_that_opened_the_thread() {
		let scripts: Scripts = serde_json::from_str(r#"{ "stuck in verification": { "goal": "a call", "methods": "ask" } }"#).unwrap();
		let thread = thread();
		assert_eq!(scripts.find(&thread).unwrap().map(Script::name), Some("stuck in verification"));
		assert!(scripts.find(&thread[1..]).unwrap().is_none());
	}

	#[test]
	fn forwards_are_matched_only_by_scripts_that_opt_in() {
		let forward = EmailMessage::parse(
			"me@gmail.com/0".into(),
			"From: Client <client@gmail.com>\r\nTo: me@gmail.com\r\nSubject: Fwd: Action requise\r\nMessage-ID: <f@client>\r\n\r\n---------- Forwarded message ---------\r\nDe : Google Business Profile <businessprofile-noreply@google.com>\r\n\r\nVotre validation n'a pas été approuvée.\r\n"
				.as_bytes(),
		)
		.unwrap();
		let by_default: Scripts = serde_json::from_str(r#"{ "Google Business Profile": { "goal": "a call", "methods": "ask" } }"#).unwrap();
		assert!(by_default.find(std::slice::from_ref(&forward)).unwrap().is_none());
		let opted_in: Scripts = serde_json::from_str(r#"{ "Google Business Profile": { "goal": "a call", "methods": "ask", "match_forwards": true } }"#).unwrap();
		assert!(opted_in.find(&[forward]).unwrap().is_some());
	}

	#[test]
	fn answer_is_achieved_or_a_reply() {
		assert_eq!("ACHIEVED\n".parse::<Step>().unwrap(), Step::Achieved);
		assert_eq!("REPLY\nHi,\nthanks.".parse::<Step>().unwrap(), Step::Reply("Hi,\nthanks.".into()));
		for bad in ["achieved", "REPLY", "REPLY\n  ", "Sure! REPLY\nhi", "ACHIEVED\nREPLY\nhi"] {
			assert!(bad.parse::<Step>().is_err(), "{bad:?} was accepted");
		}
	}
}
