//! Floppy image loading from (virtual) files

use snow_floppy::loaders::{Autodetect, FloppyImageLoader};
use snow_floppy::FloppyImage;

pub fn load_floppy_image(name: &str) -> Result<FloppyImage, String> {
    let buffer = std::fs::read(name)
        .map_err(|err| format!("Failed to read floppy image {name}: {err}"))?;
    Autodetect::load(&buffer, Some(name))
        .map_err(|err| format!("Cannot load floppy image {name}: {err:#}"))
}
