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

// Synthetic: no capture of a live thread header exists, so the wording is facebook's as remembered.
mod presence {
	use jiff::{SignedDuration, Timestamp};
	use social_networks_adapters::facebook::presence;

	const NOW: Timestamp = Timestamp::constant(1_792_000_000, 0);

	fn active(head: &[&str]) -> color_eyre::eyre::Result<Option<Timestamp>> {
		presence::active(&head.iter().map(|l| l.to_string()).collect::<Vec<_>>(), NOW)
	}

	#[test]
	fn active_now_is_the_read_time_and_a_stated_age_is_a_lower_bound_on_it() {
		assert_eq!(active(&["Person Four", "Active now"]).unwrap(), Some(NOW));
		let ago = |head: &str, secs: i64| assert_eq!(active(&[head]).unwrap(), Some(NOW - SignedDuration::from_secs(secs)), "{head}");
		// facebook rounds the age to one unit, so the person was active at most a unit earlier than stated
		ago("Active 5m ago", 6 * 60);
		ago("Active 3h ago", 4 * 3600);
		ago("Active 2d ago", 3 * 86400);
		let older = active(&["Active 59m ago"]).unwrap().unwrap();
		assert!(older <= NOW - SignedDuration::from_secs(59 * 60));
	}

	#[test]
	fn a_header_with_no_presence_states_nothing() {
		assert_eq!(active(&["Person Four", "Messenger"]).unwrap(), None);
		assert_eq!(active(&[]).unwrap(), None);
	}

	#[test]
	fn a_presence_line_that_does_not_parse_is_an_error_not_silence() {
		assert!(active(&["Active a while back"]).is_err());
	}

	#[test]
	fn the_newest_of_several_presence_lines_wins() {
		assert_eq!(active(&["Active 3h ago", "Active now"]).unwrap(), Some(NOW));
	}

	#[test]
	fn a_receipt_under_the_last_line_means_our_message_was_seen() {
		for line in ["Seen", "Seen by Person Four", "Seen just now", "Seen 5m ago"] {
			assert!(presence::seen(line), "{line}");
		}
		for line in ["Sent", "Delivered", "Seen it, thanks", "See you", ""] {
			assert!(!presence::seen(line), "{line}");
		}
	}
}

/// A profile page whose posts are `edges`, as facebook embeds them: `data.user.timeline_list_feed_units`.
/// Only `profile_timeline.json` is captured; these keep its paths and vary the nodes.
fn timeline(user: &str, edges: &[(&str, Option<i64>)]) -> String {
	let edges: Vec<_> = edges
		.iter()
		.map(|(actor, at)| {
			let mut node = serde_json::json!({ "__typename": "Story", "actors": [{ "__typename": "User", "id": actor }] });
			if let Some(at) = at {
				node["creation_time"] = (*at).into();
			}
			serde_json::json!({ "node": node })
		})
		.collect();
	serde_json::json!({ "data": { "user": { "id": user, "timeline_list_feed_units": { "edges": edges } } } }).to_string()
}

#[track_caller]
fn check_posted(scripts: &str, expected: Option<i64>) {
	let posted = Section::parse(scripts).unwrap().posted.map(|t| t.as_second());
	assert_eq!(posted, expected);
}

#[test]
fn newest_post_is_dated_by_the_story_alone_not_by_what_hangs_off_it() {
	// the capture's story shares an older one and holds a later comment by somebody else
	check_posted(&fixture("profile_timeline.json"), Some(1791890619));
}

#[test]
fn newest_post_is_the_max_over_the_page_wherever_the_pinned_one_sits() {
	check_posted(&timeline("1", &[("1", Some(1000)), ("1", Some(3000)), ("1", Some(2000))]), Some(3000));
}

#[test]
fn what_others_posted_to_the_timeline_is_not_their_activity() {
	check_posted(&timeline("1", &[("2", Some(9000)), ("1", Some(1000))]), Some(1000));
	check_posted(&timeline("1", &[("2", Some(9000))]), None);
}

#[test]
fn a_page_with_nothing_dated_gives_no_evidence() {
	check_posted(&timeline("1", &[]), None);
	check_posted(&fixture("personal_details.ndjson"), None);
}

#[test]
fn a_story_of_theirs_without_a_date_is_a_shape_we_do_not_know() {
	assert!(Section::parse(&timeline("1", &[("1", None)])).is_err());
}
