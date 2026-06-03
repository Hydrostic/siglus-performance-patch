use ffmpeg_next::Rescale;

use crate::omv_player::{clock::MICROSECOND_RATIONAL, packet::Packet};

pub struct Demuxer {
    input: ffmpeg_next::format::context::Input,
    stream_index: usize,
    stream_time_base: ffmpeg_next::Rational,
    // key_frames: KeyframeIndex,
}
impl Demuxer {
    pub(crate) fn new(
        input: ffmpeg_next::format::context::Input,
        stream_index: usize,
        stream_time_base: ffmpeg_next::Rational,
    ) -> Self {
        Self {
            input,
            stream_index,
            stream_time_base,
        }
    }

    pub fn seek(&mut self, clock_ms: i64) -> bool {
        let pts = clock_ms.rescale(MICROSECOND_RATIONAL, self.stream_time_base);
        // let nearest_keyframe = self.key_frames.find_before_or_equal(clock_ms);
        // let seek_min_pts = nearest_keyframe
        //     .map(|kf_ms| kf_ms.rescale(MICROSECOND_RATIONAL, self.stream_time_base))
        //     .unwrap_or(0);
        self.input.seek(pts, 0..pts + 1).is_ok()
    }

    pub fn next_packet(&mut self) -> Option<Packet> {
        while let Some((stream, packet)) = self.input.packets().next() {
            if stream.index() == self.stream_index {
                let pts_ms = packet
                    .pts()
                    .map(|pts| pts.rescale(stream.time_base(), MICROSECOND_RATIONAL));
                let duration_ms = packet
                    .duration()
                    .rescale(stream.time_base(), MICROSECOND_RATIONAL);
                let is_keyframe = packet.is_key();
                return Some(Packet {
                    inner: packet,
                    pts_ms,
                    duration_ms,
                    presentable: pts_ms.is_some(),
                    is_keyframe,
                });
            }
        }
        None
    }
}
