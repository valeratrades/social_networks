//! Which encoder the archive keeps its images with. Point it at a directory of real DM images:
//! `cargo r --release -p social_networks --example avif_bench -- <dir>`.
//!
//! Decode time is shared by both paths and left out; what is compared is encode wall time and the
//! bytes that land on disk at a visually comparable quality.

use std::{path::PathBuf, time::Instant};

fn main() {
	if cfg!(debug_assertions) {
		println!("!! build with --release: an unoptimised rav1e is ~50x slower than the shipped one, and compares nothing\n");
	}
	let dir = PathBuf::from(std::env::args().nth(1).expect("usage: avif_bench <dir of images>"));
	let mut inputs: Vec<PathBuf> = std::fs::read_dir(&dir)
		.expect("readable directory")
		.map(|e| e.expect("readable entry").path())
		.filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("jpg" | "jpeg" | "png" | "webp")))
		.collect();
	inputs.sort();
	inputs.truncate(8);
	assert!(!inputs.is_empty(), "no images in {}", dir.display());

	let out = std::env::temp_dir().join("avif_bench");
	let _ = std::fs::remove_dir_all(&out);
	std::fs::create_dir_all(&out).expect("writable tmp");

	println!("{:<24} {:>9} {:>26} {:>26} {:>26} {:>26}", "image", "src KiB", "ravif s6", "ravif s8", "ravif s10", "magick q50");
	let mut totals = [(0u128, 0u64); 4];
	for path in &inputs {
		let bytes = std::fs::read(path).expect("readable image");
		let rgba = image::load_from_memory(&bytes).expect("decodable image").to_rgba8();
		let (w, h) = rgba.dimensions();
		let pixels: Vec<ravif::RGBA8> = rgba.pixels().map(|p| ravif::RGBA8::new(p[0], p[1], p[2], p[3])).collect();
		let img = ravif::Img::new(pixels.as_slice(), w as usize, h as usize);

		let name = path.file_name().expect("a file").to_string_lossy().into_owned();
		print!("{name:<24} {:>9}", bytes.len() / 1024);
		for (slot, speed) in [0usize, 1, 2].into_iter().zip([6u8, 8, 10]) {
			let start = Instant::now();
			let encoded = ravif::Encoder::new()
				.with_quality(50.)
				.with_speed(speed)
				.with_num_threads(Some(1))
				.encode_rgba(img)
				.expect("ravif encodes rgba");
			let ms = start.elapsed().as_millis();
			totals[slot].0 += ms;
			totals[slot].1 += encoded.avif_file.len() as u64;
			print!("{ms:>21} ms{:>3} KiB", encoded.avif_file.len() / 1024);
		}

		let dest = out.join(format!("{name}.avif"));
		let start = Instant::now();
		let status = std::process::Command::new("magick")
			.args([&path.display().to_string(), "-quality", "50", &dest.display().to_string()])
			.status()
			.expect("magick on PATH");
		let ms = start.elapsed().as_millis();
		assert!(status.success(), "magick {status}");
		let size = std::fs::metadata(&dest).expect("magick wrote").len();
		totals[3].0 += ms;
		totals[3].1 += size;
		println!("{:>21} ms{:>3} KiB", ms, size / 1024);
	}

	println!();
	for (label, (ms, bytes)) in ["ravif s6", "ravif s8", "ravif s10", "magick q50"].into_iter().zip(totals) {
		println!("{label:<12} total {ms:>6} ms  {:>6} KiB", bytes / 1024);
	}
	std::fs::remove_dir_all(&out).expect("removable tmp");
}
