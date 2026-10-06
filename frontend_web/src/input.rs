//! Page -> emulator I/O: input events and received network packets
//!
//! Drains the page's I/O ring every main loop iteration (see
//! [`snow_frontend_web::io_protocol`]) and feeds the records to the
//! emulator (input) or the net hub (packets, link state). Also posts the
//! packets the emulated network devices queued for sending.

use snow_core::emulator::comm::{EmulatorCommand, EmulatorCommandSender};
use snow_core::emulator::MouseMode;
use snow_core::keymap::{KeyEvent, Keymap};
use snow_core::net;
use snow_frontend_web::io_protocol::IoRecord;

use crate::js_api;

/// Upper bound of records handled per tick, so a flood of packets cannot
/// starve the emulator
const MAX_RECORDS_PER_TICK: usize = 256;

pub struct Receiver {
    cmd_sender: EmulatorCommandSender,
    mouse_mode: MouseMode,
}

impl Receiver {
    pub fn new(cmd_sender: EmulatorCommandSender, mouse_mode: MouseMode) -> Self {
        Self {
            cmd_sender,
            mouse_mode,
        }
    }

    pub fn tick(&self) {
        for _ in 0..MAX_RECORDS_PER_TICK {
            let Some(raw) = js_api::poll_io() else {
                break;
            };
            match IoRecord::parse(&raw) {
                Some(record) => self.handle(record),
                None => log::debug!("Ignoring unknown I/O record {:02X?}", raw.first()),
            }
        }

        let outgoing = net::take_outgoing();
        if !outgoing.is_empty() {
            let mut frames = Vec::new();
            for (tag, payload) in &outgoing {
                net::push_frame(&mut frames, *tag, payload);
            }
            js_api::net_send(&frames);
        }
    }

    fn send(&self, cmd: EmulatorCommand) {
        let _ = self.cmd_sender.send(cmd);
    }

    fn handle(&self, record: IoRecord) {
        match record {
            IoRecord::Net(tag, payload) => net::deliver(tag, payload),
            IoRecord::Link { up, description } => net::set_link(up, &description),
            IoRecord::Key { scancode, down } => {
                let event = if down {
                    KeyEvent::KeyDown(scancode, Keymap::Universal)
                } else {
                    KeyEvent::KeyUp(scancode, Keymap::Universal)
                };
                self.send(EmulatorCommand::KeyEvent(event));
            }
            IoRecord::MouseButton(down) => self.send(EmulatorCommand::MouseUpdateRelative {
                relx: 0,
                rely: 0,
                btn: Some(down),
            }),
            IoRecord::MouseAbs { x, y } => {
                if self.mouse_mode == MouseMode::Absolute {
                    self.send(EmulatorCommand::MouseUpdateAbsolute { x, y });
                }
            }
            IoRecord::MouseRel { dx, dy } => {
                if self.mouse_mode == MouseMode::RelativeHw && (dx != 0 || dy != 0) {
                    self.send(EmulatorCommand::MouseUpdateRelative {
                        relx: dx,
                        rely: dy,
                        btn: None,
                    });
                }
            }
        }
    }
}
