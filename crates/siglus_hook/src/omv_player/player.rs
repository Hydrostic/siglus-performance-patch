use std::{
    ptr,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc, Mutex,
    },
};

use crate::omv_player::{
    clock::{ClockEvent, ClockTracker},
    frame::{Frame, frame_matches},
    looper::{LoopConfig, LoopQueues},
    overlay::{ENABLE_STATS_WINDOW, OverlayStats, StatsWindow},
    worker::WorkerHandle,
};

const FRAME_MATCH_THRESHOLD_MS: i64 = 10;
const MAX_CURRENT_PREFETCH_AHEAD_MS: i64 = 750;

pub struct Player {
    clock: Mutex<ClockTracker>,
    loop_config: Arc<LoopConfig>,
    queues: Arc<Mutex<LoopQueues>>,
    playback_clock_ms: Arc<AtomicI64>,
    last_frame: Mutex<Option<Frame>>,
    playback_state: Arc<Mutex<PlaybackState>>,
    worker_handle: WorkerHandle,
    stats_window: StatsWindow,
    display_width: u32,
    display_height: u32,
}

impl Player {
    pub fn new(
        loop_config: Arc<LoopConfig>,
        queues: Arc<Mutex<LoopQueues>>,
        playback_clock_ms: Arc<AtomicI64>,
        playback_state: Arc<Mutex<PlaybackState>>,
        worker_handle: WorkerHandle,
        display_width: u32,
        display_height: u32,
    ) -> Self {
        Self {
            clock: Mutex::new(ClockTracker::new()),
            loop_config,
            queues,
            playback_clock_ms,
            last_frame: Mutex::new(None),
            playback_state,
            worker_handle,
            stats_window: StatsWindow::new(),
            display_width,
            display_height,
        }
    }

    pub fn play(&self) {
        self.worker_handle.resume();
    }

    pub fn pause(&self) {
        self.worker_handle.pause();
    }

    pub fn seek(&self, target_ms: i64) {
        let target_ms = target_ms.max(0);
        self.last_frame.lock().unwrap().take();
        if let Ok(mut queues) = self.queues.lock() {
            queues.current.clear();
        }
        self.worker_handle.seek(target_ms);
    }

    pub fn is_playing(&self) -> bool {
        matches!(*self.playback_state.lock().unwrap(), PlaybackState::Playing)
    }

    pub fn check_need_update(&self, time_ms: i64) -> bool {
        self.check_need_update_at(time_ms, false)
    }

    pub fn check_need_update_at(&self, time_ms: i64, update_by_force: bool) -> bool {
        if !matches!(*self.playback_state.lock().unwrap(), PlaybackState::Playing) {
            return false;
        }

        let time_ms = time_ms.max(0);
        self.playback_clock_ms.store(time_ms, Ordering::Relaxed);
        self.chase_to(time_ms);

        if update_by_force {
            return true;
        }

        let Some(last_frame) = self.last_frame.lock().unwrap().as_ref().cloned() else {
            return true;
        };
        !frame_matches(&last_frame, time_ms, FRAME_MATCH_THRESHOLD_MS)
    }

    pub unsafe fn fill_buffer(
        &self,
        time_ms: i64,
        buffer: *mut u8,
        pitch: i32,
    ) -> Result<bool, String> {
        if buffer.is_null() {
            return Err("buffer is null".to_string());
        }
        if pitch <= 0 {
            return Err(format!("invalid pitch={pitch}"));
        }

        let time_ms = time_ms.max(0);
        self.playback_clock_ms.store(time_ms, Ordering::Relaxed);
        self.chase_to(time_ms);

        let frame = self
            .pop_frame(time_ms)
            .or_else(|| self.last_frame.lock().unwrap().as_ref().cloned());

        let Some(frame) = frame else {
            self.clear_buffer(buffer, pitch)?;
            self.update_stats_window(time_ms, pitch, None, None)?;
            return Ok(false);
        };

        let frame_pts_ms = frame.pts_ms;
        let frame_duration_ms = frame.duration_ms;
        self.copy_frame_to_buffer(&frame, buffer, pitch)?;
        *self.last_frame.lock().unwrap() = Some(frame);
        self.update_stats_window(time_ms, pitch, frame_pts_ms, frame_duration_ms)?;
        Ok(true)
    }

    fn chase_to(&self, time_ms: i64) {
        let event = self
            .clock
            .lock()
            .unwrap()
            .update(time_ms, &self.loop_config);
        match event {
            ClockEvent::FirstSample => self.worker_handle.forward(time_ms),
            ClockEvent::Advanced => {
                if self.needs_forward(time_ms) {
                    self.worker_handle.forward(time_ms);
                }
            }
            ClockEvent::Wrapped => {
                self.last_frame.lock().unwrap().take();
                self.worker_handle.wrap();
            }
            ClockEvent::Seeked => {
                self.seek(time_ms);
            }
        }
    }

    fn needs_forward(&self, time_ms: i64) -> bool {
        let Ok(queues) = self.queues.lock() else {
            return false;
        };
        let frame_stats = queues.current.frames.stats();
        if frame_stats.len == 0 {
            return true;
        }
        if frame_stats
            .first_pts_ms
            .is_some_and(|first_pts_ms| {
                first_pts_ms > time_ms.saturating_add(MAX_CURRENT_PREFETCH_AHEAD_MS)
            })
        {
            return false;
        }
        frame_stats.last_pts_ms.is_none_or(|last_pts_ms| {
            last_pts_ms < time_ms.saturating_add(MAX_CURRENT_PREFETCH_AHEAD_MS / 2)
        })
    }

    fn pop_frame(&self, time_ms: i64) -> Option<Frame> {
        let mut should_seek_back = false;
        let frame = {
            let mut queues = self.queues.lock().ok()?;
            let frame = queues
                .current
                .frames
                .pop_frame_for(time_ms, FRAME_MATCH_THRESHOLD_MS);
            if frame.is_none() {
                let frame_stats = queues.current.frames.stats();
                if frame_stats.first_pts_ms.is_some_and(|first_pts_ms| {
                    first_pts_ms > time_ms.saturating_add(MAX_CURRENT_PREFETCH_AHEAD_MS)
                }) {
                    queues.current.clear();
                    should_seek_back = true;
                }
            }
            frame
        };

        if should_seek_back {
            self.worker_handle.seek(time_ms);
        }
        frame
    }

    fn copy_frame_to_buffer(
        &self,
        frame: &Frame,
        buffer: *mut u8,
        pitch: i32,
    ) -> Result<(), String> {
        let pitch = usize::try_from(pitch).map_err(|_| format!("invalid pitch={pitch}"))?;
        let row_bytes = self.row_bytes_count()?;
        if pitch < row_bytes {
            return Err(format!(
                "pitch too small pitch={pitch} row_bytes={row_bytes}"
            ));
        }

        let height = self.display_height as usize;
        let expected_len = row_bytes
            .checked_mul(height)
            .ok_or_else(|| "frame size overflow".to_string())?;
        if frame.inner.len() < expected_len {
            return Err(format!(
                "frame buffer too small len={} expected={expected_len}",
                frame.inner.len()
            ));
        }

        for y in 0..height {
            let source_start = y
                .checked_mul(row_bytes)
                .ok_or_else(|| "source row offset overflow".to_string())?;
            let source_end = source_start
                .checked_add(row_bytes)
                .ok_or_else(|| "source row end overflow".to_string())?;
            let destination = unsafe { buffer.add(y * pitch) };
            unsafe {
                ptr::copy_nonoverlapping(
                    frame.inner[source_start..source_end].as_ptr(),
                    destination,
                    row_bytes,
                );
            }
        }
        Ok(())
    }

    fn clear_buffer(&self, buffer: *mut u8, pitch: i32) -> Result<(), String> {
        let pitch = usize::try_from(pitch).map_err(|_| format!("invalid pitch={pitch}"))?;
        let height = self.display_height as usize;
        for y in 0..height {
            let row = unsafe { std::slice::from_raw_parts_mut(buffer.add(y * pitch), pitch) };
            row.fill(0);
        }
        Ok(())
    }

    fn update_stats_window(
        &self,
        time_ms: i64,
        pitch: i32,
        frame_pts_ms: Option<i64>,
        frame_duration_ms: Option<i64>,
    ) -> Result<(), String> {
        if !ENABLE_STATS_WINDOW {
            return Ok(());
        }

        let pitch = usize::try_from(pitch).map_err(|_| format!("invalid pitch={pitch}"))?;
        let queues = self.queues.lock().ok().map(|queues| queues.stats());
        let playback_state = self
            .playback_state
            .lock()
            .map(|state| state.as_str())
            .unwrap_or("locked");
        let stats = OverlayStats {
            target_ms: time_ms,
            playback_state,
            frame_pts_ms,
            frame_duration_ms,
            loop_enabled: self.loop_config.is_enabled(),
            loop_duration_ms: self.loop_config.loop_duration_ms,
            pitch,
            queues,
        };

        self.stats_window.update(&stats);
        Ok(())
    }

    fn row_bytes_count(&self) -> Result<usize, String> {
        (self.display_width as usize)
            .checked_mul(4)
            .ok_or_else(|| "row byte count overflow".to_string())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackState {
    Playing,
    Paused,
    Ended,
}

impl PlaybackState {
    fn as_str(self) -> &'static str {
        match self {
            PlaybackState::Playing => "Playing",
            PlaybackState::Paused => "Paused",
            PlaybackState::Ended => "Ended",
        }
    }
}
