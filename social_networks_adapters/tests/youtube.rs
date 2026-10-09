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
