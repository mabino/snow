//! Boot media helpers for System 6 networking setups
//!
//! - [`wrap_hfs_volume`]: classic Mac OS install images are often bare HFS
//!   volumes (the format used by Infinite Mac and Mini vMac). A real Mac can
//!   only boot a SCSI disk that has an Apple partition map and a SCSI driver,
//!   so a bare volume is wrapped with a device header, as Infinite Mac does.
//! - [`appletalk_pram`]: a parameter RAM image with AppleTalk set to active.
//!   With freshly initialized PRAM AppleTalk is inactive, and System 6 never
//!   opens the LocalTalk driver (the Chooser setting is stored in PRAM).
//!   The image also carries a LocalTalk node address hint. The emulation is
//!   deterministic, so two instances booted from identical PRAM would pick
//!   the same "random" tentative addresses forever, colliding on every
//!   LLAP enquiry; a per-instance hint makes each one start (and normally
//!   settle) on its own address, just like a real Mac remembering its last
//!   address.

use snow_core::mac::MacModel;

/// Size of a disk block
const BLOCK_SIZE: usize = 512;

/// Device header with the Apple SCSI 1.5 driver in the driver descriptor
/// block (works with the 64K/128K ROMs of compact Macs); partition 0 is HFS
const HEADER_SCSI_15: &[u8] = include_bytes!("../assets/scsi-1.5-header.hda");
/// Device header with a partition map and the Apple SCSI 4.3 driver
/// (Mac II class ROMs); partition 2 is HFS
const HEADER_SCSI_43: &[u8] = include_bytes!("../assets/scsi-4.3-header.hda");

/// Whether a disk image is a bare HFS volume (no driver descriptor block)
pub fn is_bare_hfs_volume(image: &[u8]) -> bool {
    image.len() >= 3 * BLOCK_SIZE
        && image.len().is_multiple_of(BLOCK_SIZE)
        && &image[0..2] != b"ER"
        && &image[1024..1026] == b"BD"
}

/// Whether a model needs the compact-Mac SCSI driver header
fn uses_compact_scsi_driver(model: MacModel) -> bool {
    matches!(
        model,
        MacModel::Early128K
            | MacModel::Early512K
            | MacModel::Early512Ke
            | MacModel::Plus
            | MacModel::SE
            | MacModel::Classic
    )
}

/// Prepend a bootable device header to a bare HFS volume
pub fn wrap_hfs_volume(volume: &[u8], model: MacModel) -> Vec<u8> {
    let (header, hfs_partition_index) = if uses_compact_scsi_driver(model) {
        (HEADER_SCSI_15, 0)
    } else {
        (HEADER_SCSI_43, 2)
    };
    let mut image = Vec::with_capacity(header.len() + volume.len());
    image.extend_from_slice(header);

    let total_blocks = u32::try_from((header.len() + volume.len()) / BLOCK_SIZE).unwrap();
    let hfs_blocks = u32::try_from(volume.len() / BLOCK_SIZE).unwrap();
    image[4..8].copy_from_slice(&total_blocks.to_be_bytes()); // sbBlkCount

    // Partition map entries start at block 1
    let entry = (hfs_partition_index + 1) * BLOCK_SIZE;
    image[entry + 12..entry + 16].copy_from_slice(&hfs_blocks.to_be_bytes()); // pmPartBlkCnt
    image[entry + 84..entry + 88].copy_from_slice(&hfs_blocks.to_be_bytes()); // pmDataCnt

    image.extend_from_slice(volume);
    image
}

/// PRAM address of SPConfig (serial port use: low nibble = port B)
pub const PRAM_SPCONFIG: usize = 0x13;
/// SPConfig "use AppleTalk" value for port B
const USE_ATALK: u8 = 0x01;
/// PRAM address of SPATalkB (LocalTalk node address hint for port B)
pub const PRAM_NODE_HINT_B: usize = 0x12;

/// A random LocalTalk node address in the workstation range (1-127)
pub fn random_node_hint() -> u8 {
    use std::hash::{BuildHasher, Hasher};
    // RandomState is seeded from the OS random source (on the web:
    // crypto.getRandomValues)
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(std::process::id().into());
    (hasher.finish() % 127) as u8 + 1
}

/// A 256 byte PRAM image with AppleTalk active on the printer port
///
/// Holds the defaults a Macintosh SE ROM writes on first boot, plus
/// `node_hint` as the remembered LocalTalk node address.
pub fn appletalk_pram(node_hint: u8) -> Vec<u8> {
    let mut pram = vec![0u8; 256];
    let defaults: &[(usize, u8)] = &[
        (0x08, 0x03),
        (0x09, 0x88),
        (0x0B, 0x4C),
        (0x0C, b'B'), // extended PRAM signature
        (0x0D, b'u'),
        (0x0E, b'g'),
        (0x0F, b's'),
        (0x10, 0xA8), // SPValid
        (0x14, 0xCC), // SPPortA: 9600 8N2
        (0x15, 0x0A),
        (0x16, 0xCC), // SPPortB
        (0x17, 0x0A),
        (0x1D, 0x02), // SPKbd
        (0x1E, 0x63), // SPPrint
    ];
    for &(addr, val) in defaults {
        pram[addr] = val;
    }
    pram[PRAM_SPCONFIG] = USE_ATALK;
    pram[PRAM_NODE_HINT_B] = node_hint;
    pram
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bare_volume(blocks: usize) -> Vec<u8> {
        let mut vol = vec![0u8; blocks * BLOCK_SIZE];
        vol[0..2].copy_from_slice(b"LK");
        vol[1024..1026].copy_from_slice(b"BD");
        vol
    }

    fn be32(b: &[u8], at: usize) -> u32 {
        u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
    }

    #[test]
    fn detects_bare_volumes() {
        assert!(is_bare_hfs_volume(&bare_volume(100)));
        let wrapped = wrap_hfs_volume(&bare_volume(100), MacModel::SE);
        assert!(!is_bare_hfs_volume(&wrapped));
        assert!(!is_bare_hfs_volume(&[0u8; 2048]));
        assert!(!is_bare_hfs_volume(&bare_volume(100)[..1000]));
    }

    #[test]
    fn wraps_for_compact_macs() {
        let vol = bare_volume(1000);
        let img = wrap_hfs_volume(&vol, MacModel::SE);
        assert_eq!(&img[0..2], b"ER");
        assert_eq!(img.len(), HEADER_SCSI_15.len() + vol.len());
        assert_eq!(be32(&img, 4) as usize, img.len() / BLOCK_SIZE);
        // Partition 0 (block 1) is the HFS partition, sized to the volume
        assert_eq!(&img[512..514], b"PM");
        assert_eq!(&img[512 + 48..512 + 57], b"Apple_HFS");
        assert_eq!(be32(&img, 512 + 12), 1000);
        assert_eq!(be32(&img, 512 + 84), 1000);
        // ... and starts right where the volume was appended
        assert_eq!(be32(&img, 512 + 8) as usize * BLOCK_SIZE, HEADER_SCSI_15.len());
        assert_eq!(&img[HEADER_SCSI_15.len()..], &vol[..]);
    }

    #[test]
    fn wraps_for_mac_ii() {
        let vol = bare_volume(2000);
        let img = wrap_hfs_volume(&vol, MacModel::MacII);
        assert_eq!(img.len(), HEADER_SCSI_43.len() + vol.len());
        let entry = 3 * BLOCK_SIZE;
        assert_eq!(&img[entry + 48..entry + 57], b"Apple_HFS");
        assert_eq!(be32(&img, entry + 12), 2000);
        assert_eq!(be32(&img, entry + 8) as usize * BLOCK_SIZE, HEADER_SCSI_43.len());
    }

    #[test]
    fn pram_has_appletalk_active() {
        let pram = appletalk_pram(0x21);
        assert_eq!(pram.len(), 256);
        assert_eq!(pram[0x10], 0xA8, "valid classic PRAM");
        assert_eq!(pram[PRAM_SPCONFIG] & 0x0F, USE_ATALK);
        assert_eq!(pram[PRAM_NODE_HINT_B], 0x21);
    }

    #[test]
    fn node_hints_are_workstation_addresses() {
        let hints: Vec<u8> = (0..200).map(|_| random_node_hint()).collect();
        assert!(hints.iter().all(|h| (1..=127).contains(h)));
        // Not all the same (the whole point of the hint)
        assert!(hints.iter().any(|h| *h != hints[0]));
    }
}
