//! Facebook, read through a logged-in chrome: pages are loaded by URL and read from the JSON they
//! embed and the GraphQL they fetch. Sessions never mix, each paced on its own:
//!
//! ```text
//!   attached  the user's own chrome     city/<page id>   people search under the City filter
//!   launched  our chrome, the burner    group/<id>       a group's member listing
//!                                       profile(<id>)    the About tab, for where somebody lives
//!   send      our chrome, its account   send(<id>)       a Messenger conversation; `launched` when unset
//!                                       noise(<span>)    idle browsing after it, on either of ours
//! ```
//!
//! The invariants are in `docs/ARCHITECTURE.md`; why the driver must never send `Runtime.enable` is in
//! `docs/facebook/session_drop.md`.

mod browser;
pub mod lead_rate;
pub mod members;
mod messenger;
mod noise;
pub mod profile;
pub mod search;
mod searched;
mod sway;

use std::{
	collections::{HashMap, HashSet},
	path::{Path, PathBuf},
};

use base64::Engine as _;
use color_eyre::eyre::{Result, WrapErr, bail, ensure, eyre};
use strum::AsRefStr;
use tracing::info;
use v_utils::macros::MyConfigPrimitives;

use self::{
	browser::Tab,
	members::Listing,
	profile::Section,
	search::Results,
	searched::{Ended, Query},
};
use crate::{
	behaviour::{Action, Behaviour, BehaviourConfig, Order},
	nominatim::{Coords, Geocoder},
	reach::{Member, Page, Place, Profile, Profiles, Roster, Venue, VenueRef, Window},
};

/// INSEE `nat2022`, births since 1945, most frequent first; see `docs/facebook/detection.md`
const FIRST_NAMES: &str = include_str!("first_names.txt");
const FEED: &str = "https://www.facebook.com/";
const ADDRESS: &str = "a facebook venue is `city/<page id>` or `group/<group id>`";

#[derive(Clone, Debug, MyConfigPrimitives)]
pub struct FacebookConfig {
	#[primitives(skip)]
	pub attached: AttachedConfig,
	#[primitives(skip)]
	pub launched: LaunchedConfig,
	/// The account messages go out from, when it is not `launched`'s
	#[primitives(skip)]
	#[serde(default)]
	pub send: Option<LaunchedConfig>,
}
/// The user's own chrome, with exactly one facebook tab logged in as `user_id` (its `c_user` cookie).
#[derive(Clone, Debug, MyConfigPrimitives)]
pub struct AttachedConfig {
	pub cdp_port: u16,
	pub user_id: String,
	#[primitives(skip)]
	pub behaviour: BehaviourConfig,
}
/// Our own chrome on our own profile, logged in once by a human through `recon facebook-login`.
#[derive(Clone, Debug, MyConfigPrimitives)]
pub struct LaunchedConfig {
	pub chrome_executable: PathBuf,
	#[primitives(skip)]
	pub behaviour: BehaviourConfig,
}

pub struct Facebook<'t, 'c> {
	tab: &'t mut Tab<'c>,
	session: Session,
	behaviour: Behaviour,
	geocoder: Geocoder,
	dir: PathBuf,
	/// the Messenger conversation the tab is on, by handle
	conversation: Option<String>,
}
impl Facebook<'_, '_> {
	/// People search for each first name in turn under the City filter; everyone it lists counts as
	/// living there. One query's list is capped, which is why it walks names, in an [`Order`] kept
	/// in the session's state. The cursor is a [`Walked`], moved on every page.
	async fn city(&mut self, id: &str, roster: &mut impl Roster) -> Result<()> {
		let names = Order::load(FIRST_NAMES.lines(), &self.dir.join("first_names.seed"))?;
		let position = |name: &str| {
			names
				.iter()
				.position(|n| *n == name)
				.ok_or_else(|| eyre!("the walk stopped at `{name}`, which is not one of its first names"))
		};
		let (start, mut resume) = match roster.cursor().map(str::parse::<Walked>).transpose()? {
			None => (0, None),
			Some(Walked::Past(done)) => (1 + position(&done)?, None),
			Some(Walked::Within { name, page }) => (position(&name)?, Some(page)),
		};
		let searched = self.dir.join("searched").join(id);
		std::fs::create_dir_all(&searched).wrap_err_with(|| format!("failed to create {}", searched.display()))?;
		let mut rate = lead_rate::Tracker::load(self.dir.join("lead_rate.toml"))?;
		for name in &names[start..] {
			let mut query = Query::start(&searched, name, resume.is_some())?;
			self.load(&search_url(name, id), &[FEED, &unfiltered_url(name)]).await?;
			let mut results = Results::default();
			for s in self.tab.scripts().await? {
				results.absorb(&s)?;
			}
			let (filter, city) = results.city.clone().ok_or_else(|| eyre!("the search for `{name}` shows no City filter"))?;
			ensure!(filter == id, "asked for city {id}, the page filters by {filter} ({city})");
			let at = self
				.geocoder
				.point(&city)
				.await?
				.ok_or_else(|| eyre!("facebook names city {id} `{city}`, which Nominatim does not know"))?;

			// until the resumed page has landed, a kill has to resume it again rather than this first page
			let walked = |results: &Results, resume: &Option<String>| match (resume, &results.next) {
				(Some(page), _) | (None, Some(page)) => Ok(Walked::Within {
					name: name.to_string(),
					page: page.clone(),
				}),
				(None, None) => match results.more {
					Some(false) => Ok(Walked::Past(name.to_string())),
					_ => Err(eyre!("`{name}` claims more results and names no cursor to them")),
				},
			};
			let mut checked = HashSet::new();
			let mut fresh = check_in_hits(roster, &results, &mut checked, &city, at, walked(&results, &resume)?)?;
			query.page(results.hits.keys(), fresh, false)?;
			let mut seen = results.hits.len();
			let mut idle = 0;
			while results.more != Some(false) && idle < 3 {
				self.behaviour.act(Action::Scroll { seen }).await?;
				let before = results.hits.len();
				let resuming = resume.is_some();
				let progressed = self
					.tab
					.scroll(&mut resume, |body| {
						let before = results.hits.len();
						results.absorb(body)?;
						Ok(results.hits.len() > before)
					})
					.await?;
				seen = results.hits.len() - before;
				idle = if progressed { 0 } else { idle + 1 };
				if resuming && resume.is_none() {
					info!("`{name}` resumed past its last page checked in");
				}
				let new = check_in_hits(roster, &results, &mut checked, &city, at, walked(&results, &resume)?)?;
				fresh += new;
				query.page(results.hits.keys(), new, true)?;
				eprint!("\r`{name}` in {city}: {} listed, {fresh} new", results.hits.len());
			}
			eprintln!();
			query.end(match results.more {
				Some(false) => Ended::Exhausted,
				_ => Ended::Stalled,
			})?;
			ensure!(resume.is_none(), "`{name}` ended before its resumed page was asked for");
			roster.check_in(&[], Some(Walked::Past(name.to_string()).to_string()))?;
			let stalled = match results.more {
				Some(true) => " (stopped scrolling with more claimed)",
				_ => "",
			};
			info!("`{name}` in {city}: {} listed, {fresh} new{stalled}; {}", results.hits.len(), rate.record(fresh as u64)?);
		}
		info!("every first name searched in city {id}");
		Ok(())
	}

	/// The member listing, scrolled until facebook says there is no next page or scrolling stops
	/// producing members. It has no resume point, so a rerun lists from the top.
	async fn group(&mut self, id: &str, roster: &mut impl Roster) -> Result<()> {
		self.load(&format!("https://www.facebook.com/groups/{id}/members"), &[FEED]).await?;
		let mut listing = Listing::default();
		for s in self.tab.scripts().await? {
			listing.absorb(&s)?;
		}
		let group = listing.group()?;
		ensure!(
			group.id == id,
			"groups/{id}/members lists group {} ({}); address it as facebook:group/{}",
			group.id,
			group.name,
			group.id
		);

		let mut checked: HashMap<String, Member> = HashMap::new();
		let mut check_in = |listing: &Listing| -> Result<()> {
			let rows: Vec<Member> = listing.rows(id).into_iter().filter(|m| checked.get(&m.handle) != Some(m)).collect();
			if !rows.is_empty() {
				roster.check_in(&rows, None)?;
				checked.extend(rows.into_iter().map(|m| (m.handle.clone(), m)));
			}
			Ok(())
		};
		check_in(&listing)?;
		let mut seen = listing.len();
		let mut idle = 0;
		while listing.more != Some(false) && idle < 3 {
			self.behaviour.act(Action::Scroll { seen }).await?;
			let before = listing.len();
			let progressed = self
				.tab
				.scroll(&mut None, |body| {
					let before = listing.len();
					listing.absorb(body)?;
					Ok(listing.len() > before)
				})
				.await?;
			seen = listing.len() - before;
			idle = if progressed { 0 } else { idle + 1 };
			check_in(&listing)?;
			eprint!("\r{} members listed", listing.len());
		}
		eprintln!();
		let group = listing.group()?;
		info!("{}: {} members listed", group.name, listing.len());
		Ok(())
	}

	async fn section(&mut self, handle: &str, section: &str) -> Result<Section> {
		// a vanity name is what a link to them carries; the id is what a listing does
		let url = match handle.bytes().all(|b| b.is_ascii_digit()) {
			true => format!("https://www.facebook.com/profile.php?id={handle}&sk={section}"),
			false => format!("https://www.facebook.com/{handle}/{section}"),
		};
		self.load(&url, &[FEED]).await?;
		Section::parse(&self.tab.scripts().await?.join("\n"))
	}

	/// `url`, after as many ordinary loads, picked from `ordinary`, as the behaviour's noise asks for.
	/// Nothing is read from those.
	async fn load(&mut self, url: &str, ordinary: &[&str]) -> Result<()> {
		self.conversation = None;
		while self.behaviour.noise() {
			self.behaviour.act(Action::Load).await?;
			self.tab.goto(ordinary[rand::random_range(..ordinary.len())]).await?;
		}
		self.behaviour.act(Action::Load).await?;
		self.tab.goto(url).await
	}
}

/// The session `at` is read from: a city search in the user's own chrome, a group in the burner's.
pub async fn with_session<T>(config: &FacebookConfig, at: &VenueRef, work: impl AsyncFnOnce(&mut Facebook<'_, '_>) -> Result<T>) -> Result<T> {
	match Slug::of(at)? {
		Slug::City(_) => with_attached(config, work).await,
		Slug::Group(_) => with_launched(config, work).await,
	}
}

/// The burner's chrome, headless. Opened for the one command and closed after it, Ctrl-C included.
pub async fn with_launched<T>(config: &FacebookConfig, work: impl AsyncFnOnce(&mut Facebook<'_, '_>) -> Result<T>) -> Result<T> {
	launched(&config.launched, Session::Launched, work).await
}

/// The chrome messages go out from: `send`'s, or the burner's when it has none.
pub async fn with_sender<T>(config: &FacebookConfig, work: impl AsyncFnOnce(&mut Facebook<'_, '_>) -> Result<T>) -> Result<T> {
	let (c, session) = sender(config);
	launched(c, session, work).await
}

/// A window of the burner's chrome, or with `send` of the send session's, open until the human closes
/// it: for logging in, or for what Messenger asks only a human, its PIN. Credentials are never ours to type.
pub async fn login(config: &FacebookConfig, send: bool) -> Result<()> {
	let (c, session) = match send {
		true => (
			config
				.send
				.as_ref()
				.ok_or_else(|| eyre!("no `facebook.send` session in the config: messages go out from the launched one"))?,
			Session::Send,
		),
		false => (&config.launched, Session::Launched),
	};
	let dir = state(session)?;
	let profile = dir.join("chrome");
	browser::launch(&c.chrome_executable, &profile, false, &dir, async |tab| {
		tab.goto(FEED).await?;
		// chrome's own lock on its profile, `<host>-<pid>`; driven chrome outlives its last window, so the window is what is waited on
		let lock = profile.join("SingletonLock");
		let held = std::fs::read_link(&lock).wrap_err_with(|| format!("chrome holds no {}", lock.display()))?;
		let pid = held
			.to_str()
			.and_then(|l| l.rsplit_once('-'))
			.and_then(|(_, pid)| pid.parse().ok())
			.ok_or_else(|| eyre!("{} points at `{}`, not `<host>-<pid>`", lock.display(), held.display()))?;
		eprintln!("close the chrome window when done");
		sway::closed(pid).await
	})
	.await
}

fn sender(config: &FacebookConfig) -> (&LaunchedConfig, Session) {
	match &config.send {
		Some(c) => (c, Session::Send),
		None => (&config.launched, Session::Launched),
	}
}

async fn launched<T>(c: &LaunchedConfig, session: Session, work: impl AsyncFnOnce(&mut Facebook<'_, '_>) -> Result<T>) -> Result<T> {
	let dir = state(session)?;
	let behaviour = Behaviour::load(&c.behaviour, &dir)?;
	let geocoder = Geocoder::try_new()?;
	browser::launch(&c.chrome_executable, &dir.join("chrome"), true, &dir, async |tab| {
		work(&mut Facebook {
			tab,
			session,
			behaviour,
			geocoder,
			dir: dir.clone(),
			conversation: None,
		})
		.await
	})
	.await
}

async fn with_attached<T>(config: &FacebookConfig, work: impl AsyncFnOnce(&mut Facebook<'_, '_>) -> Result<T>) -> Result<T> {
	let c = &config.attached;
	let dir = state(Session::Attached)?;
	let behaviour = Behaviour::load(&c.behaviour, &dir)?;
	let geocoder = Geocoder::try_new()?;
	browser::attach(c.cdp_port, &c.user_id, &dir, async |tab| {
		work(&mut Facebook {
			tab,
			session: Session::Attached,
			behaviour,
			geocoder,
			dir: dir.clone(),
			conversation: None,
		})
		.await
	})
	.await
}

impl Venue for Facebook<'_, '_> {
	async fn venues(&mut self) -> Result<Vec<VenueRef>> {
		bail!("facebook lists no venues; {ADDRESS}")
	}

	async fn members(&mut self, at: &VenueRef, roster: &mut impl Roster) -> Result<()> {
		match (Slug::of(at)?, self.session) {
			(Slug::City(id), Session::Attached) => self.city(id, roster).await,
			(Slug::Group(id), Session::Launched) => self.group(id, roster).await,
			(_, session) => bail!("{at} is not read on the {} session", session.as_ref()),
		}
	}

	async fn posts(&mut self, at: &VenueRef, _window: Window, _assets: &Path) -> Result<Page> {
		bail!("{at}: facebook posts are not read")
	}
}

impl Profiles for Facebook<'_, '_> {
	/// Where they live, their work and education, and the accounts they link. Every call visits: who
	/// is due one is the purpose's `stale_half_life`'s to say.
	async fn profile(&mut self, handle: &str, _: Window) -> Result<Profile> {
		ensure!(self.session == Session::Launched, "a profile is visited from the launched session only");

		let personal = self.section(handle, "directory_personal_details").await?;
		let mut others = Vec::new();
		for name in ["directory_work", "directory_education", "directory_contact_info"] {
			others.push(match personal.present.iter().any(|s| s == name) {
				true => self.section(handle, name).await?,
				false => Section::default(),
			});
		}
		let [work, education, contact] = &others[..] else { unreachable!("three names") };
		let mut profile = profile::stated(&personal, work, education, contact);
		profile.lives_in = Some(match profile.sources.get("facebook:lives_in").cloned() {
			Some(name) => {
				let at = self
					.geocoder
					.point(&name)
					.await?
					.ok_or_else(|| eyre!("facebook says {handle} lives in `{name}`, which Nominatim does not know"))?;
				Some(Place { name, lat: at.lat, lon: at.lon })
			}
			None => None,
		});
		Ok(profile)
	}
}

#[derive(AsRefStr, Clone, Copy, Debug, Eq, PartialEq)]
#[strum(serialize_all = "lowercase")]
enum Session {
	Attached,
	Launched,
	Send,
}

enum Slug<'a> {
	City(&'a str),
	Group(&'a str),
}
impl<'a> Slug<'a> {
	fn of(at: &'a VenueRef) -> Result<Self> {
		let numeric = |id: &str| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit());
		match at.slug.split_once('/') {
			Some(("city", id)) if numeric(id) => Ok(Self::City(id)),
			Some(("group", id)) if numeric(id) => Ok(Self::Group(id)),
			_ => bail!("{ADDRESS}, got `{}`", at.slug),
		}
	}
}

/// `$XDG_STATE_HOME/social_networks/facebook/<session>/`
fn state(session: Session) -> Result<PathBuf> {
	Ok(xdg::BaseDirectories::with_prefix("social_networks").create_state_directory(format!("facebook/{}", session.as_ref()))?)
}

/// Where a city walk stands: past a name once its query is done, or partway through one.
#[derive(Clone, Debug, PartialEq, derive_more::Display)]
enum Walked {
	#[display("{_0}")]
	Past(String),
	/// `page` is facebook's cursor past the last page checked in
	#[display("{name}@{page}")]
	Within { name: String, page: String },
}
impl std::str::FromStr for Walked {
	type Err = color_eyre::Report;

	fn from_str(s: &str) -> Result<Self> {
		Ok(match s.split_once('@') {
			None => Self::Past(s.to_string()),
			Some((name, page)) => {
				ensure!(!name.is_empty() && !page.is_empty(), "a walk cursor is `<name>` or `<name>@<page>`, got `{s}`");
				Self::Within {
					name: name.to_string(),
					page: page.to_string(),
				}
			}
		})
	}
}

/// The hits not yet in `checked`, as rows placed where the filter says, with the walk moved to
/// `walked` whether or not there were any; how many the roster lacked.
fn check_in_hits(roster: &mut impl Roster, results: &Results, checked: &mut HashSet<String>, city: &str, at: Coords, walked: Walked) -> Result<usize> {
	let rows: Vec<Member> = results
		.hits
		.values()
		.filter(|hit| checked.insert(hit.id.clone()))
		.map(|hit| Member {
			handle: hit.id.clone(),
			display: hit.name.clone(),
			joined: None,
			lat: Some(at.lat),
			lon: Some(at.lon),
			zone: None,
			place: Some(city.to_string()),
			bio: (!hit.snippets.is_empty()).then(|| hit.snippets.join("\n")),
		})
		.collect();
	roster.check_in(&rows, Some(walked.to_string()))
}

/// The same name without the City filter: a search anybody might make.
fn unfiltered_url(name: &str) -> String {
	reqwest::Url::parse_with_params("https://www.facebook.com/search/people/", [("q", name)])
		.expect("a constant base")
		.into()
}

/// `search/people/?q=<name>&filters=base64({"city:0": "{\"name\":\"users_location\",\"args\":\"<city>\"}"})`
fn search_url(name: &str, city: &str) -> String {
	let filter = serde_json::json!({ "name": "users_location", "args": city }).to_string();
	let filters = base64::engine::general_purpose::STANDARD.encode(serde_json::json!({ "city:0": filter }).to_string());
	reqwest::Url::parse_with_params("https://www.facebook.com/search/people/", [("q", name), ("filters", &filters)])
		.expect("a constant base")
		.into()
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The config a live one is: `send` is a whole session of its own, or absent.
	#[test]
	fn messages_go_out_from_the_burner_unless_send_names_its_own_session() {
		let session = |chrome: &str| {
			format!(
				r#"chrome_executable = "{chrome}"
				behaviour = {{ active_hours = [8, 23], burst_min = 1.0, break_min = 1.0, noise_share = 0.0, load = {{ per_hour = 1, per_day = 1, dwell_secs = 1.0, spread = 0.0 }}, scroll = {{ per_hour = 1, per_day = 1, dwell_secs = 1.0, spread = 0.0, read_secs_per_item = 0.0 }} }}"#
			)
		};
		let attached = r#"[attached]
			cdp_port = 1
			user_id = "1"
			behaviour = { active_hours = [8, 23], burst_min = 1.0, break_min = 1.0, noise_share = 0.0, load = { per_hour = 1, per_day = 1, dwell_secs = 1.0, spread = 0.0 }, scroll = { per_hour = 1, per_day = 1, dwell_secs = 1.0, spread = 0.0, read_secs_per_item = 0.0 } }"#;
		let shared: FacebookConfig = toml::from_str(&format!("{attached}\n[launched]\n{}", session("/burner"))).unwrap();
		let (c, s) = sender(&shared);
		assert_eq!((c.chrome_executable.to_str(), s), (Some("/burner"), Session::Launched));

		let apart: FacebookConfig = toml::from_str(&format!("{attached}\n[launched]\n{}\n[send]\n{}", session("/burner"), session("/sender"))).unwrap();
		let (c, s) = sender(&apart);
		assert_eq!((c.chrome_executable.to_str(), s), (Some("/sender"), Session::Send));
	}
}
