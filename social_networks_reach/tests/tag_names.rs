//! A tag is one name however it is spelled: `ServiceArb`, `service-arb` and `service_arb` are the same
//! tag in the vocabulary, in a person file, in a rank term and in a pattern, and are shown snake_case.
//! Two spellings of one name side by side are a collision, refused at load.
//!
//! Needs `nix` to evaluate the person files, which the dev shell provides.

use std::path::Path;

use social_networks_reach::{
	person,
	purpose::{Purpose, Purposes},
};

fn purpose(path: &Path, tags: serde_json::Value) -> Result<Purpose, String> {
	let raw = serde_json::json!({ "p": { "path": path, "tags": tags, "rank": [{ "of": "Service-Arb", "weight": 1 }], "half_life": "30d" } });
	serde_json::from_value::<Purposes>(raw).map(|p| p.get("p").unwrap().clone()).map_err(|e| e.to_string())
}

/// Person files `(name, body of tags = { … })` loaded against `purpose`, and who `pattern` selects.
fn check(cohort: &str, people: &[(&str, &str)], pattern: &str) -> Result<Vec<String>, String> {
	let dir = std::env::temp_dir().join(format!("social_networks_tag_names_{}_{cohort}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	for (name, tags) in people {
		std::fs::create_dir_all(dir.join(name)).unwrap();
		std::fs::write(dir.join(name).join("__main__.nix"), format!("{{ tags = {{ {tags} }}; }}\n")).unwrap();
	}
	let purpose = purpose(&dir, serde_json::json!({ "ServiceArb": { "type": "bool" }, "HomeCity": ["lyon"] })).unwrap();
	let loaded = person::load_dir(&purpose).map_err(|e| format!("{e:#}"));
	std::fs::remove_dir_all(&dir).unwrap();
	Ok(loaded?.values().filter(|p| p.matches(pattern)).map(|p| p.name.clone()).collect())
}

#[test]
fn every_spelling_is_one_tag() {
	let people = [("a", "ServiceArb = true;"), ("b", "service-arb = true;"), ("c", r#"home_city = "lyon";"#), ("d", "")];
	assert_eq!(check("bool", &people, "service_arb").unwrap(), ["a", "b"]);
	assert_eq!(check("group", &people, "homeCity:lyon").unwrap(), ["c"]);

	let purpose = purpose(Path::new("/nonexistent"), serde_json::json!({ "ServiceArb": { "type": "bool" } })).unwrap();
	assert_eq!(purpose.tags.keys().collect::<Vec<_>>(), ["service_arb"]);
	assert_eq!(purpose.rank[0].of, "service_arb");
}

#[test]
fn two_spellings_of_one_name_collide() {
	assert!(
		purpose(
			Path::new("/nonexistent"),
			serde_json::json!({ "ServiceArb": { "type": "bool" }, "service_arb": { "type": "bool" } })
		)
		.is_err()
	);
	let e = check("collide", &[("a", "ServiceArb = true; service-arb = false;")], "a").unwrap_err();
	assert!(e.contains("service_arb"), "{e}");
}
