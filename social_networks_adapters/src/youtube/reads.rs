//! On-demand reads of youtube, through yt-dlp: it is the only thing that tracks youtube's player,
//! and the captions it hands back are already timed.

use std::path::{Path, PathBuf};

use color_eyre::eyre::{Context, Result, bail, ensure, eyre};
use jiff::civil::Date;
use tokio::process::Command;

const WATCH: &str = "https://www.youtube.com/watch?v=";
/// Youtube serves its chapter markers on about half the fetches of a watch page, so a chapterless
/// answer is asked again this many times over before it is believed.
const CHAPTER_FETCHES: usize = 6;

pub struct Listed {
	pub id: String,
	pub uploaded: Date,
	pub title: String,
}

pub struct Video {
	pub title: String,
	pub uploaded: Date,
	/// seconds
	pub duration: f64,
	pub channel: String,
	/// The uploader's, or the ones youtube generated; `None` for a video with neither, which is most of them.
	pub chapters: Option<Vec<Chapter>>,
	/// `None` where the uploader wrote nothing under it.
	pub description: Option<String>,
	/// `None` for a video youtube never captioned.
	pub captions: Option<Vec<Cue>>,
}

pub struct Chapter {
	pub at: f64,
	pub title: String,
}

/// One cue of the track. Auto-captions arrive as a rolling two-line window, a few words per cue,
/// re-sent as the window scrolls.
pub struct Cue {
	pub at: f64,
	pub text: String,
}

/// The flat listing is one request and carries no per-video metadata, which is why it is only ever
/// used for the ids.
pub async fn uploads(channel: &str) -> Result<Vec<String>> {
	let out = yt_dlp(&["--flat-playlist", "--print", "%(id)s", channel]).await?;
	Ok(out.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect())
}

pub async fn listing(ids: &[&str]) -> Result<Vec<Listed>> {
	if ids.is_empty() {
		return Ok(Vec::new());
	}
	let urls: Vec<String> = ids.iter().map(|id| format!("{WATCH}{id}")).collect();
	let mut args = vec!["--skip-download", "--print", "%(id)s\u{1f}%(upload_date)s\u{1f}%(title)s"];
	args.extend(urls.iter().map(String::as_str));
	let out = yt_dlp(&args).await?;
	let listed = out
		.lines()
		.map(|line| {
			let [id, uploaded, title]: [&str; 3] = line
				.split('\u{1f}')
				.collect::<Vec<_>>()
				.try_into()
				.map_err(|v| eyre!("yt-dlp was asked for three fields, and answered {v:?}"))?;
			Ok(Listed {
				id: id.to_string(),
				uploaded: date(uploaded)?,
				title: title.to_string(),
			})
		})
		.collect::<Result<Vec<_>>>()?;
	ensure!(listed.len() == ids.len(), "yt-dlp was asked about {ids:?}, and answered:\n{out}");
	Ok(listed)
}

pub async fn video(id: &str) -> Result<Video> {
	let tmp = std::env::temp_dir().join(format!("social_networks-yt-{id}"));
	// a caption file from an earlier run would be read as this one's
	if tmp.exists() {
		std::fs::remove_dir_all(&tmp).wrap_err_with(|| format!("clearing {}", tmp.display()))?;
	}
	std::fs::create_dir_all(&tmp).wrap_err_with(|| format!("creating {}", tmp.display()))?;
	let read = read(id, &tmp).await;
	std::fs::remove_dir_all(&tmp).wrap_err_with(|| format!("removing {}", tmp.display()))?;
	read
}

/// One capped pull. Youtube binds a media URL to the player client that asked for it, so anything
/// seeking into a video has to do it on a local file.
pub async fn download(id: &str, out: &Path) -> Result<()> {
	yt_dlp(&[
		// the default client hands back URLs that 403 on download, and the mobile ones are offered
		// nothing above 360p, at which a dashboard in a screen-share stops being readable
		"--extractor-args",
		"youtube:player_client=web_embedded",
		// the floor is what makes on-screen text legible and the ceiling is what keeps the pull cheap;
		// a video offering neither is refused rather than pulled illegible
		"-f",
		"bv*[height<=720][height>=480]",
		"--no-part",
		"-q",
		"-o",
		out.to_str().ok_or_else(|| eyre!("{} is not utf-8", out.display()))?,
		&format!("{WATCH}{id}"),
	])
	.await?;
	Ok(())
}
async fn read(id: &str, tmp: &Path) -> Result<Video> {
	let out = yt_dlp(&[
		"--skip-download",
		// `--print` alone implies `--simulate`, and a simulated run writes no caption file
		"--no-simulate",
		"--write-auto-subs",
		"--write-subs",
		"--sub-langs",
		"en.*",
		"--sub-format",
		"json3",
		"--print",
		"%(title)s\u{1f}%(upload_date)s\u{1f}%(duration)s\u{1f}%(channel)s\u{1f}%(chapters)j\u{1f}%(description)j",
		"-o",
		tmp.join("%(id)s").to_str().expect("the temp dir is utf-8"),
		&format!("{WATCH}{id}"),
	])
	.await?;
	let [title, uploaded, duration, channel, chapters, description]: [&str; 6] = out
		.trim()
		.split('\u{1f}')
		.collect::<Vec<_>>()
		.try_into()
		.map_err(|v| eyre!("yt-dlp was asked for six fields on {id}, and answered {v:?}"))?;
	let mut chapters = parse_chapters(chapters, id)?;
	for _ in 1..CHAPTER_FETCHES {
		if chapters.is_some() {
			break;
		}
		chapters = parse_chapters(yt_dlp(&["--skip-download", "--print", "%(chapters)j", &format!("{WATCH}{id}")]).await?.trim(), id)?;
	}
	// `NA` is yt-dlp's answer for a field youtube left empty
	let description = match description {
		"NA" => None,
		field => Some(serde_json::from_str::<String>(field).wrap_err_with(|| format!("yt-dlp states a description as a json string, and answered {field:?}"))?),
	}
	.filter(|d| !d.trim().is_empty());
	Ok(Video {
		title: title.to_string(),
		uploaded: date(uploaded)?,
		duration: duration.parse().wrap_err_with(|| format!("yt-dlp stated {id}'s duration as {duration:?}"))?,
		channel: channel.to_string(),
		chapters,
		description,
		captions: captions(tmp)?,
	})
}

fn parse_chapters(field: &str, id: &str) -> Result<Option<Vec<Chapter>>> {
	if field == "NA" {
		return Ok(None);
	}
	let parsed: Vec<serde_json::Value> = serde_json::from_str(field).wrap_err_with(|| format!("yt-dlp states {id}'s chapters as a json list, and answered {field:?}"))?;
	ensure!(!parsed.is_empty(), "{id} carries a chapter list with nothing in it");
	parsed
		.iter()
		.map(|c| {
			Ok(Chapter {
				at: c["start_time"].as_f64().ok_or_else(|| eyre!("an unstamped youtube chapter on {id}: {c}"))?,
				title: c["title"].as_str().ok_or_else(|| eyre!("an untitled youtube chapter on {id}: {c}"))?.to_string(),
			})
		})
		.collect::<Result<_>>()
		.map(Some)
}

/// yt-dlp names the track by the language it found, and asks for both the uploader's and youtube's
/// own. A hand-written track is the better read, and sorting puts its shorter name first.
fn captions(tmp: &Path) -> Result<Option<Vec<Cue>>> {
	let mut tracks: Vec<PathBuf> = std::fs::read_dir(tmp)?
		.map(|e| Ok(e?.path()))
		.collect::<std::io::Result<Vec<_>>>()?
		.into_iter()
		.filter(|p| p.extension().is_some_and(|e| e == "json3"))
		.collect();
	tracks.sort();
	let Some(track) = tracks.first() else { return Ok(None) };
	let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(track)?).wrap_err_with(|| format!("{} is not json3", track.display()))?;
	let events = json["events"].as_array().ok_or_else(|| eyre!("{} carries no events", track.display()))?;
	let mut cues = Vec::new();
	for event in events {
		let Some(segs) = event["segs"].as_array() else { continue };
		let text: String = segs.iter().filter_map(|s| s["utf8"].as_str()).collect();
		// the rolling window re-sends the newline between its two lines as a cue of its own
		if text.trim().is_empty() {
			continue;
		}
		let at = event["tStartMs"].as_f64().ok_or_else(|| eyre!("an unstamped json3 event: {event}"))? / 1000.;
		cues.push(Cue { at, text: text.trim().to_string() });
	}
	Ok(Some(cues))
}

fn date(yyyymmdd: &str) -> Result<Date> {
	Date::strptime("%Y%m%d", yyyymmdd).wrap_err_with(|| format!("yt-dlp states an upload date as YYYYMMDD, and answered {yyyymmdd:?}"))
}

async fn yt_dlp(args: &[&str]) -> Result<String> {
	let out = Command::new("yt-dlp").arg("--no-update").args(args).output().await.wrap_err("yt-dlp — is it on PATH?")?;
	if !out.status.success() {
		bail!("yt-dlp {args:?} failed:\n{}", String::from_utf8_lossy(&out.stderr));
	}
	String::from_utf8(out.stdout).wrap_err("yt-dlp prints utf-8")
}
