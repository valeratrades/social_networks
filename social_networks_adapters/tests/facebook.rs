//! What facebook's embedded JSON and GraphQL answers read as, over pages captured from the live site.

use social_networks_adapters::facebook::{
	lead_rate::{LeadRate, PERIOD_MIN},
	members::Listing,
	profile::{self, Section},
	search::Results,
};

fn fixture(name: &str) -> String {
	std::fs::read_to_string(format!("{}/tests/fixtures/facebook/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

#[test]
fn member_list_is_the_group_s_user_edges() {
	let mut listing = Listing::default();
	listing.absorb(&fixture("members_page.ndjson")).unwrap();
	let (group, members) = listing.finish().unwrap();
	assert_eq!((group.id.as_str(), group.name.as_str()), ("671440486248842", "Colocation Lyon | La Carte des Colocs"));
	assert_eq!((members.len(), members.iter().filter(|m| m.bio.is_some()).count()), (15, 14));
	assert!(members.iter().all(|m| m.joined.is_none()), "a relative join text is no date");
	let m = members.iter().find(|m| m.bio.as_deref().is_some_and(|b| b.ends_with("Joined 59 minutes ago"))).unwrap();
	assert!(m.handle.bytes().all(|b| b.is_ascii_digit()), "{m:?}");
}

#[test]
fn people_search_lists_users_under_the_city_filter() {
	let mut results = Results::default();
	results.absorb(&fixture("people_search.ndjson")).unwrap();
	assert_eq!(results.city, Some(("108560402508141".into(), "Lyon, France".into())));
	assert_eq!((results.hits.len(), results.more), (10, Some(true)));
	let hit = &results.hits["100000000000004"];
	assert_eq!(hit.name, "Person Four");
	assert_eq!(hit.snippets, ["Works at La Croix Rousse, Rhone-Alpes, France · Lives in Lyon, France"]);
	results.absorb(&fixture("people_search_page.json")).unwrap();
	assert_eq!((results.hits.len(), results.more), (21, Some(true)));
	assert_eq!(results.hits["100000000000100"].snippets, Vec::<String>::new());
}

#[test]
fn personal_details_keep_current_city_and_hometown_apart() {
	let personal = Section::parse(&fixture("personal_details.ndjson")).unwrap();
	let empty = Section::default();
	let stated = profile::stated(&personal, &empty, &empty, &empty);
	assert_eq!(stated.sources["facebook:lives_in"], "Kasserine");
	assert_eq!(stated.sources["facebook:hometown"], "Porto, Portugal");
	assert_eq!(stated.sources["facebook:birthday"], "September 25, 2002");
	assert_eq!(stated.born, Some(jiff::civil::date(2002, 9, 25)));
	assert_eq!(
		personal.present,
		["directory_personal_details", "directory_education", "directory_contact_info", "directory_names"]
	);
}

#[test]
fn contact_links_are_unwrapped_from_facebook_s_redirect() {
	let empty = Section::default();
	let stated = profile::stated(&empty, &empty, &empty, &Section::parse(&fixture("contact_info.ndjson")).unwrap());
	assert_eq!(stated.handles.into_iter().collect::<Vec<_>>(), [("instagram".to_string(), "member.zero".to_string())]);
}

#[test]
fn lead_rate_is_a_plain_mean_for_one_period_then_wilder() {
	let min = |m: f64| jiff::SignedDuration::from_secs_f64(m * 60.);
	let mut r = LeadRate::default();
	r.record(min(20.), 3);
	r.record(min(10.), 0);
	assert_eq!((r.per_minute, r.over), (0.1, min(30.)));
	r.record(min(PERIOD_MIN), 0); // closes the first period at 3 / 60, then decays for the 30 min past it
	let decay = |m: f64| (1. - 1. / PERIOD_MIN).powf(m);
	assert!((r.per_minute - 3. / PERIOD_MIN * decay(30.)).abs() < 1e-12, "{r}");
	let before = r.per_minute;
	r.record(min(1.), 2);
	assert!((r.per_minute - (before * decay(1.) + 2. / PERIOD_MIN)).abs() < 1e-12, "{r}");
	assert_eq!((r.leads, r.over), (5, min(30. + PERIOD_MIN + 1.)));
}
