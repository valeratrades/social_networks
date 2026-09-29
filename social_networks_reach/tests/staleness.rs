//! What a pull stands to move a score by, over the reviews purpose as `examples/purposes/reviews.nix`
//! writes it. Each cohort is person files and `meta.json`s on disk, ranked the way a command does.
//!
//! Needs `nix` to evaluate the purpose and the person files, which the dev shell provides.

use std::path::{Path, PathBuf};

use jiff::{SignedDuration, Timestamp};
use social_networks_reach::{person, purpose::Purposes, rank};

const DAY: SignedDuration = SignedDuration::from_hours(24);
const OWNER: &str = r#"business = true; lives_in = { name = "Paris"; lat = 48.8566; lon = 2.3522; };"#;
struct Lead {
	name: &'static str,
	tags: &'static str,
	/// How long ago the last complete, reasoned-over pull was. `None` is never.
	synced: Option<SignedDuration>,
}

/// `stale` per name, over the example purpose with `edit` applied to its raw config.
fn check(cohort: &str, edit: impl Fn(&mut serde_json::Value), leads: &[Lead]) -> Vec<(String, f64)> {
	let dir = std::env::temp_dir().join(format!("social_networks_staleness_{}_{cohort}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	let people = dir.join("people");
	for lead in leads {
		let person = people.join(lead.name);
		std::fs::create_dir_all(&person).unwrap();
		std::fs::write(person.join("__main__.nix"), format!("{{ tags = {{ {} }}; }}\n", lead.tags)).unwrap();
		if let Some(ago) = lead.synced {
			let at = Timestamp::now() - ago;
			std::fs::write(person.join("meta.json"), serde_json::json!({ "sources": {}, "fetched_at": at, "reasoned_at": at }).to_string()).unwrap();
		}
	}

	let purpose = reviews(&people, edit);
	let loaded = person::load_dir(&purpose).unwrap().into_values().collect();
	let ranked = rank::rank(&purpose, &dir.join("venues"), loaded).unwrap();
	std::fs::remove_dir_all(&dir).unwrap();
	for r in &ranked {
		assert!((0.0..=1.0).contains(&r.stale), "{}: {}", r.person.name, r.stale);
	}
	ranked.into_iter().map(|r| (r.person.name, r.stale)).collect()
}

fn reviews(path: &Path, edit: impl Fn(&mut serde_json::Value)) -> social_networks_reach::purpose::Purpose {
	let file = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../examples/purposes/reviews.nix");
	let out = std::process::Command::new("nix")
		.args(["eval", "--impure", "--json", "--file"])
		.arg(&file)
		.output()
		.expect("`nix` on PATH; the dev shell provides it");
	assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
	let mut raw: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
	raw["path"] = serde_json::json!(path);
	edit(&mut raw);
	let purposes: Purposes = serde_json::from_value(serde_json::json!({ "reviews": raw })).unwrap();
	purposes.get("reviews").unwrap().clone()
}

fn stale(ranked: &[(String, f64)], name: &str) -> f64 {
	ranked.iter().find(|(n, _)| n == name).unwrap_or_else(|| panic!("{name} was ranked")).1
}

fn as_is(_: &mut serde_json::Value) {}

#[test]
fn the_longer_since_a_pull_the_more_it_stands_to_move() {
	let ranked = check(
		"since",
		as_is,
		&[
			Lead {
				name: "never",
				tags: OWNER,
				synced: None,
			},
			Lead {
				name: "yesterday",
				tags: OWNER,
				synced: Some(DAY),
			},
			Lead {
				name: "today",
				tags: OWNER,
				synced: Some(SignedDuration::ZERO),
			},
		],
	);
	assert!(stale(&ranked, "never") > stale(&ranked, "yesterday"));
	assert!(stale(&ranked, "yesterday") > stale(&ranked, "today"));
	assert!(stale(&ranked, "today") < 1e-6, "{}", stale(&ranked, "today"));
}

#[test]
fn a_longer_half_life_forgives_the_same_wait() {
	let cohort = |half_life: &'static str| {
		let ranked = check(
			&format!("half_life_{half_life}"),
			|raw| raw["half_life"] = serde_json::json!(half_life),
			&[
				Lead {
					name: "month",
					tags: OWNER,
					synced: Some(DAY * 30),
				},
				Lead {
					name: "never",
					tags: "",
					synced: None,
				},
			],
		);
		stale(&ranked, "month")
	};
	assert!(cohort("120d") < cohort("60d"));
}

/// `last_login` is a timestamp a human writes, so no pull brings it closer to the truth.
#[test]
fn a_term_no_pull_refreshes_costs_nothing() {
	let ranked = check(
		"unrefreshed",
		|raw| raw["rank"] = serde_json::json!([{ "of": "last_login", "decay": 3, "weight": 1 }]),
		&[
			Lead {
				name: "never",
				tags: r#"last_login = "2026-01-01T00:00:00Z";"#,
				synced: None,
			},
			Lead {
				name: "blank",
				tags: "",
				synced: None,
			},
		],
	);
	assert_eq!(stale(&ranked, "never"), 0.0);
	assert_eq!(stale(&ranked, "blank"), 0.0);
}

/// With the rest of the score out of a pull's reach, the loss is the refreshable term's share of it.
#[test]
fn the_loss_scales_with_the_weight_share() {
	let with = |last_login: u32| {
		let ranked = check(
			&format!("share_{last_login}"),
			|raw| raw["rank"] = serde_json::json!([{ "of": "business", "weight": 1 }, { "of": "last_login", "decay": 3, "weight": last_login }]),
			&[
				Lead {
					name: "owner",
					tags: "business = true;",
					synced: None,
				},
				Lead {
					name: "other",
					tags: "business = false;",
					synced: Some(DAY * 10),
				},
			],
		);
		stale(&ranked, "owner")
	};
	let (half, quarter) = (with(1), with(3));
	assert!((half - 2.0 * quarter).abs() < 1e-12, "{half} against {quarter}");
}

/// Nobody unsynced is certain to move, but somebody seeded where the synced cohort turned out not to
/// be stands to move further than somebody seeded where it is.
#[test]
fn a_seed_far_from_the_synced_cohort_stands_to_move_more() {
	const LYON: &str = r#"lives_in = { name = "Lyon"; lat = 45.764; lon = 4.8357; };"#;
	let synced = |name| Lead {
		name,
		tags: OWNER,
		synced: Some(DAY),
	};
	let ranked = check(
		"seed",
		as_is,
		&[
			synced("paris_1"),
			synced("paris_2"),
			synced("paris_3"),
			Lead {
				name: "seeded_far",
				tags: LYON,
				synced: None,
			},
			Lead {
				name: "seeded_near",
				tags: OWNER,
				synced: None,
			},
		],
	);
	assert!(stale(&ranked, "seeded_far") > stale(&ranked, "seeded_near"));
}
