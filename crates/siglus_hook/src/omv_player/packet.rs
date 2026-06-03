use std::collections::VecDeque;

const MAX_PACKET_QUEUE_LEN: usize = 256;
const MAX_PACKET_QUEUE_BYTES: usize = 32 * 1024 * 1024;

pub struct PacketQueue {
    pub packets: VecDeque<Packet>,

    pub cached_bytes: usize,
    pub first_pts_ms: Option<i64>,
    pub last_pts_ms: Option<i64>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PacketQueueStats {
    pub len: usize,
    pub cached_bytes: usize,
    pub first_pts_ms: Option<i64>,
    pub last_pts_ms: Option<i64>,
}

pub struct Packet {
    pub inner: ffmpeg_next::packet::Packet,
    pub pts_ms: Option<i64>,
    pub duration_ms: i64,
    pub presentable: bool,
    pub is_keyframe: bool,
}
impl PacketQueue {
    pub(crate) fn new() -> Self {
        Self {
            packets: Default::default(),
            cached_bytes: 0,
            first_pts_ms: None,
            last_pts_ms: None,
        }
    }

    pub(crate) fn clear(&mut self) {
        self.packets.clear();
        self.cached_bytes = 0;
        self.first_pts_ms = None;
        self.last_pts_ms = None;
    }

    pub(crate) fn push_back(&mut self, packet: Packet) {
        self.cached_bytes += packet.inner.size();
        if self.first_pts_ms.is_none() {
            self.first_pts_ms = packet.pts_ms;
        }
        self.last_pts_ms = packet.pts_ms.or(self.last_pts_ms);
        self.packets.push_back(packet);
    }

    pub(crate) fn is_full(&self) -> bool {
        self.packets.len() >= MAX_PACKET_QUEUE_LEN || self.cached_bytes >= MAX_PACKET_QUEUE_BYTES
    }

    pub(crate) fn stats(&self) -> PacketQueueStats {
        PacketQueueStats {
            len: self.packets.len(),
            cached_bytes: self.cached_bytes,
            first_pts_ms: self.first_pts_ms,
            last_pts_ms: self.last_pts_ms,
        }
    }
}

impl Packet {
    pub fn set_presentable(&mut self, presentable: bool) {
        self.presentable = presentable;
    }
}
