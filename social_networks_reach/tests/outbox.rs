use std::path::PathBuf;

use social_networks_reach::outbox;

const NOW: &str = "2026-10-01T12:00:00Z";

/// `files` is `<messenger>/<name>` → body; returns `<messenger>/<name>` of what is due, or the error.
fn check(case: &str, files: &[(&str, &str)]) -> Result<Option<String>, String> {
	let person = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("outbox_{}_{case}", std::process::id()));
	for (path, body) in files {
		let path = person.join("outbox").join(path);
		std::fs::create_dir_all(path.parent().unwrap()).unwrap();
		std::fs::write(path, body).unwrap();
	}
	let due = outbox::due(&person, NOW.parse().unwrap());
	if person.exists() {
		std::fs::remove_dir_all(&person).unwrap();
	}
	due.map(|d| d.map(|d| format!("{}/{}", d.messenger, d.path.file_name().unwrap().to_string_lossy())))
		.map_err(|e| e.to_string())
}

#[test]
fn nothing_scheduled() {
	assert_eq!(check("none", &[]), Ok(None));
}

#[test]
fn a_future_message_is_not_due() {
	assert_eq!(check("future", &[("telegram/2026-10-01T12:01Z.md", "hi\n")]), Ok(None));
}

#[test]
fn the_earliest_due_across_messengers() {
	let files = [
		("telegram/2026-09-30T10:00Z.md", "later\n"),
		("discord/2026-09-29T10:00Z.md", "earliest\n"),
		("discord/2026-10-01T12:00Z.md", "due right now\n"),
		("skool/2026-10-02T00:00Z.md", "not yet\n"),
	];
	assert_eq!(check("earliest", &files), Ok(Some("discord/2026-09-29T10:00Z.md".into())));
}

#[test]
fn the_body_is_verbatim_but_its_trailing_newline() {
	let person = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("outbox_{}_body", std::process::id()));
	std::fs::create_dir_all(person.join("outbox/telegram")).unwrap();
	std::fs::write(person.join("outbox/telegram/2026-09-30T10:00Z.md"), "  hi,\n\nthere\n\n").unwrap();
	let due = outbox::due(&person, NOW.parse().unwrap()).unwrap().unwrap();
	std::fs::remove_dir_all(&person).unwrap();
	assert_eq!(due.text, "  hi,\n\nthere\n");
}

#[test]
fn a_bad_file_is_an_error_even_when_not_due() {
	for (case, name) in [
		("seconds", "2026-09-30T10:00:00Z.md"),
		("no_z", "2026-09-30T10:00.md"),
		("txt", "2026-09-30T10:00Z.txt"),
		("garbage", "soon.md"),
	] {
		let got = check(case, &[("telegram/2026-09-29T10:00Z.md", "hi\n"), (&format!("telegram/{name}"), "hi\n")]);
		assert!(got.is_err(), "{name}: {got:?}");
	}
	assert!(check("empty", &[("telegram/2026-09-29T10:00Z.md", "\n")]).is_err());
	assert!(check("future_empty", &[("telegram/2027-01-01T00:00Z.md", " \n")]).is_err());
}
