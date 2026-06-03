use crate::{
    omv_player::{
        clock::MICROSECOND_RATIONAL,
        frame::{convert_frame_pix_fmt, Frame},
        packet::Packet,
    },
    omv_types::OmvVideoInfo,
};
use ffmpeg_next::Rescale;

pub struct Decoder {
    inner: ffmpeg_next::decoder::Video,
    stream_time_base: ffmpeg_next::Rational,
    video_info: OmvVideoInfo,
}

impl Decoder {
    pub fn new(
        inner: ffmpeg_next::decoder::Video,
        stream_time_base: ffmpeg_next::Rational,
        video_info: OmvVideoInfo,
    ) -> Self {
        Self {
            inner,
            stream_time_base,
            video_info,
        }
    }

    pub fn decode_packet(&mut self, packet: &Packet) -> Result<Vec<Frame>, String> {
        self.inner
            .send_packet(&packet.inner)
            .map_err(|error| format!("send packet to decoder failed: {error}"))?;
        self.drain_frames(packet.presentable)
    }

    pub fn flush(&mut self) {
        self.inner.flush();
    }

    fn drain_frames(&mut self, presentable: bool) -> Result<Vec<Frame>, String> {
        let mut frames = Vec::new();
        loop {
            let mut video_frame = ffmpeg_next::util::frame::Video::empty();
            match self.inner.receive_frame(&mut video_frame) {
                Ok(()) => {
                    if !presentable {
                        continue;
                    }
                    frames.push(self.convert_frame(&video_frame)?);
                }
                Err(ffmpeg_next::Error::Other { errno }) if errno == ffmpeg_next::error::EAGAIN => {
                    break;
                }
                Err(ffmpeg_next::Error::Eof) => break,
                Err(error) => return Err(format!("receive frame from decoder failed: {error}")),
            }
        }
        Ok(frames)
    }

    fn convert_frame(
        &self,
        video_frame: &ffmpeg_next::util::frame::Video,
    ) -> Result<Frame, String> {
        let pts_ms = video_frame
            .timestamp()
            .map(|pts| pts.rescale(self.stream_time_base, MICROSECOND_RATIONAL));
        let data = convert_frame_pix_fmt(
            video_frame,
            self.video_info.format,
            self.video_info.display_height as usize,
            self.video_info.display_width as usize,
        )?;
        Ok(Frame {
            inner: data,
            pts_ms,
            duration_ms: None,
        })
    }
}
