//! Messenger, on the launched session: a thread is `messages/t/<user id>`.

use std::time::Duration;

use color_eyre::eyre::{Result, bail};
use miette::{GraphicalReportHandler, GraphicalTheme};
use serde_json::Value;
use v_utils::macros::wrap_err;

use super::Facebook;

#[wrap_err]
#[derive(Debug, miette::Diagnostic, thiserror::Error)]
enum MessengerError {
	#[leaf]
	#[error("facebook account {account} has no Messenger PIN, and its end-to-end encrypted chats are neither sent to nor read without one")]
	#[diagnostic(
		code(facebook::messenger::no_pin),
		help("log in as {account} in any browser, open https://www.facebook.com/messages/ and pick `Create PIN`; a browser new to the account asks for it after that")
	)]
	NoPin { account: String },
}

impl Facebook<'_, '_> {
	/// Phase-0 spike: loads `url`, lets the app render, evaluates each of `exprs`; with `typed`, focuses the
	/// composer, types it with Enter, and evaluates them again.
	pub async fn spike(&mut self, url: &str, exprs: &[String], typed: Option<&str>) -> Result<Vec<Value>> {
		self.messenger(url).await?;
		let mut out = Vec::new();
		for e in exprs {
			out.push(self.tab.eval::<Value>(e).await.unwrap_or_else(|e| Value::String(format!("{e:#}"))));
		}
		if let Some(text) = typed {
			let gate: bool = self
				.tab
				.eval(r#"(() => { const b = [...document.querySelectorAll('[role="button"], button')].find(b => b.innerText.trim() === 'Continue'); if (!b) return false; b.focus(); return document.activeElement === b; })()"#)
				.await?;
			out.push(Value::Bool(gate));
			if gate {
				self.tab.type_line("").await?;
				tokio::time::sleep(Duration::from_secs(5)).await;
			}
			let focused: bool = self
				.tab
				.eval(r#"(() => { const c = document.querySelector('[role="textbox"][contenteditable="true"]'); if (!c) return false; c.focus(); return document.activeElement === c; })()"#)
				.await?;
			out.push(Value::Bool(focused));
			if focused {
				self.tab.type_line(text).await?;
				tokio::time::sleep(Duration::from_secs(6)).await;
				for e in exprs {
					out.push(self.tab.eval::<Value>(e).await.unwrap_or_else(|e| Value::String(format!("{e:#}"))));
				}
			}
		}
		Ok(out)
	}

	/// A messenger page, rendered. Refuses an account without a PIN: nothing E2EE on it goes through.
	async fn messenger(&mut self, url: &str) -> Result<()> {
		self.load(url, &[super::FEED]).await?;
		tokio::time::sleep(Duration::from_secs(8)).await; // the app renders well after the load event
		let asks_for_pin: bool = self
			.tab
			.eval(r#"[...document.querySelectorAll('[role="dialog"]')].some(d => d.innerText.includes('Create a PIN to access your chats'))"#)
			.await?;
		if asks_for_pin {
			let e = MessengerError::new_no_pin(self.tab.account().await?);
			let mut out = String::new();
			// rendered here: the binaries report through eyre, which keeps only `Display` and would drop the help
			GraphicalReportHandler::new_themed(GraphicalTheme::unicode_nocolor())
				.render_report(&mut out, &e)
				.expect("fmt::Write into a String");
			bail!(out);
		}
		Ok(())
	}
}
