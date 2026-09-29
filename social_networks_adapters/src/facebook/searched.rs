//! A record per city query, `searched/<city id>/<name>.toml`: what it listed and how it ended, so
//! which query found whom outlives the roster. Nothing reads it to decide what to search; a query
//! resumed partway adds to its own.

use std::path::{Path, PathBuf};

use color_eyre::eyre::{Result, WrapErr};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Ended {
	Exhausted,
	Stalled,
	/// dropped unended: Ctrl-C, or an error out of the run
	Interrupted,
}

/// Written at the start and on every page; dropped without [`Query::end`], it ends `interrupted`.
pub(super) struct Query {
	record: Record,
	/// listed before a resume, which this run's listing does not repeat
	prior: Vec<String>,
	path: PathBuf,
}
impl Query {
	/// `resumed` carries on the record the query left when it was cut off partway.
	pub(super) fn start(dir: &Path, name: &str, resumed: bool) -> Result<Self> {
		assert!(!name.contains('/'), "a first name is a file name");
		let path = dir.join(format!("{name}.toml"));
		let record = match resumed {
			true => {
				let s = std::fs::read_to_string(&path).wrap_err_with(|| format!("the walk resumes `{name}` partway, and its record {} is unreadable", path.display()))?;
				let mut r: Record = toml::from_str(&s).wrap_err_with(|| format!("{} is not a query record", path.display()))?;
				(r.finished, r.ended) = (None, None);
				r
			}
			false => Record {
				started: Timestamp::now(),
				finished: None,
				listed: 0,
				new: 0,
				scrolls: 0,
				ended: None,
				ids: Vec::new(),
			},
		};
		let q = Self {
			prior: record.ids.clone(),
			record,
			path,
		};
		q.write()?;
		Ok(q)
	}

	/// Everyone listed so far, how many of them the roster lacked, and one more scroll unless it is the first page.
	pub(super) fn page<'a>(&mut self, ids: impl IntoIterator<Item = &'a String>, new: usize, scrolled: bool) -> Result<()> {
		let r = &mut self.record;
		r.ids = self.prior.clone();
		r.ids.extend(ids.into_iter().filter(|id| !self.prior.contains(id)).cloned());
		r.listed = r.ids.len();
		r.new += new;
		r.scrolls += scrolled as u32;
		self.write()
	}

	pub(super) fn end(mut self, ended: Ended) -> Result<()> {
		self.record.ended = Some(ended);
		self.record.finished = Some(Timestamp::now());
		self.write()
	}

	fn write(&self) -> Result<()> {
		std::fs::write(&self.path, toml::to_string(&self.record)?).wrap_err_with(|| format!("failed to write {}", self.path.display()))
	}
}

#[derive(Deserialize, Serialize)]
struct Record {
	started: Timestamp,
	finished: Option<Timestamp>,
	listed: usize,
	new: usize,
	scrolls: u32,
	ended: Option<Ended>,
	ids: Vec<String>,
}
impl Drop for Query {
	fn drop(&mut self) {
		if self.record.ended.is_none() {
			self.record.ended = Some(Ended::Interrupted);
			self.record.finished = Some(Timestamp::now());
			self.write().expect("the record was writable at its start");
		}
	}
}

#[cfg(test)]
mod tests {
	use std::path::Path;

	use super::*;

	fn read(dir: &Path, name: &str) -> toml::Table {
		toml::from_str(&std::fs::read_to_string(dir.join(format!("{name}.toml"))).unwrap()).unwrap()
	}

	#[test]
	fn a_query_dropped_mid_walk_keeps_its_partial_ids_as_interrupted() {
		let dir = std::env::temp_dir().join(format!("searched_{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let ids = |n: usize| (0..n).map(|i| format!("1000{i}")).collect::<Vec<_>>();

		let mut q = Query::start(&dir, "jean", false).unwrap();
		assert!(!read(&dir, "jean").contains_key("ended"));
		q.page(&ids(10), 10, false).unwrap();
		q.page(&ids(21), 9, true).unwrap();
		drop(q);
		let r = read(&dir, "jean");
		assert_eq!(r["ended"].as_str(), Some("interrupted"));
		assert_eq!((r["listed"].as_integer(), r["new"].as_integer(), r["scrolls"].as_integer()), (Some(21), Some(19), Some(1)));
		assert_eq!(r["ids"].as_array().unwrap().len(), 21);

		let mut q = Query::start(&dir, "jean", false).unwrap();
		q.page(&ids(3), 0, false).unwrap();
		q.end(Ended::Exhausted).unwrap();
		let r = read(&dir, "jean");
		assert_eq!(r["ended"].as_str(), Some("exhausted"));
		assert!(r["finished"].as_str().is_some());
		std::fs::remove_dir_all(&dir).unwrap();
	}
}
