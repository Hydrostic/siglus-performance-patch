use std::{
    sync::{
        atomic::{AtomicBool, AtomicI64, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
};

use crate::omv_player::{
    decoder::Decoder,
    demuxer::Demuxer,
    looper::{LoopConfig, LoopQueue, LoopQueues},
    packet::Packet,
    player::PlaybackState,
};

pub struct WorkerHandle {
    join_handle: Option<JoinHandle<()>>,
    command_sender: crossbeam::channel::Sender<WorkerCommand>,
    shutdown: Arc<AtomicBool>,
}

pub struct Worker {
    decoder: Decoder,
    demuxer: Demuxer,
    playback_state: Arc<Mutex<PlaybackState>>,
    queues: Arc<Mutex<LoopQueues>>,
    playback_clock_ms: Arc<AtomicI64>,
    loop_config: Arc<LoopConfig>,
}

const MAX_CURRENT_PREFETCH_AHEAD_MS: i64 = 750;

pub enum WorkerCommand {
    Pause,
    Resume,
    // The timer reached the end of the loop segment, so wrap back to the loop start position.
    Wrap(crossbeam::channel::Sender<()>),
    Seek(i64),
    // If the timer is a little ahead of the last frame's pts, skip packets until the
    // decoder catches up. Ignore requests earlier than the last queued pts.
    Forward(i64),
    Shutdown,
}

impl Worker {
    pub(crate) fn new(
        decoder: Decoder,
        demuxer: Demuxer,
        playback_state: Arc<Mutex<PlaybackState>>,
        queues: Arc<Mutex<LoopQueues>>,
        playback_clock_ms: Arc<AtomicI64>,
        loop_config: Arc<LoopConfig>,
    ) -> Self {
        Self {
            decoder,
            demuxer,
            playback_state,
            queues,
            playback_clock_ms,
            loop_config,
        }
    }

    pub(crate) fn start(self) -> WorkerHandle {
        let (command_sender, command_receiver) = crossbeam::channel::unbounded();
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = shutdown.clone();
        let join_handle = std::thread::spawn(move || {
            worker_loop(self, command_receiver, thread_shutdown);
        });
        WorkerHandle {
            join_handle: Some(join_handle),
            command_sender,
            shutdown,
        }
    }
}

impl WorkerHandle {
    pub fn pause(&self) {
        self.send(WorkerCommand::Pause);
    }

    pub fn resume(&self) {
        self.send(WorkerCommand::Resume);
    }

    pub fn wrap(&self) {
        let (ack_sender, ack_receiver) = crossbeam::channel::bounded(1);
        if self
            .command_sender
            .send(WorkerCommand::Wrap(ack_sender))
            .is_ok()
        {
            ack_receiver.recv().ok();
        }
    }

    pub fn seek(&self, target_ms: i64) {
        self.send(WorkerCommand::Seek(target_ms));
    }

    pub fn forward(&self, target_ms: i64) {
        self.send(WorkerCommand::Forward(target_ms));
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.send(WorkerCommand::Shutdown);
    }

    fn send(&self, command: WorkerCommand) {
        self.command_sender.send(command).ok();
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.command_sender.send(WorkerCommand::Shutdown).ok();
        if let Some(join_handle) = self.join_handle.take() {
            join_handle.join().ok();
        }
    }
}

enum WorkerState {
    Playing,
    // Ffmpeg seeks to the nearest keyframe, not necessarily the target. In this
    // state packets before the requested timestamp are decoded but not presented.
    Seeking(i64),
    // Used when the render timer is ahead of the buffered data. Packets before
    // the requested timestamp are dropped to let the worker catch up quickly.
    Forwarding(i64),
    Idle(IdleReason),
    Preload(PreloadTarget),
}

#[derive(Clone, Copy)]
enum IdleReason {
    CurrentReady,
    NextLoopReady,
    Ended,
}

#[derive(Clone, Copy)]
enum PreloadTarget {
    Current,
    NextLoop,
}

impl WorkerState {
    fn should_wait_cmd(&self) -> bool {
        matches!(self, WorkerState::Idle(_))
    }

    fn should_process(&self) -> bool {
        !matches!(self, WorkerState::Idle(_))
    }
}

enum WorkerStep {
    Progress,
    NoWork,
    Ended,
}

fn worker_loop(
    mut worker: Worker,
    command_receiver: crossbeam::channel::Receiver<WorkerCommand>,
    shutdown: Arc<AtomicBool>,
) {
    let mut paused = false;
    let mut worker_state = WorkerState::Playing;
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        let command = if worker_state.should_wait_cmd() {
            match command_receiver.recv() {
                Ok(command) => Some(command),
                Err(_) => break,
            }
        } else {
            match command_receiver.try_recv() {
                Ok(command) => Some(command),
                Err(crossbeam::channel::TryRecvError::Disconnected) => break,
                Err(crossbeam::channel::TryRecvError::Empty) => None,
            }
        };

        if let Some(command) = command {
            if !handle_command(&mut worker, &mut worker_state, &mut paused, command) {
                break;
            }
        }

        if worker_state.should_process() {
            match worker_process(&mut worker, &mut worker_state) {
                WorkerStep::Progress => {}
                WorkerStep::NoWork => {
                    worker_state = match worker_state {
                        WorkerState::Preload(PreloadTarget::NextLoop) => {
                            WorkerState::Idle(IdleReason::NextLoopReady)
                        }
                        WorkerState::Preload(PreloadTarget::Current) => {
                            WorkerState::Idle(IdleReason::CurrentReady)
                        }
                        _ => WorkerState::Idle(IdleReason::CurrentReady),
                    }
                }
                WorkerStep::Ended => {
                    set_playback_state(&worker, PlaybackState::Ended);
                    worker_state = WorkerState::Idle(IdleReason::Ended);
                }
            }
        }
    }
}

fn handle_command(
    worker: &mut Worker,
    worker_state: &mut WorkerState,
    paused: &mut bool,
    command: WorkerCommand,
) -> bool {
    match command {
        WorkerCommand::Pause => {
            if !*paused {
                *paused = true;
                set_playback_state(worker, PlaybackState::Paused);
                if matches!(
                    worker_state,
                    WorkerState::Playing | WorkerState::Idle(IdleReason::CurrentReady)
                ) {
                    *worker_state = WorkerState::Preload(PreloadTarget::Current);
                }
            }
        }
        WorkerCommand::Resume => {
            if *paused {
                *paused = false;
            }
            set_playback_state(worker, PlaybackState::Playing);
            if matches!(
                worker_state,
                WorkerState::Idle(IdleReason::CurrentReady)
                    | WorkerState::Preload(PreloadTarget::Current)
            ) {
                *worker_state = WorkerState::Playing;
            }
        }
        WorkerCommand::Wrap(ack_sender) => {
            handle_wrap(worker, worker_state);
            ack_sender.send(()).ok();
        }
        WorkerCommand::Seek(target_ms) => {
            if handle_seek(worker, target_ms) {
                *worker_state = WorkerState::Seeking(target_ms.max(0));
            } else {
                *worker_state = WorkerState::Idle(IdleReason::CurrentReady);
            }
        }
        WorkerCommand::Forward(target_ms) => {
            handle_forward(worker, worker_state, target_ms);
        }
        WorkerCommand::Shutdown => return false,
    }
    true
}

fn worker_process(worker: &mut Worker, worker_state: &mut WorkerState) -> WorkerStep {
    if target_queue_is_full(worker, worker_state) {
        return WorkerStep::NoWork;
    }

    let Some(mut packet) = worker.demuxer.next_packet() else {
        if worker.loop_config.is_enabled() {
            handle_preload_next_loop(worker, worker_state);
            return WorkerStep::Progress;
        }
        return WorkerStep::Ended;
    };

    match *worker_state {
        WorkerState::Playing => queue_packet(worker, packet),
        WorkerState::Preload(target) => match target {
            PreloadTarget::Current => queue_packet(worker, packet),
            PreloadTarget::NextLoop => queue_next_loop_packet(worker, packet),
        },
        WorkerState::Seeking(target_ms) => {
            if packet_reached(&packet, target_ms) {
                packet.set_presentable(true);
                *worker_state = if is_playback_paused(worker) {
                    WorkerState::Preload(PreloadTarget::Current)
                } else {
                    WorkerState::Playing
                };
            } else {
                packet.set_presentable(false);
            }
            queue_packet(worker, packet)
        }
        WorkerState::Forwarding(target_ms) => {
            if packet_reached(&packet, target_ms) {
                packet.set_presentable(true);
                *worker_state = if is_playback_paused(worker) {
                    WorkerState::Preload(PreloadTarget::Current)
                } else {
                    WorkerState::Playing
                };
                queue_packet(worker, packet)
            } else {
                WorkerStep::Progress
            }
        }
        WorkerState::Idle(_) => WorkerStep::NoWork,
    }
}

fn target_queue_is_full(worker: &Worker, worker_state: &WorkerState) -> bool {
    worker
        .queues
        .lock()
        .map(|queues| match worker_state {
            WorkerState::Preload(PreloadTarget::NextLoop) => {
                queues.next.as_ref().is_some_and(LoopQueue::is_full)
            }
            WorkerState::Playing
            | WorkerState::Preload(PreloadTarget::Current)
            | WorkerState::Seeking(_)
            | WorkerState::Forwarding(_) => {
                let clock_ms = worker.playback_clock_ms.load(Ordering::Relaxed);
                queues.current.frames.last_pts_ms().is_some_and(|last_pts_ms| {
                    last_pts_ms > clock_ms.saturating_add(MAX_CURRENT_PREFETCH_AHEAD_MS)
                }) || queues.current.is_full()
            }
            _ => queues.current.is_full(),
        })
        .unwrap_or(true)
}

fn handle_preload_next_loop(worker: &mut Worker, worker_state: &mut WorkerState) {
    if !worker.loop_config.is_enabled() {
        *worker_state = WorkerState::Idle(IdleReason::Ended);
        return;
    }

    if seek_for_next_loop_preload(worker) {
        *worker_state = WorkerState::Preload(PreloadTarget::NextLoop);
    } else {
        *worker_state = WorkerState::Idle(IdleReason::CurrentReady);
    }
}

fn handle_wrap(worker: &mut Worker, worker_state: &mut WorkerState) {
    if !worker.loop_config.is_enabled() {
        *worker_state = WorkerState::Idle(IdleReason::CurrentReady);
        return;
    }

    let switched_to_preloaded_queue = worker
        .queues
        .lock()
        .map(|mut queues| {
            let has_preloaded_queue = queues.next.is_some();
            queues.switch();
            has_preloaded_queue
        })
        .unwrap_or(false);

    if switched_to_preloaded_queue {
        *worker_state = WorkerState::Playing;
    } else if handle_seek(worker, 0) {
        *worker_state = WorkerState::Seeking(0);
    } else {
        *worker_state = WorkerState::Idle(IdleReason::CurrentReady);
    }
}

fn handle_forward(worker: &mut Worker, worker_state: &mut WorkerState, target_ms: i64) {
    let target_ms = target_ms.max(0);
    if matches!(worker_state, WorkerState::Seeking(_)) {
        return;
    }

    if let Some(last_pts_ms) = last_queued_pts_ms(worker) {
        if target_ms <= last_pts_ms {
            return;
        }
    }

    match worker_state {
        WorkerState::Forwarding(current_target_ms) => {
            *current_target_ms = (*current_target_ms).max(target_ms);
        }
        WorkerState::Preload(PreloadTarget::NextLoop) => {}
        _ => {
            *worker_state = WorkerState::Forwarding(target_ms);
        }
    }
}

fn handle_seek(worker: &mut Worker, target_ms: i64) -> bool {
    let seek_ok = worker.demuxer.seek(target_ms.max(0));
    if seek_ok {
        worker.decoder.flush();
        clear_current_queue(worker);
    }
    seek_ok
}

fn seek_for_next_loop_preload(worker: &mut Worker) -> bool {
    let seek_ok = worker.demuxer.seek(0);
    if seek_ok {
        worker.decoder.flush();
        clear_next_loop_queue(worker);
    }
    seek_ok
}

fn queue_packet(worker: &mut Worker, packet: Packet) -> WorkerStep {
    let frames = match worker.decoder.decode_packet(&packet) {
        Ok(frames) => frames,
        Err(_) => return WorkerStep::NoWork,
    };

    match worker.queues.lock() {
        Ok(mut queues) => {
            queues.current.packets.push_back(&packet);
            for frame in frames {
                queues.current.frames.push_back(frame);
            }
            WorkerStep::Progress
        }
        Err(_) => WorkerStep::NoWork,
    }
}

fn queue_next_loop_packet(worker: &mut Worker, packet: Packet) -> WorkerStep {
    let frames = match worker.decoder.decode_packet(&packet) {
        Ok(frames) => frames,
        Err(_) => return WorkerStep::NoWork,
    };

    match worker.queues.lock() {
        Ok(mut queues) => {
            if queues.next.is_none() {
                queues.next = Some(LoopQueue::new());
            }
            if let Some(next) = &mut queues.next {
                next.packets.push_back(&packet);
                for frame in frames {
                    next.frames.push_back(frame);
                }
                WorkerStep::Progress
            } else {
                WorkerStep::NoWork
            }
        }
        Err(_) => WorkerStep::NoWork,
    }
}

fn clear_current_queue(worker: &mut Worker) {
    if let Ok(mut queues) = worker.queues.lock() {
        queues.current.clear();
    }
}

fn clear_next_loop_queue(worker: &mut Worker) {
    if let Ok(mut queues) = worker.queues.lock() {
        if let Some(next) = &mut queues.next {
            next.clear();
        }
    }
}

fn last_queued_pts_ms(worker: &Worker) -> Option<i64> {
    worker
        .queues
        .lock()
        .ok()
        .and_then(|queues| queues.current.last_pts_ms())
}

fn packet_reached(packet: &Packet, target_ms: i64) -> bool {
    packet.pts_ms.is_some_and(|pts_ms| pts_ms >= target_ms)
}

fn set_playback_state(worker: &Worker, playback_state: PlaybackState) {
    if let Ok(mut state) = worker.playback_state.lock() {
        *state = playback_state;
    }
}

fn is_playback_paused(worker: &Worker) -> bool {
    worker
        .playback_state
        .lock()
        .is_ok_and(|state| matches!(*state, PlaybackState::Paused))
}
