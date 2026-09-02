//! The on-demand axis: what a platform can be asked *now*, as opposed to the stream a
//! [`Client`](crate::client::Client) listens to forever.
//!
//! ```text
//!   person ──► Profiles::profile ──► Profile      what the platform states, plus public activity
//!          ──► Direct::direct   ──► Page          the conversation with them
//!          ──► Direct::send                       the one thing that goes out
//!   venue  ──► Venue::venues    ──► [VenueRef]    what this session can see
//!          ──► Venue::members   ──► [Member]
//!          ──► Venue::posts     ──► Page
//! ```
//!
//! Everything above returns [`Item`]s, and an item carries its own author — so a direct message, a
//! group post and a public event differ in [`Kind`] and in nothing else. [`Direct::send`] sits
//! beside the read because it is the same session, the same auth and the same addressing.
//!
//! Reading is addressed by handle or by [`VenueRef`]; the platform decides what either resolves to.
//! Callers dispatch over [`Source`] and [`VenueSource`] rather than over `dyn`, so a platform that
//! gains an axis cannot fall through to an arm that silently fetches nothing.

use std::{collections::BTreeMap, path::Path, str::FromStr};

use color_eyre::eyre::{Result, bail, eyre};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use strum::{AsRefStr, EnumIter, EnumString};
use tracing::warn;

/// The window the rolodex extraction prompt reads, and therefore how much of a conversation a first
/// read puts in front of it — the history under that is the backfill's, not the prompt's.
pub const INITIAL_ITEMS: usize = 200;
/// Ceiling per read once a checkpoint exists. Whatever is left over is picked up by the next run.
pub const MAX_ITEMS: usize = 500;
/// The largest page a backfill asks for. Discord rejects a `limit` above it, and telegram serves it.
pub const PAGE: usize = 100;

/// What a platform states about one person.
#[trait_variant::make(Send)]
pub trait Profiles {
	/// `window` bounds the public activity only: what the platform *states* is a snapshot, and
	/// diffing it against the person file is what decides whether it changed.
	async fn profile(&mut self, handle: &str, window: Window) -> Result<Profile>;
}
/// A conversation with one person. `send` is here because it is the same session, the same auth and
/// the same addressing as the read.
#[trait_variant::make(Send)]
pub trait Direct {
	async fn direct(&mut self, handle: &str, window: Window, assets: &Path) -> Result<Page>;
	async fn send(&mut self, handle: &str, text: &str) -> Result<()>;
}
/// A named place with members and content.
#[trait_variant::make(Send)]
pub trait Venue {
	/// What this session can see. Without it a venue selector can only guess at slugs.
	async fn venues(&mut self) -> Result<Vec<VenueRef>>;
	async fn members(&mut self, at: &VenueRef) -> Result<Vec<Member>>;
	async fn posts(&mut self, at: &VenueRef, window: Window, assets: &Path) -> Result<Page>;
}
/// A platform the person axis can be addressed on. `as_ref` is the `handles` key a person file
/// spells, so a source cannot be reachable under a name the files do not use.
#[derive(AsRefStr, Clone, Copy, Debug, Deserialize, EnumIter, EnumString, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[strum(serialize_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum Source {
	Discord,
	Telegram,
	Github,
	Linkedin,
	Skool,
}
impl Source {
	/// Whether there is anything below the newest item to page down to. A github feed and a linkedin
	/// profile are snapshots, so their backfill is over before it starts.
	pub fn has_history(self) -> bool {
		matches!(self, Self::Discord | Self::Telegram | Self::Skool)
	}
}

/// The platforms that implement [`Venue`]. Separate from [`Source`] so that adding a venue to a
/// platform is a variant nothing compiles without handling, rather than an arm that fetches nothing.
#[derive(AsRefStr, Clone, Copy, Debug, Deserialize, EnumIter, EnumString, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[strum(serialize_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum VenueSource {
	Telegram,
	Github,
	Skool,
}
impl From<VenueSource> for Source {
	fn from(venue: VenueSource) -> Self {
		match venue {
			VenueSource::Telegram => Self::Telegram,
			VenueSource::Github => Self::Github,
			VenueSource::Skool => Self::Skool,
		}
	}
}

/// A named place with members and content: a skool group, a telegram chat, a github org or repo.
/// `slug` is whatever the platform's own URL uses, so `<platform>:<slug>` round-trips through the
/// command line and through a directory name.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct VenueRef {
	pub platform: VenueSource,
	pub slug: String,
	/// What the platform prints for it. The slug when it prints nothing else.
	pub display: String,
}
impl VenueRef {
	pub fn new(platform: VenueSource, slug: impl Into<String>) -> Self {
		let slug = slug.into();
		Self {
			platform,
			display: slug.clone(),
			slug,
		}
	}
}
impl std::fmt::Display for VenueRef {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}:{}", self.platform.as_ref(), self.slug)
	}
}
impl FromStr for VenueRef {
	type Err = color_eyre::eyre::Report;

	/// A github repo slug carries a `/` and a telegram invite link carries `:` in neither half, so
	/// the split is on the *first* colon only.
	fn from_str(s: &str) -> Result<Self> {
		let (platform, slug) = s.split_once(':').ok_or_else(|| eyre!("a venue is addressed `<platform>:<slug>`, got `{s}`"))?;
		if slug.is_empty() {
			bail!("a venue is addressed `<platform>:<slug>`, got `{s}`");
		}
		let known = || {
			use strum::IntoEnumIterator as _;
			VenueSource::iter().map(|v| v.as_ref().to_string()).collect::<Vec<_>>().join(", ")
		};
		Ok(Self::new(platform.parse().map_err(|_| eyre!("`{platform}` has no venues; one of {}", known()))?, slug))
	}
}

/// One member of a venue. `handle` is what [`Profiles::profile`] and the `handles` map address, so
/// a roster row and a person file speak the same name.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Member {
	pub handle: String,
	pub display: String,
	/// When they joined *this venue*, which is not when they made the account. `None` when the
	/// platform does not state it.
	pub joined: Option<Timestamp>,
	/// Where the platform puts them, when it puts them anywhere. Deliberately coarse: skool offsets
	/// every pin by 10+ miles, so this answers "which part of the world" and nothing finer.
	pub lat: Option<f64>,
	pub lon: Option<f64>,
	/// An IANA zone — `Europe/Paris`. Where a pin is missing, this is often the only signal left, and
	/// its first component is the continent.
	pub zone: Option<String>,
}

/// What a platform states about one person, and what they did in public above the window asked for.
#[derive(Default)]
pub struct Profile {
	/// Verbatim platform-authored text, keyed `platform:kind` — `skool:bio`, `telegram:about`.
	pub sources: BTreeMap<String, String>,
	/// Other platforms this one names, keyed by platform.
	pub handles: BTreeMap<String, String>,
	/// Where the platform says they are a member.
	pub venues: Vec<VenueRef>,
	pub activity: Page,
}
impl Profile {
	/// Record a platform-authored text under `key`, dropping the empty string platforms store for a
	/// field nobody filled in.
	pub fn state(&mut self, key: &str, value: Option<&str>) {
		if let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) {
			self.sources.insert(key.to_string(), value.to_string());
		}
	}
}

/// One read's worth of items and where the next one resumes. `newest` is the checkpoint a steady
/// read continues above; `oldest` the floor a backfill continues below.
#[derive(Default)]
pub struct Page {
	/// Oldest-first.
	pub items: Vec<Item>,
	pub newest: Option<String>,
	pub oldest: Option<String>,
	/// Nothing exists below `oldest`.
	pub exhausted: bool,
}

/// Where a read starts and what stops it. The two variants are the two walks that exist: forward
/// from a checkpoint, and one page further down than we have ever been.
#[derive(Clone, Debug)]
pub enum Window {
	/// Newest-first, stopping at whichever comes first — the checkpoint, the date, or the limit.
	Above {
		/// The newest item already seen. `None` is a first read.
		after: Option<String>,
		not_before: Option<Timestamp>,
		limit: usize,
	},
	/// One page strictly older than the floor. `None` starts at the newest item.
	Below { before: Option<String>, limit: usize },
}
impl Window {
	pub fn above(after: Option<String>) -> Self {
		let limit = if after.is_some() { MAX_ITEMS } else { INITIAL_ITEMS };
		Self::Above { after, not_before: None, limit }
	}

	pub fn below(before: Option<String>) -> Self {
		Self::Below { before, limit: PAGE }
	}

	/// One item, asked only to learn whether there is one at all.
	pub fn probe() -> Self {
		Self::Above {
			after: None,
			not_before: None,
			limit: 1,
		}
	}

	/// The date is the bound, so there is no second one: a read asked to go back to a day and stopped
	/// at a count would leave a hole nothing walks down into.
	pub fn since(at: Timestamp) -> Self {
		Self::Above {
			after: None,
			not_before: Some(at),
			limit: usize::MAX,
		}
	}

	/// Whether `at` is under the floor this window sets, and so nothing the caller wants.
	pub fn reached(&self, at: Timestamp) -> bool {
		matches!(self, Self::Above { not_before: Some(floor), .. } if at < *floor)
	}

	pub fn limit(&self) -> usize {
		match *self {
			Self::Above { limit, .. } | Self::Below { limit, .. } => limit,
		}
	}
}

/// One thing somebody wrote. What separates a DM from a group post is [`Kind`] and the author —
/// everything else about them is the same, which is why one type carries both.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Item {
	pub id: String,
	pub source: Source,
	pub at: Timestamp,
	pub kind: Kind,
	pub author: Author,
	pub text: String,
	pub attachments: Vec<Attachment>,
	pub permalink: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
	Direct,
	Post,
	Comment,
	/// Something a platform reports a person did, rather than something they wrote — a release, a
	/// new repository. Recorded under a far higher bar; see the prompt in `rolodex::delta`.
	Activity,
}

/// Who wrote an item. In a venue "outgoing" means nothing and "who wrote it" means everything, so
/// the author is the field rather than a flag.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Author {
	/// The session's own account, whichever platform it is on.
	Me,
	/// The handle the platform attributes the item to.
	Handle(String),
}
impl Author {
	pub fn handle(&self) -> Option<&str> {
		match self {
			Self::Me => None,
			Self::Handle(handle) => Some(handle),
		}
	}
}

/// An image is kept: converted once, under a name its own id determines, so a re-download costs
/// nothing. Everything else is named and not kept — a transcript that says a file went by is worth
/// far more than the bytes of it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Attachment {
	Image { file: String },
	File { name: String },
}
impl Attachment {
	/// An image that will not convert is still an attachment that went by, and losing the whole page
	/// of a backfill over one of them would cost the conversation around it.
	pub fn keep(bytes: &[u8], mime: &str, name: String, assets: &Path, file: String) -> Self {
		if !social_networks_utils::avif::still(mime, bytes) {
			return Self::File { name };
		}
		match social_networks_utils::avif::convert(bytes, &assets.join(&file)) {
			Ok(()) => Self::Image { file },
			Err(e) => {
				warn!("`{name}` stays a filename: {e:#}");
				Self::File { name }
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A variant whose name does not round-trip is one `handles` or a `<platform>:<slug>` argument
	/// cannot address, and it would never be fetched rather than fail.
	#[test]
	fn every_source_is_addressable() {
		use strum::IntoEnumIterator as _;
		for source in Source::iter() {
			assert!(matches!(source.as_ref().parse::<Source>(), Ok(parsed) if parsed == source), "{}", source.as_ref());
		}
		for venue in VenueSource::iter() {
			assert!(matches!(venue.as_ref().parse::<VenueSource>(), Ok(parsed) if parsed == venue), "{}", venue.as_ref());
			// a venue platform that is not also a source could never attribute an item
			assert!(Source::iter().any(|s| s == Source::from(venue)), "{}", venue.as_ref());
		}
	}

	/// A github repo slug is `owner/name` and carries the separator the address itself uses.
	#[test]
	fn a_venue_address_survives_its_slug() {
		let repo: VenueRef = "github:valeratrades/social_networks".parse().unwrap();
		assert_eq!(repo.platform, VenueSource::Github);
		assert_eq!(repo.slug, "valeratrades/social_networks");
		assert_eq!(repo.to_string(), "github:valeratrades/social_networks");

		assert!("skool:".parse::<VenueRef>().is_err());
		assert!("20kmodrop".parse::<VenueRef>().is_err());
		assert!("linkedin:acme".parse::<VenueRef>().is_err(), "linkedin implements no venue");
	}
}
