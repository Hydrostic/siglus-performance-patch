use std::{collections::VecDeque, mem};

use yuv::{YuvPlanarImage, YuvRange, YuvStandardMatrix, yuv444_to_bgra};

use crate::omv_types::OmvVideoFormat;

const MAX_FRAME_QUEUE_LEN: usize = 8;
const MAX_FRAME_QUEUE_BYTES: usize = 96 * 1024 * 1024;

pub struct FrameQueue {
    frames: VecDeque<Frame>,
    cached_bytes: usize,
    first_pts_ms: Option<i64>,
    last_pts_ms: Option<i64>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FrameQueueStats {
    pub len: usize,
    pub cached_bytes: usize,
    pub first_pts_ms: Option<i64>,
    pub last_pts_ms: Option<i64>,
    pub next_pts_ms: Option<i64>,
}

#[derive(Clone)]
pub struct Frame {
    pub inner: Vec<u8>,
    pub pts_ms: Option<i64>,
    pub duration_ms: Option<i64>,
}

impl FrameQueue {
    pub(crate) fn new() -> Self {
        Self {
            frames: VecDeque::new(),
            cached_bytes: 0,
            first_pts_ms: None,
            last_pts_ms: None,
        }
    }

    pub(crate) fn clear(&mut self) {
        self.frames.clear();
        self.cached_bytes = 0;
        self.first_pts_ms = None;
        self.last_pts_ms = None;
    }

    pub(crate) fn push_back(&mut self, frame: Frame) {
        self.cached_bytes += frame.inner.len();
        if self.first_pts_ms.is_none() {
            self.first_pts_ms = frame.pts_ms;
        }
        self.last_pts_ms = frame.pts_ms.or(self.last_pts_ms);
        self.frames.push_back(frame);
    }

    pub(crate) fn is_full(&self) -> bool {
        self.frames.len() >= MAX_FRAME_QUEUE_LEN || self.cached_bytes >= MAX_FRAME_QUEUE_BYTES
    }

    pub(crate) fn first_pts_ms(&self) -> Option<i64> {
        self.first_pts_ms
    }

    pub(crate) fn last_pts_ms(&self) -> Option<i64> {
        self.last_pts_ms
    }

    pub(crate) fn next_pts_ms(&self) -> Option<i64> {
        self.frames.front().and_then(|frame| frame.pts_ms)
    }

    pub(crate) fn stats(&self) -> FrameQueueStats {
        FrameQueueStats {
            len: self.frames.len(),
            cached_bytes: self.cached_bytes,
            first_pts_ms: self.first_pts_ms,
            last_pts_ms: self.last_pts_ms,
            next_pts_ms: self.next_pts_ms(),
        }
    }

    pub(crate) fn pop_frame_for(&mut self, target_ms: i64, threshold_ms: i64) -> Option<Frame> {
        while self
            .frames
            .front()
            .is_some_and(|frame| frame_is_before(frame, target_ms, threshold_ms))
        {
            self.cached_bytes = self
                .cached_bytes
                .saturating_sub(self.frames.front().map_or(0, |frame| frame.inner.len()));
            self.frames.pop_front();
        }

        let should_pop = self
            .frames
            .front()
            .is_some_and(|frame| frame_matches(frame, target_ms, threshold_ms));
        let frame = should_pop.then(|| {
            let frame = self.frames.pop_front().expect("front frame exists");
            self.cached_bytes = self.cached_bytes.saturating_sub(frame.inner.len());
            frame
        });
        self.first_pts_ms = self.frames.front().and_then(|frame| frame.pts_ms);
        self.last_pts_ms = self.frames.back().and_then(|frame| frame.pts_ms);
        frame
    }
}

pub(crate) fn frame_matches(frame: &Frame, target_ms: i64, threshold_ms: i64) -> bool {
    let Some(pts_ms) = frame.pts_ms else {
        return false;
    };
    let duration_ms = frame.duration_ms.unwrap_or(threshold_ms).max(0);
    target_ms >= pts_ms.saturating_sub(threshold_ms)
        && target_ms
            <= pts_ms
                .saturating_add(duration_ms)
                .saturating_add(threshold_ms)
}

fn frame_is_before(frame: &Frame, target_ms: i64, threshold_ms: i64) -> bool {
    let Some(pts_ms) = frame.pts_ms else {
        return true;
    };
    let duration_ms = frame.duration_ms.unwrap_or(0).max(0);
    pts_ms.saturating_add(duration_ms) < target_ms.saturating_sub(threshold_ms)
}

fn vec32_to_vec8(vec32: Vec<u32>) -> Vec<u8> {
    unsafe {
        let ratio = mem::size_of::<u32>() / mem::size_of::<u8>();

        let length = vec32.len() * ratio;
        let capacity = vec32.capacity() * ratio;
        let ptr = vec32.as_ptr() as *const u8;

        // Don't run the destructor for vec32
        mem::forget(vec32);

        // Construct new Vec
        Vec::from_raw_parts(ptr as *mut u8, length, capacity)
    }
}
fn convert_yuv444_to_bgra(video_frame: &ffmpeg_next::util::frame::Video) -> Vec<u8> {
    let width = video_frame.width() as usize;
    let height = video_frame.height() as usize;
    let mut bgra_data = vec![0u8; width * height * 4];
    let y_plane = video_frame.data(0);
    let u_plane = video_frame.data(1);
    let v_plane = video_frame.data(2);
    let yuv_image = YuvPlanarImage {
        y_plane,
        y_stride: video_frame.stride(0) as u32,
        u_plane,
        u_stride: video_frame.stride(1) as u32,
        v_plane,
        v_stride: video_frame.stride(2) as u32,
        width: width as u32,
        height: height as u32,
    };
    _ = yuv444_to_bgra(
        &yuv_image,
        &mut bgra_data,
        (width * 4) as u32,
        YuvRange::Limited,
        YuvStandardMatrix::Bt709,
    );
    bgra_data
}
fn convert_fake_rgb_to_bgra(video_frame: &ffmpeg_next::util::frame::Video) -> Vec<u8> {
    let width = video_frame.width() as usize;
    let height = video_frame.height() as usize;
    let mut bgra_data = vec![0u32; width * height];
    let r_plane = video_frame.data(0);
    let g_plane = video_frame.data(1);
    let b_plane = video_frame.data(2);
    let r_stride = video_frame.stride(0) as usize;
    let g_stride = video_frame.stride(1) as usize;
    let b_stride = video_frame.stride(2) as usize;
    for y in 0..height {
        let r_row = &r_plane[y * r_stride..][..width];
        let g_row = &g_plane[y * g_stride..][..width];
        let b_row = &b_plane[y * b_stride..][..width];
        let bgra_row = &mut bgra_data[y * width..][..width];
        for x in 0..width {
            unsafe {
                let r = *r_row.get_unchecked(x);
                let g = *g_row.get_unchecked(x);
                let b = *b_row.get_unchecked(x);
                *bgra_row.get_unchecked_mut(x) =
                    (0xFF << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
            }
        }
    }
    vec32_to_vec8(bgra_data)
}
#[inline]
#[target_feature(enable = "avx2")]
fn rgba_to_bgra_loop(
    alpha_h: usize,
    real_width: usize,
    alpha_plane: &[u8],
    alpha_stride: usize,
    r_plane: &[u8],
    r_stride: usize,
    g_plane: &[u8],
    g_stride: usize,
    b_plane: &[u8],
    b_stride: usize,
    bgra_data: &mut [u32],
) {
    use raw_cpuid::CpuId;

    let has_avx2 = CpuId::new()
        .get_extended_feature_info()
        .map_or(false, |finfo| finfo.has_avx2());
    if has_avx2 {
        for y in 0..alpha_h {
            let alpha_row = &alpha_plane[y * alpha_stride..][..real_width];
            let r_row = &r_plane[y * r_stride..][..real_width];
            let g_row = &g_plane[y * g_stride..][..real_width];
            let b_row = &b_plane[y * b_stride..][..real_width];
            let bgra_row = &mut bgra_data[y * real_width..][..real_width];
            for x in (0..real_width / 32 * 32).step_by(32) {
                unsafe {
                    let r_ptr = r_row.as_ptr().add(x);
                    let g_ptr = g_row.as_ptr().add(x);
                    let b_ptr = b_row.as_ptr().add(x);
                    let a_ptr = alpha_row.as_ptr().add(x);
                    let dest_ptr = bgra_row.as_mut_ptr().add(x);

                    std::arch::asm!(
                        // Load B/G first and start the byte unpack work.
                        "vmovdqu {ymm0}, ymmword ptr [{b_ptr}]",
                        "vmovdqu {ymm1}, ymmword ptr [{g_ptr}]",
                        "vpunpcklbw {ymm4}, {ymm0}, {ymm1}",
                        "vpunpckhbw {ymm5}, {ymm0}, {ymm1}",

                        // Load R/A while the shuffle unit is busy with B/G.
                        "vmovdqu {ymm2}, ymmword ptr [{r_ptr}]",
                        "vmovdqu {ymm3}, ymmword ptr [{a_ptr}]",
                        "vpunpcklbw {ymm6}, {ymm2}, {ymm3}",
                        "vpunpckhbw {ymm7}, {ymm2}, {ymm3}",

                        // Interleave the low half and store pixels 0..7 and 16..23.
                        "vpunpcklwd {ymm0}, {ymm4}, {ymm6}",
                        "vpunpckhwd {ymm1}, {ymm4}, {ymm6}",
                        "vperm2i128 {ymm2}, {ymm0}, {ymm1}, 0x20",
                        "vperm2i128 {ymm3}, {ymm0}, {ymm1}, 0x31",
                        "vmovdqu ymmword ptr [{dest}], {ymm2}",
                        "vmovdqu ymmword ptr [{dest} + 64], {ymm3}",

                        // Interleave the high half and store pixels 8..15 and 24..31.
                        "vpunpcklwd {ymm0}, {ymm5}, {ymm7}",
                        "vpunpckhwd {ymm1}, {ymm5}, {ymm7}",
                        "vperm2i128 {ymm2}, {ymm0}, {ymm1}, 0x20",
                        "vperm2i128 {ymm3}, {ymm0}, {ymm1}, 0x31",
                        "vmovdqu ymmword ptr [{dest} + 32], {ymm2}",
                        "vmovdqu ymmword ptr [{dest} + 96], {ymm3}",

                        r_ptr = in(reg) r_ptr,
                        g_ptr = in(reg) g_ptr,
                        b_ptr = in(reg) b_ptr,
                        a_ptr = in(reg) a_ptr,
                        dest = in(reg) dest_ptr,
                        ymm0 = out(ymm_reg) _,
                        ymm1 = out(ymm_reg) _,
                        ymm2 = out(ymm_reg) _,
                        ymm3 = out(ymm_reg) _,
                        ymm4 = out(ymm_reg) _,
                        ymm5 = out(ymm_reg) _,
                        ymm6 = out(ymm_reg) _,
                        ymm7 = out(ymm_reg) _,
                    );
                }
            }
            for x in (real_width / 32 * 32)..real_width {
                unsafe {
                    let r = *r_row.get_unchecked(x);
                    let g = *g_row.get_unchecked(x);
                    let b = *b_row.get_unchecked(x);
                    let a = *alpha_row.get_unchecked(x);
                    *bgra_row.get_unchecked_mut(x) =
                        ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
                }
            }
        }
    } else {
        for y in 0..alpha_h {
            let alpha_row = &alpha_plane[y * alpha_stride..][..real_width];
            let r_row = &r_plane[y * r_stride..][..real_width];
            let g_row = &g_plane[y * g_stride..][..real_width];
            let b_row = &b_plane[y * b_stride..][..real_width];
            let bgra_row = &mut bgra_data[y * real_width..][..real_width];
            for x in 0..real_width {
                unsafe {
                    let r = *r_row.get_unchecked(x);
                    let g = *g_row.get_unchecked(x);
                    let b = *b_row.get_unchecked(x);
                    let a = *alpha_row.get_unchecked(x);
                    *bgra_row.get_unchecked_mut(x) =
                        ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
                }
            }
        }
    }
}
fn convert_fake_rgba_to_bgra(
    video_frame: &ffmpeg_next::util::frame::Video,
    real_width: usize,
    real_height: usize,
) -> Vec<u8> {
    let mut bgra_data = vec![0u32; real_width * real_height];
    let r_plane = video_frame.data(0);
    let g_plane = video_frame.data(1);
    let b_plane = video_frame.data(2);
    let r_stride = video_frame.stride(0) as usize;
    let g_stride = video_frame.stride(1) as usize;
    let b_stride = video_frame.stride(2) as usize;
    let alpha0 = &r_plane[(video_frame.stride(0) * real_height) as usize..];
    let alpha1 = &g_plane[(video_frame.stride(1) * real_height) as usize..];
    let alpha2 = &b_plane[(video_frame.stride(2) * real_height) as usize..];
    let alpha_h = (real_height + 2) / 3;
    let alpha_h_2 = alpha_h * 2;
    unsafe {
        rgba_to_bgra_loop(
            alpha_h,
            real_width,
            alpha0,
            r_stride,
            r_plane,
            r_stride,
            g_plane,
            g_stride,
            b_plane,
            b_stride,
            &mut bgra_data,
        );
        rgba_to_bgra_loop(
            alpha_h,
            real_width,
            alpha1,
            g_stride,
            r_plane,
            r_stride,
            g_plane,
            g_stride,
            b_plane,
            b_stride,
            &mut bgra_data[alpha_h * real_width..],
        );
        rgba_to_bgra_loop(
            real_height - alpha_h_2,
            real_width,
            alpha2,
            b_stride,
            r_plane,
            r_stride,
            g_plane,
            g_stride,
            b_plane,
            b_stride,
            &mut bgra_data[alpha_h_2 * real_width..],
        );
    }
    vec32_to_vec8(bgra_data)
}
pub(crate) fn convert_frame_pix_fmt(
    video_frame: &ffmpeg_next::util::frame::Video,
    real_source_pix_fmt: OmvVideoFormat,
    real_height: usize,
    real_width: usize,
) -> Result<Vec<u8>, String> {
    let data = match real_source_pix_fmt {
        OmvVideoFormat::Rgb => convert_fake_rgb_to_bgra(video_frame),
        OmvVideoFormat::Rgba => convert_fake_rgba_to_bgra(video_frame, real_width, real_height),
        OmvVideoFormat::Yuv => convert_yuv444_to_bgra(video_frame),
        OmvVideoFormat::Unknown => return Err("Unknown real source pixel format".to_string()),
    };
    Ok(data)
}
