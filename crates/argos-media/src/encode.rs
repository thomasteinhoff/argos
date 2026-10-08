use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use argos_core::h264;
use argos_core::metrics::{SenderMetrics, StageTimer};
use argos_core::session;

use openh264::encoder::{BitRate, Complexity, Encoder, EncoderConfig, FrameRate, UsageType};
use openh264::formats::YUVSlices;
use openh264::OpenH264API;

use crate::audio::FirstError;

pub struct H264Encoder {
    encoder: Encoder,
    planes: Vec<u8>,
    target_height: Option<u32>,
}

impl H264Encoder {
    pub fn new() -> Result<Self, String> {
        Self::new_at(30.0)
    }

    pub fn new_at(fps: f32) -> Result<Self, String> {
        // Sane bitrate targets for desktop screen content at the stream
        // quality (720p by default). 30-60 Mbps is far beyond useful for
        // screen sharing: it makes the software encoder frame-bound (the fps
        // collapses) and floods the network with tens of thousands of RTP
        // packets per second, which drops keyframes on WiFi and desyncs the
        // receiver's decoder.
        let (fps, bitrate) = if fps >= 45.0 {
            (60.0, 6_000_000)
        } else {
            (30.0, 4_000_000)
        };
        let config = EncoderConfig::new()
            // Screen content, not camera video. OpenH264 has a separate rate
            // control path for it, and it is the right one here: screen frames
            // are mostly flat areas and sharp text, where the camera-tuned
            // quantiser spends bits on detail the eye resolves and starves the
            // edges that are actually readable. It also holds a still screen at a
            // far smaller frame, which matters when the same desktop is on screen
            // for minutes at a time.
            //
            // Nothing downstream depends on which of the two was chosen — the
            // packetizer and the quality ladder both work from whatever comes out
            // — so this changes picture quality and bitrate, not correctness.
            .usage_type(UsageType::ScreenContentRealTime)
            .bitrate(BitRate::from_bps(bitrate))
            .max_frame_rate(FrameRate::from_hz(fps))
            .complexity(Complexity::Low)
            .skip_frames(false)
            .scene_change_detect(false)
            .adaptive_quantization(false);
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            encoder,
            planes: Vec::new(),
            target_height: None,
        })
    }

    pub fn set_target_height(&mut self, height: Option<u32>) {
        self.target_height = height;
    }

    pub fn target_dims(width: usize, height: usize, target_height: Option<u32>) -> (usize, usize) {
        match target_height {
            Some(target) if (target as usize) >= 2 && (target as usize) < height => {
                let out_height = (target as usize) & !1;
                let out_width = ((width * out_height / height) & !1).max(2);
                (out_width, out_height)
            }
            _ => (width, height),
        }
    }

    pub fn encode(&mut self, rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
        let (width, height) = self.convert(rgba, width, height)?;
        self.encode_planes(width, height)
    }

    /// Converts an RGBA frame into the encoder's I420 planes and returns the
    /// dimensions actually encoded.
    ///
    /// Split out from [`Self::encode`] so the colour conversion can be timed
    /// separately from the H.264 encode. These are very different costs with
    /// very different fixes: the conversion is our own scalar code, and when it
    /// needs to scale it does five integer divisions per output pixel.
    pub fn convert(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> Result<(usize, usize), String> {
        if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err("frame dimensions must be even".to_string());
        }
        let src_width = width as usize;
        let src_height = height as usize;
        if rgba.len() != src_width * src_height * 4 {
            return Err(format!(
                "rgba buffer length {} does not match {}x{}",
                rgba.len(),
                width,
                height
            ));
        }
        let (width, height) = Self::target_dims(src_width, src_height, self.target_height);
        if width == src_width && height == src_height {
            rgba_to_i420(rgba, width, height, &mut self.planes);
        } else {
            rgba_to_i420_scaled(rgba, src_width, src_height, width, height, &mut self.planes);
        }
        Ok((width, height))
    }

    /// Encodes the I420 planes produced by the last [`Self::convert`] call.
    pub fn encode_planes(&mut self, width: usize, height: usize) -> Result<Vec<u8>, String> {
        let y_len = width * height;
        let uv_len = (width / 2) * (height / 2);
        let yuv = YUVSlices::new(
            (
                &self.planes[..y_len],
                &self.planes[y_len..y_len + uv_len],
                &self.planes[y_len + uv_len..],
            ),
            (width, height),
            (width, width / 2, width / 2),
        );
        let bitstream = self
            .encoder
            .encode(&yuv)
            .map_err(|error| error.to_string())?;
        Ok(bitstream.to_vec())
    }

    pub fn force_keyframe(&mut self) {
        self.encoder.force_intra_frame();
    }
}

pub fn rgba_to_i420(rgba: &[u8], width: usize, height: usize, planes: &mut Vec<u8>) {
    let y_len = width * height;
    let uv_len = (width / 2) * (height / 2);
    planes.clear();
    planes.resize(y_len + uv_len + uv_len, 0);
    let (y, rest) = planes.split_at_mut(y_len);
    let (u, v) = rest.split_at_mut(uv_len);
    let half = width / 2;

    for y_row in 0..height / 2 {
        let row0 = y_row * 2;
        let row1 = row0 + 1;
        let src0 = row0 * width * 4;
        let src1 = row1 * width * 4;
        let y_dst = row0 * width;
        for x_col in 0..half {
            let s0 = src0 + x_col * 8;
            let s1 = src1 + x_col * 8;
            let r0 = rgba[s0] as i32;
            let g0 = rgba[s0 + 1] as i32;
            let b0 = rgba[s0 + 2] as i32;
            let r1 = rgba[s0 + 4] as i32;
            let g1 = rgba[s0 + 5] as i32;
            let b1 = rgba[s0 + 6] as i32;
            let r2 = rgba[s1] as i32;
            let g2 = rgba[s1 + 1] as i32;
            let b2 = rgba[s1 + 2] as i32;
            let r3 = rgba[s1 + 4] as i32;
            let g3 = rgba[s1 + 5] as i32;
            let b3 = rgba[s1 + 6] as i32;

            let px = x_col * 2;
            y[y_dst + px] = luma(r0, g0, b0);
            y[y_dst + px + 1] = luma(r1, g1, b1);
            y[y_dst + width + px] = luma(r2, g2, b2);
            y[y_dst + width + px + 1] = luma(r3, g3, b3);

            let ar = (r0 + r1 + r2 + r3 + 2) >> 2;
            let ag = (g0 + g1 + g2 + g3 + 2) >> 2;
            let ab = (b0 + b1 + b2 + b3 + 2) >> 2;
            let idx = y_row * half + x_col;
            u[idx] = chroma_u(ar, ag, ab);
            v[idx] = chroma_v(ar, ag, ab);
        }
    }
}

pub fn rgba_to_i420_scaled(
    rgba: &[u8],
    src_width: usize,
    src_height: usize,
    dst_width: usize,
    dst_height: usize,
    planes: &mut Vec<u8>,
) {
    let y_len = dst_width * dst_height;
    let uv_len = (dst_width / 2) * (dst_height / 2);
    planes.clear();
    planes.resize(y_len + uv_len + uv_len, 0);
    let (y, rest) = planes.split_at_mut(y_len);
    let (u, v) = rest.split_at_mut(uv_len);
    let half = dst_width / 2;

    for block_y in 0..dst_height / 2 {
        for block_x in 0..half {
            let mut chroma = [0i32; 3];
            for oy in 0..2 {
                let dst_y = block_y * 2 + oy;
                let src_y0 = dst_y * src_height / dst_height;
                let src_y1 = ((dst_y + 1) * src_height / dst_height)
                    .max(src_y0 + 1)
                    .min(src_height);
                for ox in 0..2 {
                    let dst_x = block_x * 2 + ox;
                    let src_x0 = dst_x * src_width / dst_width;
                    let src_x1 = ((dst_x + 1) * src_width / dst_width)
                        .max(src_x0 + 1)
                        .min(src_width);
                    let mut r_sum = 0u32;
                    let mut g_sum = 0u32;
                    let mut b_sum = 0u32;
                    let mut count = 0u32;
                    for sy in src_y0..src_y1 {
                        let row = sy * src_width * 4;
                        for sx in src_x0..src_x1 {
                            let base = row + sx * 4;
                            r_sum += rgba[base] as u32;
                            g_sum += rgba[base + 1] as u32;
                            b_sum += rgba[base + 2] as u32;
                            count += 1;
                        }
                    }
                    let count = count.max(1) as i32;
                    let r = r_sum as i32 / count;
                    let g = g_sum as i32 / count;
                    let b = b_sum as i32 / count;
                    y[dst_y * dst_width + dst_x] = luma(r, g, b);
                    chroma[0] += r;
                    chroma[1] += g;
                    chroma[2] += b;
                }
            }
            let idx = block_y * half + block_x;
            u[idx] = chroma_u(chroma[0] / 4, chroma[1] / 4, chroma[2] / 4);
            v[idx] = chroma_v(chroma[0] / 4, chroma[1] / 4, chroma[2] / 4);
        }
    }
}

#[inline]
fn luma(r: i32, g: i32, b: i32) -> u8 {
    (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16).clamp(0, 255) as u8
}

#[inline]
fn chroma_u(r: i32, g: i32, b: i32) -> u8 {
    (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8
}

#[inline]
fn chroma_v(r: i32, g: i32, b: i32) -> u8 {
    (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(0, 255) as u8
}

/// Fallback interval for periodic intra frames.
///
/// This used to be 2 s and was the sharer's *only* recovery mechanism, because
/// the rtc transport offers no RTCP feedback path (see `session.rs`). It is
/// now a backstop behind on-demand requests over the LAN channel: every
/// keyframe is a burst on a Radmin tunnel, and a burst is what causes loss in
/// the first place.
pub const KEYFRAME_INTERVAL: Duration = Duration::from_secs(4);

/// Floor between keyframes forced on a viewer's request.
///
/// A keyframe costs a full-frame burst. Honouring every request from a viewer
/// on a bad link would turn a recovery mechanism into a denial of service, so
/// this rate-limits a peer regardless of how often it asks.
pub const KEYFRAME_REQUEST_FLOOR: Duration = Duration::from_millis(500);

/// Packs a resolution choice into the atomic the encode worker polls.
///
/// A negative value means "native" (no scaling); anything else is a target
/// height in pixels. The target travels as an atomic rather than as a message
/// because it must not share the frame queue: that queue is full exactly when
/// this matters (the encoder is behind), and a control message behind a
/// backlog of stale frames would be dropped or applied minutes late.
pub fn encode_target(height: Option<u32>) -> i32 {
    height.map_or(-1, |height| height as i32)
}

pub fn decode_target(value: i32) -> Option<u32> {
    (value >= 0).then_some(value as u32)
}

/// One captured frame handed to the encode worker.
pub struct EncodeMsg {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Encodes captured frames and hands the bitstream to the transport, on its
/// own thread.
///
/// The send side of the share used to live as a free function in the app's UI
/// file, together with the resolution and keyframe policy it runs. Moving it
/// here put the worker next to the encoder it drives, gave the loss counters a
/// crate where they are read without a UI mutex, and left the UI file with
/// just the frame pushes. The worker's only exit is the frame channel
/// disconnecting, so the queue sender must be dropped before joining it.
pub struct EncodeWorker {
    join: Option<JoinHandle<()>>,
    /// First encode or write failure, surfaced by [`Self::take_error`].
    error: Arc<FirstError>,
}

impl EncodeWorker {
    pub fn spawn(
        rx: Receiver<EncodeMsg>,
        sharer: Arc<session::Sharer>,
        mut encoder: H264Encoder,
        metrics: Arc<SenderMetrics>,
        force_keyframe: Arc<AtomicBool>,
        target_height: Arc<AtomicI32>,
    ) -> Result<Self, String> {
        let error = Arc::new(FirstError::default());
        let thread_error = Arc::clone(&error);
        let join = thread::Builder::new()
            .name("argos-encode".to_string())
            .spawn(move || {
                // RTP timestamps come from elapsed wall time, not from the frame
                // rate. The counter form (`timestamp += 90_000 / fps`) claims a
                // fixed interval per frame, which is false for any frame that is
                // late, coalesced or dropped — so the sender's clock outruns real
                // time and the receiver has to discard good frames to stay in
                // sync. The frame rate now lives only in the encoder's own
                // configuration, set before this thread starts.
                let clock = h264::Clock::new();
                let mut last_keyframe = Instant::now();
                // The target the encoder is currently configured for. Compared
                // against the shared atomic on every frame, so a resolution
                // change is picked up on the next frame regardless of how backed
                // up the frame queue is.
                let mut applied_target = target_height.load(Ordering::Relaxed);
                // Set when an intra frame has been asked for but not yet
                // encoded. The flag outlives the request, because the frame that
                // carries the intra arrives later — and that frame's size is the
                // number worth measuring, not the request's.
                let mut pending_keyframe = false;
                while let Ok(msg) = rx.recv() {
                    metrics.captured_width.set(msg.width as u64);
                    metrics.captured_height.set(msg.height as u64);
                    // Pick up an adaptive or user resolution change. This is
                    // polled rather than queued: the frame queue fills up
                    // precisely when the encoder is behind, which is exactly when
                    // the controller is trying to lower the resolution, so a
                    // queued control message would be dropped by the very
                    // congestion it exists to relieve.
                    let target = target_height.load(Ordering::Relaxed);
                    if target != applied_target {
                        applied_target = target;
                        encoder.set_target_height(decode_target(target));
                        // A resolution change is unviewable until the next intra
                        // frame, so this one is never optional.
                        encoder.force_keyframe();
                        last_keyframe = Instant::now();
                        pending_keyframe = true;
                    }
                    // A viewer asking for recovery. An atomic flag rather than a
                    // message: the frame queue below is small and fills up
                    // exactly when the machine is struggling, and a dropped
                    // request would strand a viewer waiting for an intra frame it
                    // was promised. Requests that arrive faster than the floor
                    // are coalesced — one intra frame serves every request in the
                    // interval.
                    let requested = force_keyframe.swap(false, Ordering::Relaxed);
                    // Backstop for a request that never arrived: the LAN channel
                    // is UDP, and a manual-code session has no channel at all.
                    let overdue = last_keyframe.elapsed() >= KEYFRAME_INTERVAL;
                    if (requested && last_keyframe.elapsed() >= KEYFRAME_REQUEST_FLOOR) || overdue {
                        encoder.force_keyframe();
                        last_keyframe = Instant::now();
                        pending_keyframe = true;
                    }
                    // Conversion and encoding are timed separately: they have very
                    // different costs and very different fixes. The conversion is
                    // our own scalar code and, when it scales, does five integer
                    // divisions per output pixel; the encode is openh264.
                    let converted = {
                        let _convert = StageTimer::new(&metrics.convert);
                        encoder.convert(&msg.rgba, msg.width, msg.height)
                    };
                    let bitstream = match converted {
                        Ok(dims) => {
                            metrics.encoded_width.set(dims.0 as u64);
                            metrics.encoded_height.set(dims.1 as u64);
                            let _encode = StageTimer::new(&metrics.encode);
                            encoder.encode_planes(dims.0, dims.1)
                        }
                        Err(error) => Err(error),
                    };
                    match bitstream {
                        Ok(bitstream) => {
                            // Sampled at send time, not at receipt: the clock is
                            // measuring how long this frame waited, which is
                            // exactly the latency a receiver has to absorb.
                            let ts = clock.ticks(Instant::now());
                            let size = bitstream.len() as u64;
                            metrics.encoded_bytes.add(size);
                            if pending_keyframe {
                                // The frame following a forced intra is an IDR, so
                                // this is the size of the burst that goes out on
                                // the wire. If this number is large, it is a
                                // plausible cause of the loss it is meant to help
                                // recover from.
                                metrics.keyframes.incr();
                                metrics.last_keyframe_bytes.set(size);
                                pending_keyframe = false;
                            }
                            match session::block_on(sharer.send_frame(&bitstream, ts, &metrics)) {
                                Ok(()) => metrics.encoded.record(),
                                Err(message) => thread_error.set(message),
                            }
                        }
                        Err(message) => {
                            metrics.encode_errors.incr();
                            thread_error.set(message);
                        }
                    }
                }
            })
            .map_err(|error| format!("could not start the encode worker thread: {error}"))?;
        Ok(Self {
            join: Some(join),
            error,
        })
    }

    /// First encode or write failure, taken (cleared) by the call.
    pub fn take_error(&self) -> Option<String> {
        self.error.take()
    }

    /// Waits for the worker to finish. The frame queue sender must be dropped
    /// first: the loop only exits when its `recv` sees the channel disconnect.
    pub fn join(mut self) {
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for EncodeWorker {
    fn drop(&mut self) {
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::rgba_to_i420;

    fn reference(rgba: &[u8], width: usize, height: usize) -> Vec<u8> {
        let y_len = width * height;
        let uv_len = (width / 2) * (height / 2);
        let mut planes = vec![0u8; y_len + uv_len + uv_len];
        let (y, rest) = planes.split_at_mut(y_len);
        let (u, v) = rest.split_at_mut(uv_len);
        for row in 0..height / 2 {
            for col in 0..width / 2 {
                let mut sum = [0u32; 3];
                for oy in 0..2 {
                    for ox in 0..2 {
                        let px = col * 2 + ox;
                        let py = row * 2 + oy;
                        let base = (py * width + px) * 4;
                        let r = rgba[base] as f32;
                        let g = rgba[base + 1] as f32;
                        let b = rgba[base + 2] as f32;
                        sum[0] += rgba[base] as u32;
                        sum[1] += rgba[base + 1] as u32;
                        sum[2] += rgba[base + 2] as u32;
                        let y_val = 0.2578125 * r + 0.50390625 * g + 0.09765625 * b + 16.0;
                        y[py * width + px] = y_val as u8;
                    }
                }
                let ar = sum[0] as f32 / 4.0;
                let ag = sum[1] as f32 / 4.0;
                let ab = sum[2] as f32 / 4.0;
                let idx = row * (width / 2) + col;
                let u_val = -0.1484375 * ar - 0.2890625 * ag + 0.4375 * ab + 128.0;
                let v_val = 0.4375 * ar - 0.3671875 * ag - 0.0703125 * ab + 128.0;
                u[idx] = u_val as u8;
                v[idx] = v_val as u8;
            }
        }
        planes
    }

    #[test]
    fn integer_conversion_matches_float_reference() {
        let width = 32;
        let height = 16;
        let mut rgba = vec![0u8; width * height * 4];
        for (i, byte) in rgba.iter_mut().enumerate() {
            *byte = ((i * 97 + 13) % 251) as u8;
        }
        let mut actual = Vec::new();
        rgba_to_i420(&rgba, width, height, &mut actual);
        let expected = reference(&rgba, width, height);
        assert_eq!(actual.len(), expected.len());
        let max_diff = actual
            .iter()
            .zip(&expected)
            .map(|(a, b)| (*a as i32 - *b as i32).abs())
            .max()
            .unwrap_or(0);
        assert!(max_diff <= 1, "max difference {max_diff}");
    }

    #[test]
    fn scaled_conversion_averages_source_blocks() {
        use super::{luma, rgba_to_i420_scaled};
        let src_width = 8;
        let src_height = 4;
        let mut rgba = vec![0u8; src_width * src_height * 4];
        for (i, byte) in rgba.iter_mut().enumerate() {
            *byte = ((i * 53 + 7) % 251) as u8;
        }
        let dst_width = 4;
        let dst_height = 2;
        let mut planes = Vec::new();
        rgba_to_i420_scaled(
            &rgba,
            src_width,
            src_height,
            dst_width,
            dst_height,
            &mut planes,
        );
        assert_eq!(planes.len(), dst_width * dst_height * 3 / 2);

        for dst_y in 0..dst_height {
            for dst_x in 0..dst_width {
                let mut sum = [0i32; 3];
                for oy in 0..2 {
                    for ox in 0..2 {
                        let base = ((dst_y * 2 + oy) * src_width + dst_x * 2 + ox) * 4;
                        sum[0] += rgba[base] as i32;
                        sum[1] += rgba[base + 1] as i32;
                        sum[2] += rgba[base + 2] as i32;
                    }
                }
                let expected = luma(sum[0] / 4, sum[1] / 4, sum[2] / 4);
                let actual = planes[dst_y * dst_width + dst_x];
                assert!(
                    (actual as i32 - expected as i32).abs() <= 1,
                    "luma mismatch at {dst_x},{dst_y}: {actual} vs {expected}"
                );
            }
        }
    }
}
