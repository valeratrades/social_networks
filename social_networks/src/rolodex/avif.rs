//! Attachment images, kept at a size an archive can hold decades of.

use std::path::Path;

use color_eyre::eyre::{Result, WrapErr, bail};
use image::AnimationDecoder as _;

/// Measured over real DM images against `magick -quality 50`: at this speed ravif is a little
/// faster for ~30% more bytes, and every slower preset loses on time. See `examples/avif_bench.rs`.
///
/// rav1e's `asm` feature halves the encode, and is off: it wants `nasm` at build time, on every CI
/// runner and every machine that builds from source, to buy back bytes an archive can afford.
const QUALITY: f32 = 50.;
const SPEED: u8 = 10;

/// Whether the bytes are a still raster [`convert`] can keep whole. An animated gif is not — only
/// its first frame would survive, and a reaction gif is the animation.
pub fn still(mime: &str, bytes: &[u8]) -> bool {
	match mime {
		"image/png" | "image/jpeg" | "image/webp" => true,
		"image/gif" => image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes)).is_ok_and(|gif| gif.into_frames().take(2).count() == 1),
		_ => false,
	}
}

/// A no-op when `dest` is already there, which is what makes a re-downloaded attachment free and an
/// orphan from a failed pull harmless.
pub fn convert(bytes: &[u8], dest: &Path) -> Result<()> {
	if dest.exists() {
		return Ok(());
	}
	let rgba = image::load_from_memory(bytes).wrap_err("undecodable image")?.to_rgba8();
	let (width, height) = rgba.dimensions();
	if width == 0 || height == 0 {
		bail!("image is {width}x{height}");
	}
	let pixels: Vec<ravif::RGBA8> = rgba.pixels().map(|p| ravif::RGBA8::new(p[0], p[1], p[2], p[3])).collect();
	let encoded = ravif::Encoder::new()
		.with_quality(QUALITY)
		.with_speed(SPEED)
		.encode_rgba(ravif::Img::new(pixels.as_slice(), width as usize, height as usize))
		.map_err(|e| color_eyre::eyre::eyre!("avif encode: {e}"))?;

	let parent = dest.parent().expect("assets paths carry a directory");
	std::fs::create_dir_all(parent).wrap_err_with(|| format!("failed to create {}", parent.display()))?;
	std::fs::write(dest, encoded.avif_file).wrap_err_with(|| format!("failed to write {}", dest.display()))
}
