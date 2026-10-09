//! A video's captions, as youtube serves them today. Only a live read covers which tracks it offers
//! and which of them it refuses.
#![cfg(feature = "youtube-reads")]

use social_networks_adapters::youtube;

#[tokio::test]
#[ignore = "reads youtube live"]
async fn an_auto_captioned_video_reads_its_own_track() {
	let video = youtube::video("Qt_i0SHQuDc").await.unwrap();
	let cues = video.captions.expect("youtube captioned it");
	assert!(
		cues.iter().any(|c| c.text.to_lowercase().contains("contractor")),
		"the title's subject is never said: {:?}",
		&cues[..cues.len().min(20)]
	);
}

#[tokio::test]
async fn a_hold_sends_nothing_until_it_lifts() {
	let state = std::env::temp_dir().join(format!("youtube-hold-test-{}", std::process::id()));
	std::fs::create_dir_all(state.join("social_networks")).unwrap();
	let until = jiff::Timestamp::now() + jiff::SignedDuration::from_hours(1);
	std::fs::write(state.join("social_networks/youtube_hold"), until.to_string()).unwrap();
	// SAFETY: the only test in this binary that reads the environment without being ignored
	unsafe { std::env::set_var("XDG_STATE_HOME", &state) };
	let started = std::time::Instant::now();
	let err = youtube::video("Qt_i0SHQuDc").await.err().expect("a hold refuses the read");
	assert!(format!("{err:?}").contains(&until.to_string()), "{err:?}");
	assert!(started.elapsed() < std::time::Duration::from_secs(1), "a hold answers before yt-dlp is started");
	std::fs::remove_dir_all(&state).unwrap();
}
