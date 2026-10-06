//! Standalone web frontend for Snow
//!
//! Runs in a browser Web Worker (see `www/`). For networking, the page
//! connects to the host-side `snow-bridge` process over a WebSocket and
//! relays packets to/from the emulated network devices through the net hub
//! ([`snow_core::net`]):
//! - `--localtalk-bridge`  enables LocalTalk/AppleTalk on the printer port
//!   (serial channel B) - System 6 AppleTalk between browser tabs, other
//!   LToUDP emulators and real Macs
//! - `--appletalk-pram`    when no `--pram` is given, boot with a PRAM image
//!   that has AppleTalk set to active (fresh PRAM has it inactive, and
//!   System 6 then never opens the LocalTalk driver) and a random LocalTalk
//!   node address hint (`--appletalk-node <n>` picks it instead)
//! - `--ethernet-nat`      attaches a DaynaPORT SCSI Ethernet adapter that
//!   reaches the internet through the bridge's NAT engine (with MacTCP)
//! - `--full-speed`        disable the real-time speed governor
//!
//! All media (ROMs, disk images, floppies, CD-ROMs, PRAM) is read through
//! the Emscripten virtual file system; the page writes user-selected files
//! there before starting the emulator. Bare HFS volumes (as used by
//! Infinite Mac) are given a SCSI driver/partition map header on the fly.

use snow_core::emulator::comm::{
    EmulatorCommand, EmulatorEvent, EmulatorStatus, UserMessageType,
};
use snow_core::emulator::{Emulator, MouseMode};
use snow_core::mac::scc::SccCh;
use snow_core::mac::serial_bridge::SerialBridgeConfig;
use snow_core::mac::{ExtraROMs, MacModel, MacMonitor};
use snow_core::tickable::Tickable;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::cdrom::CdromManager;
use crate::disk::FileDiskImage;
use snow_frontend_web::media;
use crate::floppy::load_floppy_image;
use crate::floppy_manager::FloppyManager;

mod audio;
mod cdrom;
mod clipboard;
mod disk;
mod floppy;
mod floppy_manager;
mod framebuffer;
mod input;
mod js_api;
mod memory;
mod removable_media;

fn main() {
    let mut args = pico_args::Arguments::from_env();
    let rom_path: String = args.value_from_str("--rom").unwrap();
    let disk_names: Vec<String> = args.values_from_str("--disk").unwrap();
    let floppy_names: Vec<String> = args.values_from_str("--floppy").unwrap_or_default();
    let cdrom_names: Vec<String> = args.values_from_str("--cdrom").unwrap_or_default();
    let gestalt_id: u32 = args.value_from_str("--gestalt-id").unwrap();
    let ram_size: usize = args.value_from_str("--ram-size").unwrap();
    let monitor_id: Option<String> = args.opt_value_from_str("--monitor").unwrap();
    let extra_rom_paths: Vec<String> = args.values_from_str("--extra-rom").unwrap_or_default();
    let pram_path: Option<String> = args.opt_value_from_str("--pram").unwrap();
    let debug_log = args.contains("--debug-log");
    let full_speed = args.contains("--full-speed");
    let mouse_mode = if args.contains("--use-mouse-deltas") {
        MouseMode::RelativeHw
    } else {
        MouseMode::Absolute
    };

    // Networking
    let localtalk_bridge = args.contains("--localtalk-bridge");
    let ethernet_nat = args.contains("--ethernet-nat");
    let ethernet_scsi_id: usize = args
        .opt_value_from_str("--ethernet-id")
        .unwrap_or_else(|err| panic!("invalid --ethernet-id: {err}"))
        .unwrap_or(3);
    let appletalk_pram = args.contains("--appletalk-pram");
    let appletalk_node: Option<u8> = args
        .opt_value_from_str("--appletalk-node")
        .unwrap_or_else(|err| panic!("invalid --appletalk-node: {err}"));

    let default_log_filter = if debug_log { "trace" } else { "info" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_log_filter))
        .target(env_logger::Target::Stderr)
        .init();

    // Initialize the web UI (canvas, audio, input) before anything else
    js_api::init();

    let rom_data =
        std::fs::read(&rom_path).unwrap_or_else(|err| panic!("Failed to read ROM: {err}"));

    let model = model_from_gestalt(gestalt_id).unwrap_or_else(|| {
        panic!("Unknown gestalt ID {gestalt_id} (no matching Snow model)")
    });
    if !model.ram_size_options().contains(&ram_size) {
        panic!(
            "Unsupported RAM size {ram_size} for {model} (default {})",
            model.ram_size_default()
        );
    }
    let monitor = monitor_id.map(|id| match id.as_str() {
        "RGB12" => MacMonitor::RGB12,
        "HiRes14" => MacMonitor::HiRes14,
        "RGB16" => MacMonitor::RGB16,
        "RGB21" => MacMonitor::RGB21,
        "PortraitBW" => MacMonitor::PortraitBW,
        _ => panic!("Unknown monitor ID '{id}'"),
    });

    let mut extra_rom_data = Vec::new();
    for rom_path in &extra_rom_paths {
        let data = std::fs::read(rom_path).unwrap_or_else(|err| {
            panic!("Failed to read extra ROM '{rom_path}': {err}")
        });
        extra_rom_data.push((rom_path.clone(), data));
    }
    let mut extra_roms = Vec::new();
    for (rom_path, data) in &extra_rom_data {
        let data_ref = data.as_slice();
        let rom = match rom_path.as_str() {
            "mac-ii-display-card.rom" => ExtraROMs::Toby(data_ref),
            "mac-ii-display-card-8-24.rom" => ExtraROMs::MDC12(data_ref),
            "se30-video.rom" => ExtraROMs::SE30Video(data_ref),
            "extension.rom" => ExtraROMs::ExtensionROM(data_ref),
            _ => panic!("Unknown extra ROM '{rom_path}'"),
        };
        extra_roms.push(rom);
    }

    let (mut emulator, frame_receiver) = Emulator::new_with_extra(
        &rom_data,
        &extra_roms,
        model,
        monitor,
        mouse_mode,
        Some(ram_size),
        None,
        // We don't expose a separate PPMU option, so just trigger it for the
        // FDHD model.
        model == MacModel::MacIIFDHD,
        None,
    )
    .expect("Failed to create emulator");

    if let Some(pram_path) = &pram_path {
        emulator.persist_pram(Path::new(pram_path));
    } else if appletalk_pram {
        const PRAM_PATH: &str = "/appletalk.pram";
        let node_hint = appletalk_node
            .filter(|n| (1..=254).contains(n))
            .unwrap_or_else(media::random_node_hint);
        match std::fs::write(PRAM_PATH, media::appletalk_pram(node_hint)) {
            Ok(()) => {
                emulator.persist_pram(Path::new(PRAM_PATH));
                log::info!("Booting with AppleTalk active in PRAM (LocalTalk node hint {node_hint})");
            }
            Err(err) => log::error!("Failed to create AppleTalk PRAM: {err}"),
        }
    }
    emulator.set_pram_logging(debug_log);

    // SCSI devices
    let mut next_disk_scsi_id = 0;
    let mut occupied_scsi_ids = [false; 7];
    for disk_name in &disk_names {
        match FileDiskImage::open_hard_disk(disk_name, model) {
            Ok(disk) => match emulator.attach_disk_image_at(Box::new(disk), next_disk_scsi_id) {
                Ok(_) => {
                    occupied_scsi_ids[next_disk_scsi_id] = true;
                    next_disk_scsi_id += 1;
                }
                Err(err) => {
                    log::error!("Failed to attach SCSI disk '{disk_name}': {err}");
                }
            },
            Err(err) => {
                js_api::report_error(&format!("Failed to open SCSI disk '{disk_name}': {err}"));
            }
        }
    }

    // DaynaPORT SCSI/Link ethernet (only meaningful with a bridge)
    #[cfg(feature = "ethernet")]
    if ethernet_nat {
        if !model.has_scsi() {
            log::warn!(
                "Skipping DaynaPORT ethernet: {model} does not have SCSI"
            );
        } else if !occupied_scsi_ids[ethernet_scsi_id] {
            emulator.attach_ethernet(ethernet_scsi_id);
            occupied_scsi_ids[ethernet_scsi_id] = true;
            log::info!("Attached DaynaPORT ethernet at SCSI ID #{ethernet_scsi_id}");
        } else {
            js_api::report_error(&format!(
                "Cannot attach DaynaPORT ethernet: SCSI ID #{ethernet_scsi_id} is in use"
            ));
        }
    }

    // CD-ROM drives (read drive status through events)
    let mut cdrom_manager =
        CdromManager::new(&mut emulator, next_disk_scsi_id, cdrom_names);
    if cdrom_manager.is_some() {
        occupied_scsi_ids[next_disk_scsi_id] = true;
    }

    let audio_provider = Arc::new(Mutex::new(audio::WebAudioProvider::new()));
    emulator
        .set_audio_provider(audio_provider)
        .expect("Failed to initialize audio");

    log::info!(
        "Initialized {} SCSI devices",
        occupied_scsi_ids.iter().filter(|occupied| **occupied).count()
    );

    let cmd_sender = emulator.create_cmd_sender();
    let event_recv = emulator.create_event_recv();
    let mut floppy_manager = FloppyManager::new(cmd_sender.clone());

    // LocalTalk/AppleTalk on the B serial channel
    if localtalk_bridge {
        cmd_sender
            .send(EmulatorCommand::SerialBridgeEnable(
                SccCh::B,
                SerialBridgeConfig::LocalTalk,
            ))
            .unwrap();
        log::info!("LocalTalk/AppleTalk bridge enabled (serial channel B)");
    }

    // Initial floppies
    let mut floppy_drive = 0usize;
    for floppy_name in &floppy_names {
        if floppy_drive >= 3 {
            js_api::report_error(&format!(
                "Failed to insert floppy '{floppy_name}': no free drive (max 3)"
            ));
            continue;
        }
        match load_floppy_image(floppy_name) {
            Ok(img) => {
                if let Err(err) = cmd_sender.send(EmulatorCommand::InsertFloppyImage(
                    floppy_drive,
                    Box::new(img),
                    false,
                )) {
                    js_api::report_error(&format!(
                        "Failed to insert floppy '{floppy_name}': {err}"
                    ));
                } else {
                    floppy_drive += 1;
                }
            }
            Err(err) => {
                js_api::report_error(&format!("Failed to open floppy '{floppy_name}': {err}"));
            }
        }
    }

    cmd_sender.send(EmulatorCommand::Run).unwrap();

    let mut framebuffer_sender = framebuffer::Sender::new(frame_receiver);
    let input_receiver = input::Receiver::new(cmd_sender, mouse_mode);
    let mut memory_mirror = memory::MemoryMirror::new();
    let mut clipboard_sync = clipboard::ClipboardSync::new();
    let mut last_status: Option<Box<EmulatorStatus>> = None;

    // Pacing (speed governor) state. A full-speed 68000 pins the worker's
    // event loop, starving the WebSocket and Web Audio that networking and
    // audio rely on. The governor measures guest time (Snow's 68k bus runs
    // at a fixed 16 MHz, so guest seconds = CPU cycles / 16e6) against wall
    // time and sleeps - yielding the event loop - when the guest gets ahead
    // of real time. Disabled with --full-speed.
    let mut pace_speed = 1.0; // smoothed guest-seconds per wall-second
    let mut pace_debt = 0.0; // unslept pacing budget (seconds)
    let mut pace_anchor_cycles = emulator.get_cycles();
    let mut pace_work_anchor = std::time::Instant::now();
    loop {
        input_receiver.tick();

        while let Ok(event) = event_recv.try_recv() {
            match event {
                EmulatorEvent::Status(status) => {
                    last_status = Some(status);
                }
                EmulatorEvent::Memory((addr, data, size)) => {
                    memory_mirror.update(addr, &data, size);
                }
                EmulatorEvent::UserMessage(message_type, message) => match message_type {
                    UserMessageType::Error => js_api::report_error(&message),
                    UserMessageType::Warning => log::warn!("{message}"),
                    UserMessageType::Notice | UserMessageType::Success => {
                        log::info!("{message}");
                    }
                },
                _ => {}
            }
        }

        if let Some(update) = clipboard_sync.tick(&memory_mirror) {
            log::info!("Updating host clipboard from emulator scrap");
            js_api::set_clipboard_text(&update.text);
        }

        if let Some(cdrom_manager) = cdrom_manager.as_mut() {
            cdrom_manager.tick(&mut emulator, last_status.as_deref());
        }
        floppy_manager.tick(&mut emulator, last_status.as_deref());

        if let Err(e) = emulator.tick(1, ()) {
            js_api::report_error(&format!("Emulator tick error: {e:#}"));
            break;
        }

        framebuffer_sender.tick();

        if !full_speed {
            pace_guest(
                &emulator,
                &mut pace_speed,
                &mut pace_debt,
                &mut pace_anchor_cycles,
                &mut pace_work_anchor,
            );
        }
    }
}

const GESTALT_MODEL_MAP: &[(u32, MacModel)] = &[
    (1, MacModel::Early128K),
    (2, MacModel::Early512K),
    (3, MacModel::Early512Ke),
    (4, MacModel::Plus),
    (5, MacModel::SE),
    (6, MacModel::MacII),
    (7, MacModel::MacIIx),
    (8, MacModel::MacIIcx),
    (9, MacModel::SE30),
    (17, MacModel::Classic),
];

fn model_from_gestalt(gestalt_id: u32) -> Option<MacModel> {
    GESTALT_MODEL_MAP
        .iter()
        .find(|(id, _)| *id == gestalt_id)
        .map(|(_, model)| *model)
}

/// Snow's 68k bus is clocked at 16 MHz; guest seconds = cycles / 16e6
const CPU_CLOCK_HZ: f64 = 16e6;
/// Governor EMA time constant (seconds)
const PACE_WINDOW: f64 = 0.1;
/// Start pacing above this guest speed (guest-seconds per wall-second)
const PACE_ON: f64 = 1.15;
/// Stop pacing below this speed
const PACE_OFF: f64 = 0.9;
/// Maximum single sleep (one display frame)
const PACE_SLEEP_MAX: f64 = 0.016;
/// Start sleeping once the accumulated budget exceeds this
const PACE_DEBT_ON: f64 = 0.016;

/// Measure guest time against wall time; sleep (yielding the worker's
/// event loop) when the guest runs ahead of real time
#[allow(clippy::cast_precision_loss)]
fn pace_guest(
    emulator: &Emulator,
    pace_speed: &mut f64,
    pace_debt: &mut f64,
    pace_anchor_cycles: &mut u64,
    pace_work_anchor: &mut std::time::Instant,
) {
    let now = std::time::Instant::now();
    let work_wall = now.duration_since(*pace_work_anchor).as_secs_f64();
    let cycles = emulator.get_cycles();
    let guest_secs = (cycles as f64 - *pace_anchor_cycles as f64) / CPU_CLOCK_HZ;
    *pace_anchor_cycles = cycles;
    *pace_work_anchor = now;
    if work_wall <= 0.0 {
        return;
    }
    // Smooth the (noisy) per-iteration speed estimate
    let alpha = (work_wall / PACE_WINDOW).min(1.0);
    *pace_speed += alpha * (guest_secs / work_wall - *pace_speed);
    if *pace_speed > PACE_ON {
        *pace_debt += guest_secs - work_wall;
        if *pace_debt > PACE_DEBT_ON {
            let chunk = (*pace_debt).clamp(0.002, PACE_SLEEP_MAX);
            js_api::sleep_seconds(chunk);
            *pace_debt -= chunk;
            *pace_work_anchor = std::time::Instant::now();
        }
    } else if *pace_speed < PACE_OFF {
        *pace_debt = 0.0;
    }
}
