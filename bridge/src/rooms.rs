//! Rooms: isolated LocalTalk networks
//!
//! Each room is its own AppleTalk network: LToUDP datagrams are relayed
//! between the clients in one room only. The bridge assigns every client
//! its LToUDP sender ID (clients cannot choose or spoof one). Optionally,
//! the default room is linked to the LToUDP multicast group on the LAN.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::frames::SENDER_ID_LEN;

/// Name of the room used when a client does not ask for one; the only room
/// that can be linked to the LAN
pub const DEFAULT_ROOM: &str = "default";

/// A frame queued for a client: (channel tag, payload)
pub type Outgoing = (u8, Vec<u8>);

/// Whether a room name is acceptable
pub fn valid_room_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// Sink for datagrams leaving a room towards the LAN
pub trait LanSink: Send + Sync {
    fn send(&self, dgram: &[u8]);
}

struct Member {
    sender_id: u32,
    tx: mpsc::Sender<Outgoing>,
}

struct Room {
    members: HashMap<u64, Member>,
}

/// Why a client could not join
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinError {
    TooManyRooms,
    RoomFull,
}

/// All rooms of the bridge
///
/// One lock covers all rooms; it is held only while queueing datagrams
/// (non-blocking `try_send`), so a client never waits on another one.
pub struct Rooms {
    rooms: Mutex<HashMap<String, Room>>,
    max_rooms: usize,
    max_room_clients: usize,
    lan: Mutex<Option<Arc<dyn LanSink>>>,
}

// Membership checks and fan-out must see a consistent room under the lock
#[allow(clippy::significant_drop_tightening)]
impl Rooms {
    pub fn new(max_rooms: usize, max_room_clients: usize) -> Self {
        Self {
            rooms: Mutex::default(),
            max_rooms,
            max_room_clients,
            lan: Mutex::new(None),
        }
    }

    /// Link the default room to the LAN
    pub fn set_lan(&self, lan: Arc<dyn LanSink>) {
        *self.lan.lock().unwrap() = Some(lan);
    }

    /// Whether a client could join `room` right now
    pub fn has_room_for(&self, room: &str) -> Result<(), JoinError> {
        let rooms = self.rooms.lock().unwrap();
        match rooms.get(room) {
            Some(r) if r.members.len() >= self.max_room_clients => Err(JoinError::RoomFull),
            Some(_) => Ok(()),
            None if rooms.len() >= self.max_rooms => Err(JoinError::TooManyRooms),
            None => Ok(()),
        }
    }

    /// Add client `id` to `room`; returns its assigned sender ID
    pub fn join(&self, room: &str, id: u64, tx: mpsc::Sender<Outgoing>) -> Result<u32, JoinError> {
        self.has_room_for(room)?;
        let mut rooms = self.rooms.lock().unwrap();
        let r = rooms.entry(room.to_string()).or_insert_with(|| Room {
            members: HashMap::new(),
        });
        let sender_id = loop {
            let candidate: u32 = rand::random();
            if candidate != 0 && r.members.values().all(|m| m.sender_id != candidate) {
                break candidate;
            }
        };
        r.members.insert(id, Member { sender_id, tx });
        Ok(sender_id)
    }

    /// Remove client `id` from `room` (empty rooms disappear)
    pub fn leave(&self, room: &str, id: u64) {
        let mut rooms = self.rooms.lock().unwrap();
        if let Some(r) = rooms.get_mut(room) {
            r.members.remove(&id);
            if r.members.is_empty() {
                rooms.remove(room);
            }
        }
    }

    /// Relay a datagram from client `from` to the rest of its room (and the
    /// LAN for the default room), with the client's assigned sender ID.
    /// Returns how many clients it was queued for.
    pub fn relay_from_client(&self, room: &str, from: u64, dgram: &[u8]) -> usize {
        let (stamped, queued) = {
            let rooms = self.rooms.lock().unwrap();
            let Some(r) = rooms.get(room) else { return 0 };
            let Some(sender) = r.members.get(&from) else {
                return 0;
            };
            let mut stamped = dgram.to_vec();
            stamped[..SENDER_ID_LEN].copy_from_slice(&sender.sender_id.to_be_bytes());
            let queued = Self::fan_out(r, Some(from), &stamped);
            (stamped, queued)
        };
        if room == DEFAULT_ROOM
            && let Some(lan) = self.lan.lock().unwrap().as_ref()
        {
            lan.send(&stamped);
        }
        queued
    }

    /// Relay a datagram received from the LAN to the default room, unless
    /// it is the echo of one of the room's own clients
    pub fn relay_from_lan(&self, dgram: &[u8]) {
        let rooms = self.rooms.lock().unwrap();
        let Some(r) = rooms.get(DEFAULT_ROOM) else {
            return;
        };
        let sender = u32::from_be_bytes(dgram[..SENDER_ID_LEN].try_into().unwrap());
        if r.members.values().any(|m| m.sender_id == sender) {
            return;
        }
        Self::fan_out(r, None, dgram);
    }

    fn fan_out(r: &Room, exclude: Option<u64>, dgram: &[u8]) -> usize {
        let mut queued = 0;
        for (id, m) in &r.members {
            if Some(*id) == exclude {
                continue;
            }
            // A full queue drops the datagram (LocalTalk is lossy anyway)
            if m.tx
                .try_send((snow_core::net::TAG_LOCALTALK, dgram.to_vec()))
                .is_ok()
            {
                queued += 1;
            }
        }
        queued
    }

    pub fn room_count(&self) -> usize {
        self.rooms.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(rooms: &Rooms, room: &str, id: u64) -> (u32, mpsc::Receiver<Outgoing>) {
        let (tx, rx) = mpsc::channel(16);
        (rooms.join(room, id, tx).unwrap(), rx)
    }

    const ENQ: [u8; 7] = [0xAA, 0xBB, 0xCC, 0xDD, 0x4F, 0x4F, 0x81];

    #[test]
    fn room_names() {
        assert!(valid_room_name("default"));
        assert!(valid_room_name("Bolo_night-2.0"));
        assert!(!valid_room_name(""));
        assert!(!valid_room_name("a/b"));
        assert!(!valid_room_name("x".repeat(65).as_str()));
    }

    #[test]
    fn relays_within_room_only_with_assigned_ids() {
        let rooms = Rooms::new(8, 8);
        let (id_a, mut a) = member(&rooms, "one", 1);
        let (_, mut b) = member(&rooms, "one", 2);
        let (_, mut c) = member(&rooms, "two", 3);

        assert_eq!(rooms.relay_from_client("one", 1, &ENQ), 1);
        let (tag, got) = b.try_recv().unwrap();
        assert_eq!(tag, snow_core::net::TAG_LOCALTALK);
        assert_eq!(
            got[..4],
            id_a.to_be_bytes(),
            "sender ID is the bridge's, not the client's"
        );
        assert_eq!(got[4..], ENQ[4..]);
        assert!(a.try_recv().is_err(), "no echo to the sender");
        assert!(c.try_recv().is_err(), "other rooms are isolated");
    }

    #[test]
    fn room_limits_and_cleanup() {
        let rooms = Rooms::new(1, 2);
        let _a = member(&rooms, "one", 1);
        let _b = member(&rooms, "one", 2);
        let (tx, _rx) = mpsc::channel(1);
        assert_eq!(rooms.join("one", 3, tx.clone()), Err(JoinError::RoomFull));
        assert_eq!(
            rooms.join("two", 4, tx.clone()),
            Err(JoinError::TooManyRooms)
        );
        rooms.leave("one", 1);
        rooms.leave("one", 2);
        assert_eq!(rooms.room_count(), 0, "empty rooms disappear");
        assert!(rooms.join("two", 4, tx).is_ok());
    }

    struct Capture(Mutex<Vec<Vec<u8>>>);
    impl LanSink for Capture {
        fn send(&self, dgram: &[u8]) {
            self.0.lock().unwrap().push(dgram.to_vec());
        }
    }

    #[test]
    fn lan_link_is_default_room_only_and_filters_echo() {
        let rooms = Rooms::new(8, 8);
        let lan = Arc::new(Capture(Mutex::default()));
        rooms.set_lan(lan.clone());
        let (id_a, _a) = member(&rooms, DEFAULT_ROOM, 1);
        let (_, mut b) = member(&rooms, DEFAULT_ROOM, 2);
        let _c = member(&rooms, "private", 3);

        rooms.relay_from_client(DEFAULT_ROOM, 1, &ENQ);
        rooms.relay_from_client("private", 3, &ENQ);
        assert_eq!(
            lan.0.lock().unwrap().len(),
            1,
            "only the default room reaches the LAN"
        );
        b.try_recv().unwrap();

        // Echo of client 1's own datagram from the multicast group: dropped
        let mut echo = ENQ;
        echo[..4].copy_from_slice(&id_a.to_be_bytes());
        rooms.relay_from_lan(&echo);
        assert!(b.try_recv().is_err());
        // A real LAN node's datagram reaches everyone
        rooms.relay_from_lan(&[9, 9, 9, 9, 0x10, 0x10, 0x81]);
        assert!(b.try_recv().is_ok());
    }
}
