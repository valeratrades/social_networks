//! A fact is a tag platforms state: a purpose opts into it by name, and the store a roster walk checks
//! into keeps everyone it ever listed and where the walk stopped.

use social_networks_adapters::reach::{Member, Roster, VenueRef, VenueSource};
use social_networks_reach::{purpose::Purposes, venue::Store};

fn purpose(lives_in: serde_json::Value) -> Result<Purposes, String> {
	serde_json::from_value(serde_json::json!({ "reviews": {
		"path": "/nonexistent",
		"tags": { "lives_in": lives_in },
		"rank": [{ "of": "interactions", "weight": 1 }],
	}}))
	.map_err(|e| e.to_string())
}

#[test]
fn a_fact_is_declared_with_its_own_type() {
	purpose(serde_json::json!({ "type": "place" })).unwrap();
	let e = purpose(serde_json::json!({ "type": "bool" })).unwrap_err();
	assert!(e.contains("lives_in") && e.contains("place"), "{e}");
}

fn row(handle: &str, bio: &str) -> Member {
	Member {
		handle: handle.to_string(),
		display: handle.to_string(),
		joined: None,
		lat: Some(45.76),
		lon: Some(4.83),
		zone: None,
		place: Some("Lyon, France".to_string()),
		bio: Some(bio.to_string()),
	}
}

#[test]
fn a_roster_is_upserted_and_resumes_where_it_stopped() {
	let dir = std::env::temp_dir().join(format!("social_networks_facts_{}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	let at = VenueRef::new(VenueSource::Facebook, "city/1");

	let mut store = Store::open(&dir, &at).unwrap();
	assert_eq!(store.check_in(&[row("1", "a"), row("2", "a")], None).unwrap(), 2);
	assert_eq!(store.check_in(&[], Some("jean".to_string())).unwrap(), 0);

	let mut store = Store::open(&dir, &at).unwrap();
	assert_eq!(store.cursor(), Some("jean"));
	assert_eq!(store.check_in(&[row("2", "b"), row("3", "b")], Some("marie".to_string())).unwrap(), 1);
	let roster = store.roster().unwrap();
	assert_eq!(
		roster.iter().map(|m| (m.handle.as_str(), m.bio.as_deref().unwrap())).collect::<Vec<_>>(),
		[("1", "a"), ("2", "b"), ("3", "b")]
	);
	assert_eq!(social_networks_reach::venue::all(&dir).unwrap(), vec![at]);
	std::fs::remove_dir_all(&dir).unwrap();
}
