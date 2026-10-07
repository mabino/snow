//! Infinite Mac's emulated Ethernet (its AppleTalk zone relay), as used by
//! the Basilisk II and SheepShaver cores

use std::ffi::CString;
use std::os::raw::c_char;

extern "C" {
    fn js_ether_init(mac_address: *const c_char);
    fn js_ether_write(destination: *const c_char, buf_ptr: *const u8, buf_size: u32);
    fn js_ether_read(buf_ptr: *mut u8, buf_size: u32) -> i32;
}

/// Attach to the zone with a MAC address ("aa:bb:cc:dd:ee:ff")
pub fn init(mac_address: &str) {
    let mac = CString::new(mac_address).unwrap();
    unsafe {
        js_ether_init(mac.as_ptr());
    }
}

/// Send a frame; `destination` is a MAC address, "*" (everyone in the zone)
/// or "AT" (AppleTalk multicast)
pub fn write(destination: &str, frame: &[u8]) {
    let destination = CString::new(destination).unwrap();
    let len = u32::try_from(frame.len()).unwrap_or(u32::MAX);
    unsafe {
        js_ether_write(destination.as_ptr(), frame.as_ptr(), len);
    }
}

/// Receive one frame into `buf`; returns its length, 0 if none is waiting
pub fn read(buf: &mut [u8]) -> usize {
    let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
    let n = unsafe { js_ether_read(buf.as_mut_ptr(), len) };
    usize::try_from(n).unwrap_or(0)
}
