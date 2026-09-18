//! `SKOOL_EMAIL=… SKOOL_PASSWORD=… cargo r -p social_networks_adapters --example skool_classroom -- <group>`
//!
//! Three questions `Skool::classroom` cannot be written correctly without: whether the classroom
//! index carries the whole tree or only its top level, what addresses a lesson, and where a lesson's
//! body and its video actually live. Dumps the index's `pageProps` key shapes, one course whole, and
//! what a lesson's own `?md=` route adds over it — then reads the classroom and prints what came out.

use color_eyre::eyre::{Result, eyre};
use social_networks_adapters::{
	reach::{VenueRef, VenueSource},
	skool::{Skool, SkoolCredentials},
};

#[tokio::main]
async fn main() -> Result<()> {
	tracing_subscriber::fmt().with_env_filter("info").init();
	let group = std::env::args().nth(1).ok_or_else(|| eyre!("usage: skool_classroom <group>"))?;
	let creds = SkoolCredentials {
		email: std::env::var("SKOOL_EMAIL")?,
		password: std::env::var("SKOOL_PASSWORD")?,
	};
	let mut session = Skool::try_new(Some(creds))?;

	let payload = session.page(&format!("/{group}/classroom")).await?;
	println!("route: {}", payload.get("page").unwrap_or(&serde_json::Value::Null));
	let props = payload.pointer("/props/pageProps").ok_or_else(|| eyre!("no pageProps"))?;
	shape("pageProps", props, 0);

	let courses = props.get("allCourses").and_then(|v| v.as_array()).ok_or_else(|| eyre!("no allCourses"))?.clone();
	println!("\n=== the index's first course, whole ===\n{:#}", courses.first().ok_or_else(|| eyre!("an empty classroom"))?);

	let addr = courses[0].get("name").and_then(|v| v.as_str()).ok_or_else(|| eyre!("a course without a name"))?.to_string();
	let opened = session.page(&format!("/{group}/classroom/{addr}")).await?;
	println!("\n=== /classroom/<name> route: {} ===", opened.get("page").unwrap_or(&serde_json::Value::Null));
	let props = opened.pointer("/props/pageProps").ok_or_else(|| eyre!("no pageProps on the course route"))?;
	shape("pageProps", props, 0);
	println!("\n=== `course` with no `md`, whole ===\n{:#}", props.get("course").unwrap_or(&serde_json::Value::Null));

	// whether `video` follows the selected module, or is the course's first one whatever is asked for
	let lesson = props
		.pointer("/course/children/1/course/id")
		.and_then(|v| v.as_str())
		.ok_or_else(|| eyre!("a course with fewer than two lessons"))?
		.to_string();
	for md in ["", &lesson] {
		let query = match md.is_empty() {
			true => String::new(),
			false => format!("?md={md}"),
		};
		let opened = session.page(&format!("/{group}/classroom/{addr}{query}")).await?;
		let props = opened.pointer("/props/pageProps").ok_or_else(|| eyre!("no pageProps on the lesson route"))?;
		println!("\n=== /classroom/{addr}{query} ===");
		println!("selectedModule: {}", props.get("selectedModule").unwrap_or(&serde_json::Value::Null));
		println!("video: {:#}", props.get("video").unwrap_or(&serde_json::Value::Null));
	}

	println!("\n=== what the read makes of it ===");
	for lesson in session.classroom(&VenueRef::new(VenueSource::Skool, group)).await? {
		println!("\n{} / {}\n  {}\n  {} {:?}", lesson.module, lesson.title, lesson.permalink, lesson.at, lesson.video);
		if !lesson.body.is_empty() {
			println!("  {}", lesson.body.replace('\n', "\n  "));
		}
	}
	Ok(())
}

/// Key shapes rather than values: a classroom payload is large and it is the *shape* that decides
/// where the read points.
fn shape(name: &str, value: &serde_json::Value, depth: usize) {
	let pad = "  ".repeat(depth);
	match value {
		// skool ships the whole UI dictionary on every page, and none of it is content
		_ if name == "translation" => println!("{pad}{name}: <the i18n dictionary>"),
		serde_json::Value::Object(map) if depth < 3 => {
			println!("{pad}{name}: object({})", map.len());
			for (key, child) in map {
				shape(key, child, depth + 1);
			}
		}
		serde_json::Value::Object(map) => println!("{pad}{name}: object{:?}", map.keys().collect::<Vec<_>>()),
		serde_json::Value::Array(list) => {
			println!("{pad}{name}: array({})", list.len());
			if let Some(item) = list.first() {
				shape("[0]", item, depth + 1);
			}
		}
		serde_json::Value::String(s) => println!("{pad}{name}: str({})", s.chars().take(60).collect::<String>()),
		other => println!("{pad}{name}: {other}"),
	}
}
