//! The ranking formula's invariants, over the reviews purpose as `examples/purposes/reviews.nix`
//! writes it. Each cohort is person files and year files on disk, loaded and ranked the way a command
//! does; what is asserted is order, never a reimplementation of the arithmetic.
//!
//! Needs `nix` to evaluate the purpose and the person files, which the dev shell provides.

use std::path::{Path, PathBuf};

use social_networks_reach::{person, purpose::Purposes, rank};

struct Lead {
	name: &'static str,
	/// The body of `tags = { … };`, as a human would write it.
	tags: &'static str,
	/// `(day, time)` of each of their lines to me.
	lines: &'static [(&'static str, &'static str)],
	/// `(day, time)` of each of my lines to them.
	ours: &'static [(&'static str, &'static str)],
}

const fn lead(name: &'static str, tags: &'static str) -> Lead {
	Lead { name, tags, lines: &[], ours: &[] }
}

/// Best first, with the score each got.
fn check(cohort: &str, leads: &[Lead]) -> Vec<(String, f64)> {
	let dir = std::env::temp_dir().join(format!("social_networks_reviews_rank_{}_{cohort}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	let people = dir.join("people");
	for lead in leads {
		let person = people.join(lead.name);
		std::fs::create_dir_all(&person).unwrap();
		std::fs::write(person.join("__main__.nix"), format!("{{ tags = {{ {} }}; }}\n", lead.tags)).unwrap();
		let mut lines: Vec<(&str, &str, &str)> = lead
			.lines
			.iter()
			.map(|&(day, time)| (day, time, lead.name))
			.chain(lead.ours.iter().map(|&(day, time)| (day, time, "me")))
			.collect();
		if !lines.is_empty() {
			lines.sort();
			let mut body = format!("# {} — 2026 (times UTC)\n", lead.name);
			for (i, (day, time, by)) in lines.iter().enumerate() {
				if i == 0 || lines[i - 1].0 != *day {
					body.push_str(&format!("\n## {day}\n\n"));
				}
				body.push_str(&format!("- {time} [{by}/skool] hi\n"));
			}
			std::fs::write(person.join("2026.md"), body).unwrap();
		}
	}

	let purpose = reviews(&people);
	let loaded = person::load_dir(&purpose).unwrap().into_values().collect();
	let ranked = rank::rank(&purpose, &dir.join("venues"), loaded).unwrap();
	std::fs::remove_dir_all(&dir).unwrap();
	ranked.into_iter().map(|r| (r.person.name, r.score)).collect()
}

fn reviews(path: &Path) -> social_networks_reach::purpose::Purpose {
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
				lines: &[("2026-03-05", "10:00:00")],
				ours: &[],
			},
			Lead {
				name: "zealous",
				tags: "",
				lines: &[("2026-03-01", "10:00:00"), ("2026-03-03", "10:00:00"), ("2026-03-05", "10:00:00")],
				ours: &[],
			},
		],
	);
	assert_eq!(order(&ranked), ["zealous", "seldom"]);
}

/// Outreach writes to everybody, so an answer is what sets a lead apart, however little else is known
/// about them and however much is known about whoever never answered.
#[test]
fn one_reply_outranks_an_owner_who_never_answered() {
	let ranked = check(
		"replied",
		&[
			Lead {
				name: "chatty",
				tags: "",
				lines: &[
					("2026-01-01", "10:00:00"),
					("2026-01-02", "10:00:00"),
					("2026-01-03", "10:00:00"),
					("2026-01-04", "10:00:00"),
					("2026-01-05", "10:00:00"),
					("2026-01-06", "10:00:00"),
					("2026-01-07", "10:00:00"),
					("2026-01-08", "10:00:00"),
					("2026-01-09", "10:00:00"),
					("2026-01-10", "10:00:00"),
				],
				ours: &[("2026-01-01", "09:00:00")],
			},
			Lead {
				name: "answered",
				tags: "",
				lines: &[("2026-03-01", "10:00:00")],
				ours: &[("2026-03-01", "09:00:00")],
			},
			Lead {
				name: "owner",
				tags: "business = true; birthday = { min = 2004; max = 2004; };",
				lines: &[],
				ours: &[("2026-09-01", "09:00:00")],
			},
		],
	);
	assert!(score(&ranked, "answered") > score(&ranked, "owner"), "{ranked:?}");
}

/// A line of ours says nothing about them: sent to everybody, it would make everybody equally recent.
#[test]
fn only_our_line_is_no_interaction() {
	let ranked = check(
		"ours",
		&[
			lead("untouched", ""),
			Lead {
				name: "written_to",
				tags: "",
				lines: &[],
				ours: &[("2026-09-01", "09:00:00")],
			},
		],
	);
	assert_eq!(score(&ranked, "written_to"), score(&ranked, "untouched"), "{ranked:?}");
}
