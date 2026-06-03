use std::sync::atomic::{AtomicBool, Ordering};

use crate::omv_player::{
    frame::{FrameQueue, FrameQueueStats},
    packet::{PacketQueue, PacketQueueStats},
};

pub struct LoopConfig {
    pub enabled: AtomicBool,
    pub head_guard_ms: i64,
    pub tail_guard_ms: i64,
    pub loop_duration_ms: i64,
}

impl LoopConfig {
    pub fn new(enabled: bool, loop_duration_ms: i64) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            head_guard_ms: 100,
            tail_guard_ms: 100,
            loop_duration_ms,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn disable(&self) {
        self.enabled.store(false, Ordering::Relaxed);
    }
}

pub struct LoopQueues {
    pub current: LoopQueue,
    pub next: Option<LoopQueue>,
}

impl LoopQueues {
    pub fn new() -> Self {
        Self {
            current: LoopQueue::new(),
            next: None,
        }
    }

    pub fn switch(&mut self) {
        if let Some(next_queue) = self.next.take() {
            self.current = next_queue;
        } else {
            self.current = LoopQueue::new();
        }
    }

    pub(crate) fn stats(&self) -> LoopQueuesStats {
        LoopQueuesStats {
            current: self.current.stats(),
            next: self.next.as_ref().map(LoopQueue::stats),
        }
    }
}
pub struct LoopQueue {
    pub frames: FrameQueue,
    pub packets: PacketQueue,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct LoopQueuesStats {
    pub current: LoopQueueStats,
    pub next: Option<LoopQueueStats>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct LoopQueueStats {
    pub frames: FrameQueueStats,
    pub packets: PacketQueueStats,
}

impl LoopQueue {
    pub fn new() -> Self {
        Self {
            frames: FrameQueue::new(),
            packets: PacketQueue::new(),
        }
    }

    pub(crate) fn clear(&mut self) {
        self.frames.clear();
        self.packets.clear();
    }

    pub(crate) fn last_pts_ms(&self) -> Option<i64> {
        self.frames.last_pts_ms().or(self.packets.last_pts_ms)
    }

    pub(crate) fn is_full(&self) -> bool {
        self.frames.is_full()
    }

    pub(crate) fn stats(&self) -> LoopQueueStats {
        LoopQueueStats {
            frames: self.frames.stats(),
            packets: self.packets.stats(),
        }
    }
}
