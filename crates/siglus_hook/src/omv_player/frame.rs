use std::{
    collections::VecDeque,
    ops::{Deref, DerefMut},
    sync::{Arc, Mutex},
};

use bytemuck::{cast_slice, cast_slice_mut};
use yuv::{YuvPlanarImage, YuvRange, YuvStandardMatrix, yuv444_to_bgra};

use crate::omv_types::OmvVideoFormat;

const MAX_FRAME_QUEUE_LEN: usize = 8;
const MAX_FRAME_QUEUE_BYTES: usize = 96 * 1024 * 1024;
const MAX_POOLED_FRAME_BUFFERS: usize = MAX_FRAME_QUEUE_LEN * 2 + 4;

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

pub struct Frame {
    pub inner: PooledFrameBuffer,
    pub pts_ms: Option<i64>,
    pub duration_ms: Option<i64>,
}

#[derive(Clone)]
pub(crate) struct FramePool {
    inner: Arc<FramePoolInner>,
}

struct FramePoolInner {
    pixel_count: usize,
    max_cached: usize,
    buffers: Mutex<Vec<Vec<u32>>>,
}

pub struct PooledFrameBuffer {
    pixels: Option<Vec<u32>>,
    pool: Arc<FramePoolInner>,
}

impl FramePool {
    pub(crate) fn new(width: usize, height: usize) -> Result<Self, String> {
        let pixel_count = width
            .checked_mul(height)
            .ok_or_else(|| "frame pixel count overflow".to_string())?;
        Ok(Self {
            inner: Arc::new(FramePoolInner {
                pixel_count,
                max_cached: MAX_POOLED_FRAME_BUFFERS,
                buffers: Mutex::new(Vec::with_capacity(MAX_POOLED_FRAME_BUFFERS)),
            }),
        })
    }

    pub(crate) fn acquire(&self) -> PooledFrameBuffer {
        let mut pixels = self
            .inner
            .buffers
            .lock()
            .ok()
            .and_then(|mut buffers| buffers.pop())
            .unwrap_or_else(|| Vec::with_capacity(self.inner.pixel_count));
        pixels.resize(self.inner.pixel_count, 0);
        PooledFrameBuffer {
            pixels: Some(pixels),
            pool: Arc::clone(&self.inner),
        }
    }

    #[cfg(test)]
    fn cached_buffer_count(&self) -> usize {
        self.inner
            .buffers
            .lock()
            .map(|buffers| buffers.len())
            .unwrap_or(0)
    }
}

impl PooledFrameBuffer {
    pub(crate) fn len(&self) -> usize {
        self.pixels().len() * size_of::<u32>()
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        cast_slice(self.pixels())
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        cast_slice_mut(self.pixels_mut())
    }

    pub(crate) fn as_mut_pixels(&mut self) -> &mut [u32] {
        self.pixels_mut()
    }

    fn pixels(&self) -> &[u32] {
        self.pixels
            .as_ref()
            .expect("pooled frame buffer is present")
    }

    fn pixels_mut(&mut self) -> &mut [u32] {
        self.pixels
            .as_mut()
            .expect("pooled frame buffer is present")
    }
}

impl Deref for PooledFrameBuffer {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

impl DerefMut for PooledFrameBuffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.as_mut_slice()
    }
}

impl Drop for PooledFrameBuffer {
    fn drop(&mut self) {
        let Some(mut pixels) = self.pixels.take() else {
            return;
        };
        if pixels.capacity() < self.pool.pixel_count {
            return;
        }
        pixels.truncate(self.pool.pixel_count);
        if let Ok(mut buffers) = self.pool.buffers.lock() {
            if buffers.len() < self.pool.max_cached {
                buffers.push(pixels);
            }
        }
    }
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

    pub(crate) fn len(&self) -> usize {
        self.frames.len()
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
        let mut candidate = None;
        while self.frames.front().is_some_and(|frame| {
            frame_is_not_after_target(frame, target_ms, threshold_ms)
        }) {
            let frame = self.frames.pop_front().expect("front frame exists");
            self.cached_bytes = self.cached_bytes.saturating_sub(frame.inner.len());
            if frame.pts_ms.is_some() {
                candidate = Some(frame);
            }
        }

        self.first_pts_ms = self.frames.front().and_then(|frame| frame.pts_ms);
        self.last_pts_ms = self.frames.back().and_then(|frame| frame.pts_ms);
        candidate
    }

    pub(crate) fn trim_to_first_future_frame(&mut self, target_ms: i64, ahead_ms: i64) {
        if self.frames.len() <= 1 {
            return;
        }
        let Some(first_pts_ms) = self.first_pts_ms else {
            return;
        };
        if first_pts_ms <= target_ms.saturating_add(ahead_ms) {
            return;
        }

        while self.frames.len() > 1 {
            if let Some(frame) = self.frames.pop_back() {
                self.cached_bytes = self.cached_bytes.saturating_sub(frame.inner.len());
            }
        }
        self.first_pts_ms = self.frames.front().and_then(|frame| frame.pts_ms);
        self.last_pts_ms = self.frames.back().and_then(|frame| frame.pts_ms);
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

fn frame_is_not_after_target(frame: &Frame, target_ms: i64, threshold_ms: i64) -> bool {
    let Some(pts_ms) = frame.pts_ms else {
        return true;
    };
    pts_ms <= target_ms.saturating_add(threshold_ms)
}

fn convert_yuv444_to_bgra(
    video_frame: &ffmpeg_next::util::frame::Video,
    mut bgra_data: PooledFrameBuffer,
) -> PooledFrameBuffer {
    let width = video_frame.width() as usize;
    let height = video_frame.height() as usize;
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
        bgra_data.as_mut_slice(),
        (width * 4) as u32,
        YuvRange::Limited,
        YuvStandardMatrix::Bt709,
    );
    bgra_data
}
fn convert_fake_rgb_to_bgra(
    video_frame: &ffmpeg_next::util::frame::Video,
    mut bgra_data: PooledFrameBuffer,
) -> PooledFrameBuffer {
    let width = video_frame.width() as usize;
    let height = video_frame.height() as usize;
    let r_plane = video_frame.data(0);
    let g_plane = video_frame.data(1);
    let b_plane = video_frame.data(2);
    let r_stride = video_frame.stride(0) as usize;
    let g_stride = video_frame.stride(1) as usize;
    let b_stride = video_frame.stride(2) as usize;
    let bgra_pixels = bgra_data.as_mut_pixels();
    for y in 0..height {
        let r_row = &r_plane[y * r_stride..][..width];
        let g_row = &g_plane[y * g_stride..][..width];
        let b_row = &b_plane[y * b_stride..][..width];
        let bgra_row = &mut bgra_pixels[y * width..][..width];
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
    bgra_data
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
    mut bgra_data: PooledFrameBuffer,
) -> PooledFrameBuffer {
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
    let r_plane_1 = &r_plane[alpha_h * r_stride..];
    let g_plane_1 = &g_plane[alpha_h * g_stride..];
    let b_plane_1 = &b_plane[alpha_h * b_stride..];
    let r_plane_2 = &r_plane[alpha_h_2 * r_stride..];
    let g_plane_2 = &g_plane[alpha_h_2 * g_stride..];
    let b_plane_2 = &b_plane[alpha_h_2 * b_stride..];
    let bgra_pixels = bgra_data.as_mut_pixels();
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
            bgra_pixels,
        );
        rgba_to_bgra_loop(
            alpha_h,
            real_width,
            alpha1,
            g_stride,
            r_plane_1,
            r_stride,
            g_plane_1,
            g_stride,
            b_plane_1,
            b_stride,
            &mut bgra_pixels[alpha_h * real_width..],
        );
        rgba_to_bgra_loop(
            real_height - alpha_h_2,
            real_width,
            alpha2,
            b_stride,
            r_plane_2,
            r_stride,
            g_plane_2,
            g_stride,
            b_plane_2,
            b_stride,
            &mut bgra_pixels[alpha_h_2 * real_width..],
        );
    }
    bgra_data
}
pub(crate) fn convert_frame_pix_fmt(
    video_frame: &ffmpeg_next::util::frame::Video,
    real_source_pix_fmt: OmvVideoFormat,
    real_height: usize,
    real_width: usize,
    frame_pool: &FramePool,
) -> Result<PooledFrameBuffer, String> {
    let bgra_data = frame_pool.acquire();
    let data = match real_source_pix_fmt {
        OmvVideoFormat::Rgb => convert_fake_rgb_to_bgra(video_frame, bgra_data),
        OmvVideoFormat::Rgba => {
            convert_fake_rgba_to_bgra(video_frame, real_width, real_height, bgra_data)
        }
        OmvVideoFormat::Yuv => convert_yuv444_to_bgra(video_frame, bgra_data),
        OmvVideoFormat::Unknown => return Err("Unknown real source pixel format".to_string()),
    };
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_pool_reuses_returned_buffer() {
        let pool = FramePool::new(2, 2).unwrap();
        let first_ptr = {
            let mut buffer = pool.acquire();
            buffer.as_mut_slice().fill(0x7f);
            buffer.as_slice().as_ptr()
        };

        assert_eq!(pool.cached_buffer_count(), 1);

        let second = pool.acquire();
        assert_eq!(second.as_slice().as_ptr(), first_ptr);
        assert_eq!(second.len(), 16);
    }

    #[test]
    fn frame_pool_limits_cached_buffers() {
        let pool = FramePool::new(1, 1).unwrap();
        let buffers: Vec<_> = (0..MAX_POOLED_FRAME_BUFFERS + 3)
            .map(|_| pool.acquire())
            .collect();

        drop(buffers);

        assert_eq!(pool.cached_buffer_count(), MAX_POOLED_FRAME_BUFFERS);
    }
}
