mod clock;
mod decoder;
mod demuxer;
mod frame;
mod looper;
mod overlay;
mod packet;
mod player;
mod worker;

use std::{
    ffi::CString,
    path::PathBuf,
    sync::{
        atomic::AtomicI64,
        Arc, Mutex, Once, OnceLock,
    },
};

use ffmpeg_next::{self as ffmpeg, Rescale};

use crate::omv_types::{OmvVideoFormat, OmvVideoInfo};

use self::{
    clock::MICROSECOND_RATIONAL,
    decoder::Decoder,
    demuxer::Demuxer,
    looper::{LoopConfig, LoopQueues},
    player::{PlaybackState, Player},
    worker::Worker,
};

static INIT_FFMPEG: Once = Once::new();

pub struct OmvPlayer {
    inner: OnceLock<OmvPlayerInner>,
    last_error: Mutex<String>,
}

struct OmvPlayerInner {
    player: Player,
    video_info: OmvVideoInfo,
    duration_ms: i64,
    loop_config: Arc<LoopConfig>,
}

impl OmvPlayer {
    pub fn new_boxed() -> Box<Self> {
        Box::new(Self {
            inner: OnceLock::new(),
            last_error: Mutex::new(String::new()),
        })
    }

    pub fn init(&self, path: &str, loop_enabled: bool) -> bool {
        if self.inner.get().is_some() {
            self.set_last_error("player is already initialized");
            return false;
        }

        INIT_FFMPEG.call_once(|| {
            if let Err(error) = ffmpeg::init() {
                crate::debug_log(&format!("ffmpeg init failed: {error}"));
            }
        });

        match OmvPlayerInner::open(path, loop_enabled) {
            Ok(inner) => {
                if self.inner.set(inner).is_ok() {
                    self.set_last_error("");
                    true
                } else {
                    self.set_last_error("player is already initialized");
                    false
                }
            }
            Err(error) => {
                self.set_last_error(&error);
                false
            }
        }
    }

    pub fn get_size(&self) -> (u32, u32) {
        self.inner
            .get()
            .map(|inner| {
                (
                    inner.video_info.display_width,
                    inner.video_info.display_height,
                )
            })
            .unwrap_or((0, 0))
    }

    pub fn check_need_update(&self, time_ms: i64) -> bool {
        self.check_need_update_at(time_ms, false)
    }

    pub fn check_need_update_at(&self, time_ms: i64, update_by_force: bool) -> bool {
        self.inner
            .get()
            .is_some_and(|inner| inner.player.check_need_update_at(time_ms, update_by_force))
    }

    pub fn end_loop(&self) {
        if let Some(inner) = self.inner.get() {
            inner.loop_config.disable();
        }
    }

    pub fn get_total_time(&self) -> i64 {
        self.inner.get().map_or(0, |inner| inner.duration_ms)
    }

    pub fn fill_buffer_at(&self, time_ms: i64, buffer: *mut u8, pitch: i32) -> bool {
        let Some(inner) = self.inner.get() else {
            self.set_last_error("player is not initialized");
            return false;
        };

        match unsafe { inner.player.fill_buffer(time_ms, buffer, pitch) } {
            Ok(result) => {
                self.set_last_error("");
                result
            }
            Err(error) => {
                self.set_last_error(&error);
                false
            }
        }
    }

    pub fn is_rgb(&self) -> bool {
        self.inner
            .get()
            .is_some_and(|inner| matches!(inner.video_info.format, OmvVideoFormat::Rgb))
    }

    pub fn is_playing(&self) -> bool {
        self.inner
            .get()
            .is_some_and(|inner| inner.player.is_playing())
    }

    pub fn seek(&self, target_ms: i64) -> bool {
        let Some(inner) = self.inner.get() else {
            self.set_last_error("player is not initialized");
            return false;
        };
        inner.player.seek(target_ms);
        true
    }

    pub fn last_error(&self) -> String {
        self.last_error.lock().unwrap().clone()
    }

    fn set_last_error(&self, error: &str) {
        *self.last_error.lock().unwrap() = error.to_string();
    }
}

impl OmvPlayerInner {
    fn open(path: &str, loop_enabled: bool) -> Result<Self, String> {
        let path = PathBuf::from(path);
        let video_info = OmvVideoInfo::from_path(&path)?;
        let mut options = ffmpeg::Dictionary::new();
        options.set("skip_initial_bytes", &video_info.payload_offset.to_string());

        let format = input_format("ogg")?;
        let input = ffmpeg::format::open_with(&path, &format, options)
            .map_err(|error| format!("open video input failed: {error}"))?
            .input();
        let stream = input
            .streams()
            .best(ffmpeg::media::Type::Video)
            .ok_or_else(|| "no video stream found".to_string())?;
        let duration_ms = stream
            .duration()
            .rescale(stream.time_base(), MICROSECOND_RATIONAL);
        let stream_index = stream.index();
        let stream_time_base = stream.time_base();

        let decoder = ffmpeg::codec::Context::from_parameters(stream.parameters())
            .map_err(|error| format!("create decoder context failed: {error}"))?
            .decoder()
            .video()
            .map_err(|error| format!("open video decoder failed: {error}"))?;
        validate_decoder_size(&decoder, video_info)?;

        let queues = Arc::new(Mutex::new(LoopQueues::new()));
        let playback_state = Arc::new(Mutex::new(PlaybackState::Playing));
        let playback_clock_ms = Arc::new(AtomicI64::new(0));
        let loop_config = Arc::new(LoopConfig::new(loop_enabled, duration_ms));
        let demuxer = Demuxer::new(input, stream_index, stream_time_base);
        let decoder = Decoder::new(decoder, stream_time_base, video_info);
        let worker_handle = Worker::new(
            decoder,
            demuxer,
            playback_state.clone(),
            queues.clone(),
            playback_clock_ms.clone(),
            loop_config.clone(),
        )
        .start();
        let player = Player::new(
            loop_config.clone(),
            queues,
            playback_clock_ms,
            playback_state,
            worker_handle,
            video_info.display_width,
            video_info.display_height,
        );

        Ok(Self {
            player,
            video_info,
            duration_ms,
            loop_config,
        })
    }
}

fn input_format(name: &str) -> Result<ffmpeg::Format, String> {
    let name = CString::new(name).map_err(|_| "input format name contains nul".to_string())?;
    let format = unsafe { ffmpeg::ffi::av_find_input_format(name.as_ptr()) };

    if format.is_null() {
        return Err(format!(
            "ffmpeg input format not found: {}",
            name.to_string_lossy()
        ));
    }

    Ok(ffmpeg::Format::Input(unsafe {
        ffmpeg::format::format::Input::wrap(format.cast_mut())
    }))
}

fn validate_decoder_size(
    decoder: &ffmpeg::decoder::Video,
    video_info: OmvVideoInfo,
) -> Result<(), String> {
    let decoded_width = decoder.width();
    let decoded_height = decoder.height();
    if decoded_width != video_info.display_width {
        return Err(format!(
            "decoded width mismatch decoded={} display={}",
            decoded_width, video_info.display_width
        ));
    }

    let expected_min_height = match video_info.format {
        OmvVideoFormat::Rgba => video_info
            .display_height
            .checked_add(video_info.display_height.div_ceil(3))
            .ok_or_else(|| "rgba expected decoded height overflow".to_string())?,
        _ => video_info.display_height,
    };
    if decoded_height < expected_min_height {
        return Err(format!(
            "decoded height too small decoded={} expected_at_least={}",
            decoded_height, expected_min_height
        ));
    }

    Ok(())
}
