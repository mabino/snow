//! Plain-file disk image (reads through the operating system filesystem;
//! on the web that is the Emscripten virtual file system, where the web
//! page uploads user-selected disk images)
//!
//! Bare HFS volumes are made bootable by prepending a SCSI driver and
//! partition map header (see [`snow_frontend_web::media`]).

use snow_core::mac::scsi::disk::DISK_BLOCKSIZE;
use snow_core::mac::scsi::disk_image::DiskImage;
use snow_core::mac::MacModel;
use snow_frontend_web::media;
use std::path::Path;

pub struct FileDiskImage {
    /// Disk contents
    disk: Vec<u8>,

    /// Path where the original image resides
    path: String,
}

impl FileDiskImage {
    /// Open a hard disk image, making bare HFS volumes bootable on `model`
    pub fn open_hard_disk(path: &str, model: MacModel) -> Result<Self, String> {
        let mut image = Self::open_block_sized(path)?;
        if media::is_bare_hfs_volume(&image.disk) {
            log::info!("{path} is a bare HFS volume, adding a SCSI driver header");
            image.disk = media::wrap_hfs_volume(&image.disk, model);
        }
        Ok(image)
    }

    /// Open a block-sized disk image from a (virtual) file
    pub fn open_block_sized(path: &str) -> Result<Self, String> {
        let disk =
            std::fs::read(path).map_err(|err| format!("Failed to read {}: {err}", path))?;
        if !disk.len().is_multiple_of(DISK_BLOCKSIZE) {
            return Err(format!(
                "Cannot load disk image {path}: not a multiple of {DISK_BLOCKSIZE} bytes"
            ));
        }
        Ok(Self {
            disk,
            path: path.to_string(),
        })
    }
}

impl DiskImage for FileDiskImage {
    fn byte_len(&self) -> usize {
        self.disk.len()
    }

    fn read_bytes(&self, offset: usize, length: usize) -> Vec<u8> {
        self.disk[offset..(offset + length)].to_vec()
    }

    fn write_bytes(&mut self, offset: usize, data: &[u8]) {
        self.disk[offset..(offset + data.len())].copy_from_slice(data);
    }

    fn media_bytes(&self) -> Option<&[u8]> {
        Some(&self.disk)
    }

    fn image_path(&self) -> Option<&Path> {
        Some(Path::new(&self.path))
    }
}
