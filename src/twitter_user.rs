use std::str::FromStr;

use color_eyre::eyre::{Result, eyre};
use serde::{Deserialize, Deserializer, Serialize, de};

#[deprecated(note = "Not currently used, but kept for potential future use")]
#[allow(dead_code)]
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(untagged)]
pub enum TwitterUser {
	UserId(u64),
	Username(String),
}

#[allow(deprecated)]
impl Default for TwitterUser {
	fn default() -> Self {
		Self::UserId(0)
	}
}

#[allow(deprecated)]
impl<'de> Deserialize<'de> for TwitterUser {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>, {
		#[derive(Deserialize)]
		#[serde(untagged)]
		enum TwitterUserHelper {
			UserId(u64),
			String(String),
			SignedId(i64),
		}

		let helper = TwitterUserHelper::deserialize(deserializer)?;
		match helper {
			TwitterUserHelper::UserId(id) => Ok(TwitterUser::UserId(id)),
			TwitterUserHelper::String(s) => parse_twitter_user_str(&s).map_err(de::Error::custom),
			TwitterUserHelper::SignedId(id) =>
				if id < 0 {
					Err(de::Error::custom("Twitter user ID cannot be negative"))
				} else {
					Ok(TwitterUser::UserId(id as u64))
				},
		}
	}
}

#[allow(deprecated)]
impl FromStr for TwitterUser {
	type Err = color_eyre::eyre::Report;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		parse_twitter_user_str(s)
	}
}

#[allow(dead_code, deprecated)]
fn parse_twitter_user_str(s: &str) -> Result<TwitterUser, color_eyre::eyre::Report> {
	let trimmed = s.trim();

	// Handle full URLs: https://twitter.com/username or https://x.com/username
	if let Some(username) = trimmed
		.strip_prefix("https://twitter.com/")
		.or_else(|| trimmed.strip_prefix("https://x.com/"))
		.or_else(|| trimmed.strip_prefix("http://twitter.com/"))
		.or_else(|| trimmed.strip_prefix("http://x.com/"))
	{
		let username = username.split('/').next().unwrap_or(username);
		return Ok(TwitterUser::Username(username.to_string()));
	}

	// Handle @username
	if let Some(username) = trimmed.strip_prefix('@') {
		return Ok(TwitterUser::Username(username.to_string()));
	}

	// Try to parse as numeric ID
	if let Ok(id) = trimmed.parse::<u64>() {
		return Ok(TwitterUser::UserId(id));
	}

	// Otherwise treat as username (alphanumeric + underscore)
	if trimmed.chars().all(|c| c.is_alphanumeric() || c == '_') {
		Ok(TwitterUser::Username(trimmed.to_string()))
	} else {
		Err(eyre!("Invalid Twitter user format: {trimmed}"))
	}
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
	use serde_json::from_str;

	use super::*;

	#[test]
	fn test_deserialize_user_id() {
		let json = r#"1507244316154023968"#;
		let user: TwitterUser = from_str(json).unwrap();
		insta::assert_debug_snapshot!(user, @r#"
		UserId(
		    1507244316154023968,
		)
		"#);
	}

	#[test]
	fn test_deserialize_username() {
		let json = r#""elonmusk""#;
		let user: TwitterUser = from_str(json).unwrap();
		insta::assert_debug_snapshot!(user, @r#"
		Username(
		    "elonmusk",
		)
		"#);
	}

	#[test]
	fn test_deserialize_username_with_at() {
		let json = r#""@elonmusk""#;
		let user: TwitterUser = from_str(json).unwrap();
		insta::assert_debug_snapshot!(user, @r#"
		Username(
		    "elonmusk",
		)
		"#);
	}

	#[test]
	fn test_deserialize_twitter_url() {
		let json = r#""https://twitter.com/elonmusk""#;
		let user: TwitterUser = from_str(json).unwrap();
		insta::assert_debug_snapshot!(user, @r#"
		Username(
		    "elonmusk",
		)
		"#);
	}

	#[test]
	fn test_deserialize_x_url() {
		let json = r#""https://x.com/elonmusk""#;
		let user: TwitterUser = from_str(json).unwrap();
		insta::assert_debug_snapshot!(user, @r#"
		Username(
		    "elonmusk",
		)
		"#);
	}

	#[test]
	fn test_deserialize_numeric_string() {
		let json = r#""1507244316154023968""#;
		let user: TwitterUser = from_str(json).unwrap();
		insta::assert_debug_snapshot!(user, @r#"
		UserId(
		    1507244316154023968,
		)
		"#);
	}

	#[test]
	fn test_from_str_username() {
		let user = TwitterUser::from_str("elonmusk").unwrap();
		assert_eq!(user, TwitterUser::Username("elonmusk".to_string()));
	}

	#[test]
	fn test_from_str_at_username() {
		let user = TwitterUser::from_str("@elonmusk").unwrap();
		assert_eq!(user, TwitterUser::Username("elonmusk".to_string()));
	}

	#[test]
	fn test_from_str_url() {
		let user = TwitterUser::from_str("https://twitter.com/elonmusk").unwrap();
		assert_eq!(user, TwitterUser::Username("elonmusk".to_string()));
	}

	#[test]
	fn test_from_str_user_id() {
		let user = TwitterUser::from_str("1507244316154023968").unwrap();
		assert_eq!(user, TwitterUser::UserId(1507244316154023968));
	}

	#[test]
	fn test_invalid_format() {
		let result = TwitterUser::from_str("invalid@name!");
		assert!(result.is_err());
	}
}
