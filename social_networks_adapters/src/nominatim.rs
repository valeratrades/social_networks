//! Nominatim, at the ≤1 req/s its usage policy allows. Lookups are remembered on disk forever, misses
//! included: a place string names the same place next year.

use std::{
	collections::BTreeMap,
	path::PathBuf,
	time::{Duration, Instant},
};

use color_eyre::eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};

pub struct Geocoder {
	http: reqwest::Client,
	last: Option<Instant>,
	cache_path: PathBuf,
	cache: BTreeMap<String, Geocoded>,
}
impl Geocoder {
	pub fn try_new() -> Result<Self> {
		let cache_path = xdg::BaseDirectories::with_prefix("social_networks").place_cache_file("geocode.toml")?;
		let cache = match std::fs::read_to_string(&cache_path) {
			Ok(s) => toml::from_str(&s).wrap_err_with(|| format!("{} is corrupt", cache_path.display()))?,
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
			Err(e) => return Err(e).wrap_err_with(|| format!("failed to read {}", cache_path.display())),
		};
		let http = reqwest::Client::builder().user_agent(concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"))).build()?;
		Ok(Self {
			http,
			last: None,
			cache_path,
			cache,
		})
	}

	/// `None` is Nominatim knowing no such place.
	pub async fn point(&mut self, place: &str) -> Result<Option<Coords>> {
		if let Some(hit) = self.cache.get(place) {
			return Ok(hit.found);
		}
		if let Some(last) = self.last {
			tokio::time::sleep(Duration::from_millis(1100).saturating_sub(last.elapsed())).await;
		}
		self.last = Some(Instant::now());
		let hits: Vec<Hit> = self
			.http
			.get("https://nominatim.openstreetmap.org/search")
			.query(&[("q", place), ("format", "jsonv2"), ("limit", "1")])
			.send()
			.await?
			.error_for_status()?
			.json()
			.await
			.wrap_err_with(|| format!("Nominatim answer for `{place}`"))?;
		let found = match hits.into_iter().next() {
			Some(hit) => Some(Coords {
				lat: hit.lat.parse()?,
				lon: hit.lon.parse()?,
			}),
			None => None,
		};
		self.cache.insert(place.to_string(), Geocoded { found });
		std::fs::write(&self.cache_path, toml::to_string(&self.cache)?).wrap_err_with(|| format!("failed to write {}", self.cache_path.display()))?;
		Ok(found)
	}
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct Coords {
	pub lat: f64,
	pub lon: f64,
}

#[derive(Debug, Deserialize, Serialize)]
struct Geocoded {
	found: Option<Coords>,
}

#[derive(Deserialize)]
struct Hit {
	lat: String,
	lon: String,
}
