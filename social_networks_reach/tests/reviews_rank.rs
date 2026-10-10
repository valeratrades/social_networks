//! The ranking formula's invariants, over the reviews purpose as `examples/purposes/reviews.nix`
//! writes it. Each cohort is person files and year files on disk, loaded and ranked the way a command
//! does; what is asserted is order, never a reimplementation of the arithmetic.
//!
//! Needs `nix` to evaluate the purpose and the person files, which the dev shell provides.

use std::{
	collections::BTreeMap,
	path::{Path, PathBuf},
};

use derivs::{Kind, Tape};
use jiff::{SignedDuration, Timestamp, tz::TimeZone};
use social_networks_reach::{
	history::ME,
	person::{self, Person},
	purpose::{Purpose, Purposes},
	rank,
};

const MINE: bool = true;
const THEIRS: bool = false;

struct Lead {
	name: &'static str,
	/// The body of `tags = { … };`, as a human would write it.
	tags: &'static str,
	/// Each line of our conversation, and whether I wrote it.
	lines: Vec<(Timestamp, bool)>,
}

const fn lead(name: &'static str, tags: &'static str) -> Lead {
	Lead { name, tags, lines: Vec::new() }
}

fn at(rfc3339: &str) -> Timestamp {
	rfc3339.parse().unwrap()
}

/// `seconds` past the start of the UTC day `ago` before now.
fn day_ago(ago: SignedDuration, seconds: i8) -> Timestamp {
	(Timestamp::now() - ago)
		.to_zoned(TimeZone::UTC)
		.date()
		.at(0, 0, seconds, 0)
		.to_zoned(TimeZone::UTC)
		.unwrap()
		.timestamp()
}

/// Best first, with the score each got.
fn check(cohort: &str, leads: &[Lead]) -> Vec<(String, f64)> {
	let ranked = on_disk(cohort, leads, |purpose, venues, people| rank::rank(purpose, venues, people).unwrap());
	ranked.into_iter().map(|r| (r.person.name, r.score)).collect()
}

/// `leads` written out as person files and year files, loaded and handed to `f` the way a command does.
fn on_disk<T>(cohort: &str, leads: &[Lead], f: impl FnOnce(&Purpose, &Path, Vec<Person>) -> T) -> T {
	let dir = std::env::temp_dir().join(format!("social_networks_reviews_rank_{}_{cohort}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	let people = dir.join("people");
	for lead in leads {
		let person = people.join(lead.name);
		std::fs::create_dir_all(&person).unwrap();
		std::fs::write(person.join("__main__.nix"), format!("{{ tags = {{ {} }}; }}\n", lead.tags)).unwrap();
		let mut lines = lead.lines.clone();
		lines.sort();
		let (mut years, mut day) = (BTreeMap::new(), None);
		for (at, mine) in lines {
			let at = at.to_zoned(TimeZone::UTC);
			let body = years.entry(at.year()).or_insert_with(|| format!("# {} — {} (times UTC)\n", lead.name, at.year()));
			if day != Some(at.date()) {
				body.push_str(&format!("\n## {}\n\n", at.date()));
				day = Some(at.date());
			}
			let who = if mine { ME } else { lead.name };
			body.push_str(&format!("- {:02}:{:02}:{:02} [{who}/skool] hi\n", at.hour(), at.minute(), at.second()));
		}
		for (year, body) in years {
			std::fs::write(person.join(format!("{year}.md")), body).unwrap();
		}
	}

	let purpose = reviews(&people);
	let loaded = person::load_dir(&purpose).unwrap().into_values().collect();
	let out = f(&purpose, &dir.join("venues"), loaded);
	std::fs::remove_dir_all(&dir).unwrap();
	out
}

fn reviews(path: &Path) -> Purpose {
	let file = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../examples/purposes/reviews.nix");
	let out = std::process::Command::new("nix")
		.args(["eval", "--impure", "--json", "--file"])
		.arg(&file)
		.output()
		.expect("`nix` on PATH; the dev shell provides it");
	assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
	let mut raw: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
	raw["path"] = serde_json::json!(path);
	let purposes: Purposes = serde_json::from_value(serde_json::json!({ "reviews": raw })).unwrap();
	purposes.get("reviews").unwrap().clone()
}

fn score(ranked: &[(String, f64)], name: &str) -> f64 {
	ranked.iter().find(|(n, _)| n == name).unwrap_or_else(|| panic!("{name} was ranked")).1
}

fn order(ranked: &[(String, f64)]) -> Vec<&str> {
	ranked.iter().map(|(n, _)| n.as_str()).collect()
}

#[test]
fn an_owner_outranks_an_otherwise_identical_non_owner() {
	let ranked = check(
		"owner",
		&[lead("employee", "business = false; interest = 0.5;"), lead("owner", "business = true; interest = 0.5;")],
	);
	assert_eq!(order(&ranked), ["owner", "employee"]);
}

/// Inside the radius distance does not matter; beyond it every step further away costs.
#[test]
fn distance_costs_only_outside_the_radius() {
	let ranked = check(
		"distance",
		&[
			// Paris is the business location in the example
			lead("far", r#"lives_in = { name = "Lyon"; lat = 45.764; lon = 4.8357; };"#),
			lead("center", r#"lives_in = { name = "Paris"; lat = 48.8566; lon = 2.3522; };"#),
			lead("suburb", r#"lives_in = { name = "Versailles"; lat = 48.8049; lon = 2.1204; };"#),
			lead("near", r#"lives_in = { name = "Chartres"; lat = 48.4439; lon = 1.489; };"#),
		],
	);
	assert_eq!(score(&ranked, "center"), score(&ranked, "suburb"));
	assert!(score(&ranked, "suburb") > score(&ranked, "near"));
	assert!(score(&ranked, "near") > score(&ranked, "far"));
}

/// Absent is no credit, so a lost closeness bonus is the whole of a penalty for distance: somebody
/// known to live far away still beats somebody whose place is unknown.
#[test]
fn a_known_value_beats_an_absent_one() {
	let ranked = check(
		"known",
		&[
			lead("unknown", ""),
			lead("far", r#"lives_in = { name = "Lyon"; lat = 45.764; lon = 4.8357; };"#),
			lead("aged", "birthday = { min = 2010; max = 2012; };"),
		],
	);
	assert!(score(&ranked, "far") > score(&ranked, "unknown"));
	assert!(score(&ranked, "aged") > score(&ranked, "unknown"));
}

#[test]
fn more_days_of_interaction_beat_fewer() {
	let ranked = check(
		"days",
		&[
			// the same newest line, so `last_interaction` cannot tell them apart; named so that a tie would
			// put them the other way round
			Lead {
				name: "seldom",
				tags: "",
				lines: vec![(at("2026-03-05T10:00:00Z"), THEIRS)],
			},
			Lead {
				name: "zealous",
				tags: "",
				lines: vec![(at("2026-03-01T10:00:00Z"), THEIRS), (at("2026-03-03T10:00:00Z"), THEIRS), (at("2026-03-05T10:00:00Z"), THEIRS)],
			},
		],
	);
	assert_eq!(order(&ranked), ["zealous", "seldom"]);
}

/// Twins share a timeline to the second and differ only in who wrote a line, so whatever separates
/// them is the decay on our unanswered line.
#[test]
fn our_last_message_cools_a_lead_until_it_decays_or_they_answer() {
	let (yesterday, month) = (SignedDuration::from_hours(24), SignedDuration::from_hours(24 * 30));
	let conversation = |name, ago, authors: [bool; 3]| Lead {
		name,
		tags: "business = true;",
		lines: authors.into_iter().zip(0..).map(|(mine, s)| (day_ago(ago, s), mine)).collect(),
	};
	let ranked = check(
		"unanswered",
		&[
			conversation("just_messaged", yesterday, [THEIRS, THEIRS, MINE]),
			conversation("answered", yesterday, [THEIRS, MINE, THEIRS]),
			conversation("never_messaged", yesterday, [THEIRS, THEIRS, THEIRS]),
			conversation("messaged_a_month_ago", month, [THEIRS, THEIRS, MINE]),
			conversation("silent_a_month", month, [THEIRS, THEIRS, THEIRS]),
		],
	);
	assert_eq!(order(&ranked).last(), Some(&"just_messaged"));
	assert_eq!(score(&ranked, "answered"), score(&ranked, "never_messaged"), "their reply lifts it");
	let (cooled, warm) = (score(&ranked, "messaged_a_month_ago"), score(&ranked, "silent_a_month"));
	assert!(cooled < warm && cooled > 0.9 * warm, "a month is four half-lives of a week: {cooled} against {warm}");
}

fn graph(cohort: &str) -> Tape {
	on_disk(
		cohort,
		&[
			lead(
				"owner",
				r#"business = true; birthday = { min = 1999; max = 2001; }; lives_in = { name = "Paris"; lat = 48.8566; lon = 2.3522; };"#,
			),
			Lead {
				name: "talker",
				tags: "interest = 0.5; last_login = \"2026-01-01T00:00:00Z\";",
				lines: vec![(at("2026-03-01T10:00:00Z"), THEIRS), (at("2026-03-05T10:00:00Z"), MINE)],
			},
			lead("unknown", ""),
		],
		|purpose, venues, people| rank::graph(purpose, venues, people).unwrap(),
	)
}

/// The shape of the computation, never its values: a node appearing, vanishing or reading something else is what this pins.
#[test]
fn the_reviews_ranking_is_this_graph() {
	let tape = graph("graph_shape");
	let lines: Vec<String> = tape
		.nodes()
		.iter()
		.map(|node| {
			let kind = match &node.kind {
				Kind::Source { writer, .. } => format!("Source by {writer}"),
				Kind::Reduce { names } => format!("Reduce to {}", names.join(", ")),
				Kind::Map { .. } => "Map".to_string(),
				Kind::Cohort => "Cohort".to_string(),
				Kind::WeightedMean { weights } => format!("WeightedMean {weights:?}"),
				Kind::Gate => "Gate".to_string(),
			};
			let deps: Vec<&str> = node.deps.iter().map(|id| tape.node(*id).name.as_str()).collect();
			match deps.is_empty() {
				true => format!("{:<22} {kind}", node.name),
				false => format!("{:<22} {kind} <- {}", node.name, deps.join(", ")),
			}
		})
		.collect();
	insta::assert_snapshot!(lines.join("\n"));
}

/// A term relative to the cohort reads the cohort: its `v` has an edge from what it reduced everybody to.
#[test]
fn every_cohort_coupled_term_reads_a_reduction() {
	let tape = graph("graph_reductions");
	for of in ["interactions", "last_interaction", "last_login"] {
		let v = tape.nodes().iter().find(|n| n.name == format!("{of}.v")).unwrap_or_else(|| panic!("{of}.v is recorded"));
		assert!(v.deps.iter().any(|id| matches!(tape.node(*id).kind, Kind::Reduce { .. })), "{of}.v reads no reduction");
	}
}

#[test]
fn a_tape_round_trips_through_json() {
	let tape = graph("graph_json");
	let json = serde_json::to_string(&tape).unwrap();
	assert_eq!(serde_json::from_str::<Tape>(&json).unwrap(), tape);
}
