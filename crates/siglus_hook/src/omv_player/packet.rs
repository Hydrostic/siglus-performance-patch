use std::collections::VecDeque;

const MAX_PACKET_STATS_LEN: usize = 64;

pub struct PacketQueue {
    packets: VecDeque<PacketInfo>,
    retained_bytes: usize,
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

struct PacketInfo {
    size: usize,
    pts_ms: Option<i64>,
}

impl PacketQueue {
    pub(crate) fn new() -> Self {
        Self {
            packets: Default::default(),
            retained_bytes: 0,
            first_pts_ms: None,
            last_pts_ms: None,
        }
    }

    pub(crate) fn clear(&mut self) {
        self.packets.clear();
        self.retained_bytes = 0;
        self.first_pts_ms = None;
        self.last_pts_ms = None;
    }

    pub(crate) fn push_back(&mut self, packet: &Packet) {
        let info = PacketInfo {
            size: packet.inner.size(),
            pts_ms: packet.pts_ms,
        };
        self.retained_bytes += info.size;
        if self.first_pts_ms.is_none() {
            self.first_pts_ms = info.pts_ms;
        }
        self.last_pts_ms = info.pts_ms.or(self.last_pts_ms);
        self.packets.push_back(info);

        while self.packets.len() > MAX_PACKET_STATS_LEN {
            if let Some(info) = self.packets.pop_front() {
                self.retained_bytes = self.retained_bytes.saturating_sub(info.size);
            }
        }
        self.first_pts_ms = self.packets.front().and_then(|info| info.pts_ms);
    }

    pub(crate) fn stats(&self) -> PacketQueueStats {
        PacketQueueStats {
            len: self.packets.len(),
            cached_bytes: self.retained_bytes,
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
