//! A group is a tag with one value per person out of a declared list, and what a procurement strategy
//! can be generic over. What is asserted: a purpose naming what it does not declare fails at load, a
//! binding outside the group is refused, and a `<group>:<value>` pattern is a cohort.
//!
//! Needs `nix` to evaluate the person files, which the dev shell provides.

use std::{collections::BTreeMap, path::Path};

use social_networks_reach::{
	person::{self, Value},
	purpose::{Purpose, Purposes},
};

fn purpose(path: &Path, procure: serde_json::Value) -> Result<Purpose, String> {
	let raw = serde_json::json!({ "p": {
		"path": path,
		"tags": {
			"location": ["lyon", "paris"],
			"home": ["lyon"],
			"ServiceArb": { "type": "bool" },
		},
		"procure": procure,
		"rank": [{ "of": "interactions", "weight": 1 }],
		"half_life": "30d",
	}});
	serde_json::from_value::<Purposes>(raw).map(|p| p.get("p").unwrap().clone()).map_err(|e| e.to_string())
}

fn generic(path: &Path) -> Purpose {
	purpose(
		path,
		serde_json::json!({ "servicing": { "venue": "skool:x", "where": "zone LIKE '%$location%'", "tags": { "location": "$location", "ServiceArb": true } } }),
	)
	.unwrap()
}

fn bindings(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
	pairs.iter().map(|(g, v)| (g.to_string(), v.to_string())).collect()
}

#[test]
fn a_placeholder_must_name_a_group_that_fits() {
	let dir = Path::new("/nonexistent");
	let refused = |procure: serde_json::Value, named: &str| {
		let e = purpose(dir, procure).unwrap_err();
		assert!(e.contains(named), "`{e}` does not name `{named}`");
	};
	refused(serde_json::json!({ "s": { "venue": "skool:x", "where": "zone LIKE '%$city%'" } }), "$city");
	refused(serde_json::json!({ "s": { "venue": "skool:x", "where": "$ServiceArb" } }), "$ServiceArb");
	refused(serde_json::json!({ "s": { "venue": "skool:x", "tags": { "location": "$city" } } }), "$city");
	// `home` holds only lyon, and `$location` may be bound to paris
	refused(serde_json::json!({ "s": { "venue": "skool:x", "tags": { "home": "$location" } } }), "paris");
	refused(serde_json::json!({ "s": { "venue": "skool:x", "tags": { "location": "x-$location" } } }), "location");
}

#[test]
fn a_strategy_binds_exactly_its_groups_to_their_values() {
	let purpose = generic(Path::new("/nonexistent"));
	let servicing = &purpose.procure["servicing"];
	assert_eq!(servicing.generic_over().collect::<Vec<_>>(), ["location"]);

	let bound = servicing.bind(&bindings(&[("location", "lyon")])).unwrap();
	assert_eq!(bound.predicate.as_deref(), Some("zone LIKE '%lyon%'"));
	assert_eq!(bound.tags["location"], Value::Text("lyon".to_string()));

	for refused in [&[][..], &[("location", "berlin")], &[("location", "lyon"), ("home", "lyon")]] {
		assert!(servicing.bind(&bindings(refused)).is_err(), "{refused:?} was bound");
	}
}

#[test]
fn a_group_pattern_is_a_cohort() {
	let dir = std::env::temp_dir().join(format!("social_networks_groups_{}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	for (name, tags) in [("lyonnais", r#"location = "lyon";"#), ("parisien", r#"location = "paris";"#), ("lyon-fan", "ServiceArb = true;")] {
		std::fs::create_dir_all(dir.join(name)).unwrap();
		std::fs::write(dir.join(name).join("__main__.nix"), format!("{{ tags = {{ {tags} }}; }}\n")).unwrap();
	}
	let people = person::load_dir(&generic(&dir)).unwrap();
	std::fs::remove_dir_all(&dir).unwrap();
	let cohort: Vec<&str> = people.values().filter(|p| p.matches("location:lyon")).map(|p| p.name.as_str()).collect();
	assert_eq!(cohort, ["lyonnais"]);
}
