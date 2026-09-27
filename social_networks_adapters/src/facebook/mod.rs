//! Facebook, read through a logged-in chrome: pages are loaded by URL and read from the JSON they
//! embed and the GraphQL they fetch. Two sessions, never mixed, each paced on its own:
//!
//! ```text
//!   attached  the user's own chrome     city/<page id>   people search under the City filter
//!   launched  our chrome, the burner    group/<id>       a group's member listing
//!                                       profile(<id>)    the About tab, for where somebody lives
//! ```
//!
//! The invariants are in `docs/ARCHITECTURE.md`; why the CDP client is hand-rolled is in
//! `docs/facebook/session_drop.md`.

mod browser;
pub mod lead_rate;
pub mod members;
mod pacer;
pub mod profile;
pub mod search;
mod sway;

use std::{
	collections::{HashMap, HashSet},
	path::PathBuf,
};

use base64::Engine as _;
use color_eyre::eyre::{Result, WrapErr, bail, ensure, eyre};
use jiff::{Timestamp, tz::TimeZone};
use strum::AsRefStr;
use tracing::info;
use v_utils::macros::MyConfigPrimitives;

use self::{browser::Tab, members::Listing, pacer::Pacer, profile::Section, search::Results};
use crate::{
	nominatim::{Coords, Geocoder},
	reach::{Member, Page, Place, Profile, Profiles, Roster, Venue, VenueRef, Window},
};

/// INSEE `nat2022`, births since 1945, most frequent first; see `docs/facebook/detection.md`
const FIRST_NAMES: &str = include_str!("first_names.txt");
const ADDRESS: &str = "a facebook venue is `city/<page id>` or `group/<group id>`";

#[derive(Clone, Debug, MyConfigPrimitives)]
pub struct FacebookConfig {
	#[primitives(skip)]
	pub attached: AttachedConfig,
	#[primitives(skip)]
	pub launched: LaunchedConfig,
	/// A profile visited fewer days ago than this is not visited again.
	pub revisit_days: u32,
}
/// The user's own chrome, with exactly one facebook tab logged in as `user_id` (its `c_user` cookie).
#[derive(Clone, Debug, MyConfigPrimitives)]
pub struct AttachedConfig {
	pub cdp_port: u16,
	pub user_id: String,
	pub views_per_hour: u32,
	pub scrolls_per_hour: u32,
	/// `[min, max]` seconds before each view and each scroll
	#[primitives(skip)]
	pub pause_secs: [u64; 2],
}
/// Our own chrome on our own profile, logged in once by a human through `recon facebook-login`.
#[derive(Clone, Debug, MyConfigPrimitives)]
pub struct LaunchedConfig {
	pub chrome_executable: PathBuf,
	pub views_per_hour: u32,
	pub scrolls_per_hour: u32,
	/// `[min, max]` seconds before each view and each scroll
	#[primitives(skip)]
	pub pause_secs: [u64; 2],
}

pub struct Facebook<'t, 'c> {
	tab: &'t mut Tab<'c>,
	session: Session,
	views: Pacer,
	scrolls: Pacer,
	revisit_days: u32,
	geocoder: Geocoder,
	dir: PathBuf,
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
	let c = &config.launched;
	let (dir, views, scrolls) = state(Session::Launched, c.views_per_hour, c.scrolls_per_hour, c.pause_secs)?;
	let geocoder = Geocoder::try_new()?;
	browser::launch(&c.chrome_executable, &dir.join("chrome"), true, &dir.join("sessions.toml"), async |tab| {
		work(&mut Facebook {
			tab,
			session: Session::Launched,
			views,
			scrolls,
			revisit_days: config.revisit_days,
			geocoder,
			dir: dir.clone(),
		})
		.await
	})
	.await
}

/// A window of the burner's chrome, waiting for a human to log in. Credentials are never ours to type.
pub async fn login(config: &FacebookConfig) -> Result<()> {
	let c = &config.launched;
	let (dir, ..) = state(Session::Launched, c.views_per_hour, c.scrolls_per_hour, c.pause_secs)?;
	browser::launch(&c.chrome_executable, &dir.join("chrome"), false, &dir.join("sessions.toml"), async |tab| {
		tab.goto("https://www.facebook.com/").await
	})
	.await
}

async fn with_attached<T>(config: &FacebookConfig, work: impl AsyncFnOnce(&mut Facebook<'_, '_>) -> Result<T>) -> Result<T> {
	let c = &config.attached;
	let (dir, views, scrolls) = state(Session::Attached, c.views_per_hour, c.scrolls_per_hour, c.pause_secs)?;
	let geocoder = Geocoder::try_new()?;
	browser::attach(c.cdp_port, &c.user_id, &dir.join("sessions.toml"), async |tab| {
		work(&mut Facebook {
			tab,
			session: Session::Attached,
			views,
			scrolls,
			revisit_days: config.revisit_days,
			geocoder,
			dir: dir.clone(),
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

	async fn posts(&mut self, at: &VenueRef, _window: Window, _assets: &std::path::Path) -> Result<Page> {
		bail!("{at}: facebook posts are not read")
	}
}

impl Profiles for Facebook<'_, '_> {
	/// Where they live, their work and education, and the accounts they link. The checkpoint is the
	/// day of the visit, as linkedin's is: a profile visited inside `revisit_days` is not visited again.
	async fn profile(&mut self, handle: &str, window: Window) -> Result<Profile> {
		ensure!(self.session == Session::Launched, "a profile is visited from the launched session only");
		let today = Timestamp::now().to_zoned(TimeZone::UTC).date();
		if let Window::Above { after: Some(last), .. } = &window {
			let last: jiff::civil::Date = last.parse().wrap_err("a facebook checkpoint is a date")?;
			// leaving `activity.newest` unset is what keeps the checkpoint where it is
			if last.until((jiff::Unit::Day, today))?.get_days() < self.revisit_days as i32 {
				return Ok(Profile::default());
			}
		}

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
		profile.activity.newest = Some(today.to_string());
		Ok(profile)
	}
}

impl Facebook<'_, '_> {
	/// People search for each first name in turn under the City filter; everyone it lists counts as
	/// living there. One query's list is capped, which is why it walks names. The cursor is the last
	/// name exhausted.
	async fn city(&mut self, id: &str, roster: &mut impl Roster) -> Result<()> {
		let names: Vec<&str> = FIRST_NAMES.lines().collect();
		let mut cursor = roster.cursor().map(str::to_string);
		let start = match &cursor {
			None => 0,
			Some(done) =>
				1 + names
					.iter()
					.position(|n| n == done)
					.ok_or_else(|| eyre!("the walk stopped after `{done}`, which is not one of its first names"))?,
		};
		let mut rate = lead_rate::Tracker::load(self.dir.join("lead_rate.toml"))?;
		for name in &names[start..] {
			self.views.wait().await?;
			self.tab.goto(&search_url(name, id)).await?;
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

			let mut checked = HashSet::new();
			let mut fresh = check_in_hits(roster, &results, &mut checked, &city, at, &cursor)?;
			let mut idle = 0;
			while results.more != Some(false) && idle < 3 {
				self.scrolls.wait().await?;
				let progressed = self
					.tab
					.scroll(|body| {
						let before = results.hits.len();
						results.absorb(body)?;
						Ok(results.hits.len() > before)
					})
					.await?;
				idle = if progressed { 0 } else { idle + 1 };
				fresh += check_in_hits(roster, &results, &mut checked, &city, at, &cursor)?;
				eprint!("\r`{name}` in {city}: {} listed, {fresh} new", results.hits.len());
			}
			eprintln!();
			cursor = Some(name.to_string());
			roster.check_in(&[], cursor.clone())?;
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
		self.views.wait().await?;
		self.tab.goto(&format!("https://www.facebook.com/groups/{id}/members")).await?;
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
		let mut idle = 0;
		while listing.more != Some(false) && idle < 3 {
			self.scrolls.wait().await?;
			let progressed = self
				.tab
				.scroll(|body| {
					let before = listing.len();
					listing.absorb(body)?;
					Ok(listing.len() > before)
				})
				.await?;
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
		self.views.wait().await?;
		self.tab.goto(&url).await?;
		Section::parse(&self.tab.scripts().await?.join("\n"))
	}
}

#[derive(AsRefStr, Clone, Copy, Debug, Eq, PartialEq)]
#[strum(serialize_all = "lowercase")]
enum Session {
	Attached,
	Launched,
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

/// `$XDG_STATE_HOME/social_networks/facebook/<session>/`, and the session's pacers, whose logs outlive
/// a restart.
fn state(session: Session, views_per_hour: u32, scrolls_per_hour: u32, pause_secs: [u64; 2]) -> Result<(PathBuf, Pacer, Pacer)> {
	let dir = xdg::BaseDirectories::with_prefix("social_networks").create_state_directory(format!("facebook/{}", session.as_ref()))?;
	let views = Pacer::load(dir.join("views"), "page loads", views_per_hour, pause_secs)?;
	let scrolls = Pacer::load(dir.join("scrolls"), "scrolls", scrolls_per_hour, pause_secs)?;
	Ok((dir, views, scrolls))
}

/// The hits not yet in `checked`, as rows placed where the filter says; how many the roster lacked.
fn check_in_hits(roster: &mut impl Roster, results: &Results, checked: &mut HashSet<String>, city: &str, at: Coords, cursor: &Option<String>) -> Result<usize> {
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
	match rows.is_empty() {
		true => Ok(0),
		false => roster.check_in(&rows, cursor.clone()),
	}
}

/// `search/people/?q=<name>&filters=base64({"city:0": "{\"name\":\"users_location\",\"args\":\"<city>\"}"})`
fn search_url(name: &str, city: &str) -> String {
	let filter = serde_json::json!({ "name": "users_location", "args": city }).to_string();
	let filters = base64::engine::general_purpose::STANDARD.encode(serde_json::json!({ "city:0": filter }).to_string());
	reqwest::Url::parse_with_params("https://www.facebook.com/search/people/", [("q", name), ("filters", &filters)])
		.expect("a constant base")
		.into()
}
