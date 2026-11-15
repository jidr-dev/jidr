//! Device audio for recordings (#28): the emulator's gRPC
//! `streamAudio(AudioFormat) → stream AudioPacket`, laid on the video's
//! timeline.
//!
//! Video and audio share one clock: the instant the recording started. A
//! frame is stamped by when the encoder took it, and an audio packet by when
//! it arrived, less its own length. The emulator sends packets every
//! 20–30 ms while the guest plays sound and nothing at all while it's
//! silent, so [`Timeline`] keeps the track continuous:
//!
//! - a packet that lands within [`TOLERANCE`] of where the track ends is
//!   appended as is (arrival jitter is not a gap, and padding it would click);
//! - a later packet is preceded by silence up to its place;
//! - a packet whose place the track has already passed by more than
//!   [`TOLERANCE`] loses its overlapping start (it arrived too late);
//! - with no packets, silence is written up to [`HOLD`] behind the clock, so
//!   the audio never lags the video by more than that (AVAssetWriter
//!   interleaves the two) and a late packet still finds its place.
//!
//! So the audio never drifts from the video by more than [`TOLERANCE`].

use std::time::Duration;

use jidr_proto::control as pb;

/// Sample rate asked of the emulator and written to the file.
pub const RATE: u32 = 48_000;
/// Stereo.
pub const CHANNELS: u32 = 2;
/// Signed 16-bit little-endian, interleaved.
pub const FRAME_BYTES: usize = 2 * CHANNELS as usize;
/// How far a packet may sit from the track's end and still be appended
/// without padding or trimming.
pub const TOLERANCE: Duration = Duration::from_millis(40);
/// How far behind the clock silence is written while no packets come.
pub const HOLD: Duration = Duration::from_millis(300);

/// The format asked of `streamAudio`.
// The build may use the installed SDK's newer protos, which can add fields.
#[allow(clippy::needless_update)]
pub fn format_pb() -> pb::AudioFormat {
    pb::AudioFormat {
        sampling_rate: u64::from(RATE),
        channels: pb::audio_format::Channels::Stereo as i32,
        format: pb::audio_format::SampleFormat::AudFmtS16 as i32,
        ..Default::default()
    }
}

/// A packet's samples as 16-bit stereo at [`RATE`], or `None` when the
/// emulator sent another rate or 8-bit samples (it honours the requested
/// format, so this only guards against a surprise). Mono is doubled. A
/// packet without a format is in the requested one.
pub fn to_stereo(format: Option<&pb::AudioFormat>, pcm: &[u8]) -> Option<Vec<u8>> {
    let Some(f) = format else {
        return Some(whole_frames(pcm, FRAME_BYTES).to_vec());
    };
    let rate_ok = f.sampling_rate == 0 || f.sampling_rate == u64::from(RATE);
    if !rate_ok || f.format != pb::audio_format::SampleFormat::AudFmtS16 as i32 {
        return None;
    }
    if f.channels == pb::audio_format::Channels::Stereo as i32 {
        return Some(whole_frames(pcm, FRAME_BYTES).to_vec());
    }
    let mono = whole_frames(pcm, 2);
    let mut out = Vec::with_capacity(mono.len() * 2);
    for s in mono.as_chunks::<2>().0 {
        out.extend_from_slice(s);
        out.extend_from_slice(s);
    }
    Some(out)
}

fn whole_frames(pcm: &[u8], frame: usize) -> &[u8] {
    &pcm[..pcm.len() - pcm.len() % frame]
}

/// Frames in `d` at [`RATE`].
pub fn frames_in(d: Duration) -> u64 {
    (d.as_secs_f64() * f64::from(RATE)).round() as u64
}

/// Samples to write at `start` (in frames from the recording's start).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub start: u64,
    pub pcm: Vec<u8>,
}

impl Chunk {
    pub fn frames(&self) -> u64 {
        (self.pcm.len() / FRAME_BYTES) as u64
    }

    fn silence(start: u64, frames: u64) -> Self {
        Self {
            start,
            pcm: vec![0; frames as usize * FRAME_BYTES],
        }
    }
}

/// Places packets and silence on the shared timeline; see the module docs.
#[derive(Debug, Default)]
pub struct Timeline {
    /// Frames written so far: where the track ends.
    end: u64,
}

impl Timeline {
    pub fn end(&self) -> u64 {
        self.end
    }

    /// A packet whose first sample belongs at `at` (since the recording's
    /// start), 16-bit stereo. Returns what to write, in order.
    pub fn place(&mut self, at: Duration, pcm: &[u8]) -> Vec<Chunk> {
        let pcm = whole_frames(pcm, FRAME_BYTES);
        let frames = (pcm.len() / FRAME_BYTES) as u64;
        if frames == 0 {
            return Vec::new();
        }
        let want = frames_in(at);
        let tol = frames_in(TOLERANCE);
        let mut out = Vec::new();
        let skip = if want > self.end + tol {
            out.push(Chunk::silence(self.end, want - self.end));
            self.end = want;
            0
        } else if self.end > want + tol {
            // Late: what overlaps the track is dropped.
            (self.end - want).min(frames)
        } else {
            0
        };
        if skip < frames {
            let rest = pcm[skip as usize * FRAME_BYTES..].to_vec();
            let chunk = Chunk {
                start: self.end,
                pcm: rest,
            };
            self.end += chunk.frames();
            out.push(chunk);
        }
        out
    }

    /// Silence up to [`HOLD`] behind `now` (since the recording's start).
    pub fn fill(&mut self, now: Duration) -> Option<Chunk> {
        self.pad_to(frames_in(now.saturating_sub(HOLD)))
    }

    /// Silence up to `end` (the video's length), when the recording stops.
    pub fn finish(&mut self, end: Duration) -> Option<Chunk> {
        self.pad_to(frames_in(end))
    }

    fn pad_to(&mut self, frame: u64) -> Option<Chunk> {
        if frame <= self.end {
            return None;
        }
        let c = Chunk::silence(self.end, frame - self.end);
        self.end = frame;
        Some(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// `n` frames of a constant sample.
    fn pcm(n: usize, v: u8) -> Vec<u8> {
        vec![v; n * FRAME_BYTES]
    }

    /// Every chunk starts where the previous one ended.
    fn contiguous(chunks: &[Chunk]) -> bool {
        chunks
            .windows(2)
            .all(|w| w[0].start + w[0].frames() == w[1].start)
    }

    #[test]
    fn packets_on_time_are_appended_back_to_back() {
        let mut t = Timeline::default();
        let mut all = Vec::new();
        // 20 ms packets (960 frames) arriving with ±10 ms of jitter.
        for (i, jitter) in [0i64, 8, -6, 10, -10, 3].iter().enumerate() {
            let at = (i as i64 * 20 + jitter).max(0) as u64;
            all.extend(t.place(ms(at), &pcm(960, 1)));
        }
        assert!(contiguous(&all));
        assert_eq!(all.len(), 6, "no silence was inserted: {all:?}");
        assert_eq!(all[0].start, 0);
        assert_eq!(t.end(), 6 * 960);
    }

    #[test]
    fn a_gap_is_filled_with_silence_so_the_sound_stays_in_place() {
        let mut t = Timeline::default();
        let mut all = t.place(ms(0), &pcm(960, 1));
        // Nothing for 2 s, then sound again.
        all.extend(t.place(ms(2_000), &pcm(960, 1)));
        assert!(contiguous(&all));
        assert_eq!(all.len(), 3);
        assert!(all[1].pcm.iter().all(|b| *b == 0));
        assert_eq!(all[2].start, frames_in(ms(2_000)));
    }

    #[test]
    fn the_first_packet_after_a_quiet_start_is_padded_from_zero() {
        let mut t = Timeline::default();
        let all = t.place(ms(1_500), &pcm(960, 1));
        assert_eq!(all[0], Chunk::silence(0, 72_000));
        assert_eq!(all[1].start, 72_000);
    }

    #[test]
    fn silence_follows_the_clock_while_nothing_plays() {
        let mut t = Timeline::default();
        assert_eq!(t.fill(ms(200)), None, "still inside the hold");
        let c = t.fill(ms(1_300)).unwrap();
        assert_eq!((c.start, c.frames()), (0, frames_in(ms(1_000))));
        assert_eq!(t.fill(ms(1_300)), None);
        // A packet that arrives inside the hold still lands in place.
        let all = t.place(ms(1_100), &pcm(960, 1));
        assert_eq!(all[0].start, frames_in(ms(1_000)));
        assert_eq!(all.last().unwrap().start, frames_in(ms(1_100)));
    }

    #[test]
    fn a_late_packet_loses_only_what_overlaps() {
        let mut t = Timeline::default();
        t.fill(ms(1_300)); // track ends at 1.0 s
        // Belongs at 0.9 s, 200 ms long: the first 100 ms is dropped.
        let all = t.place(ms(900), &pcm(9_600, 1));
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].start, frames_in(ms(1_000)));
        assert_eq!(all[0].frames(), 4_800);
        // Entirely in the past: nothing.
        assert!(t.place(ms(0), &pcm(960, 1)).is_empty());
    }

    /// The track never drifts from the clock by more than the tolerance,
    /// even when the emulator's packets run a little fast or slow.
    #[test]
    fn drift_stays_within_the_tolerance() {
        for speed in [0.97f64, 1.03] {
            let mut t = Timeline::default();
            for i in 0..3_000u64 {
                // Packets hold 20 ms but arrive every 20 ms × speed.
                let at = Duration::from_secs_f64(i as f64 * 0.020 * speed);
                for c in t.place(at, &pcm(960, 1)) {
                    if c.pcm[0] == 1 {
                        // A trimmed packet starts `960 - frames` samples in.
                        let want = (frames_in(at) + 960 - c.frames()) as i64;
                        let off = c.start as i64 - want;
                        assert!(
                            off.unsigned_abs() <= frames_in(TOLERANCE),
                            "speed {speed}: packet {i} off by {off} frames"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn finishing_pads_to_the_video_length() {
        let mut t = Timeline::default();
        t.place(ms(0), &pcm(960, 1));
        let c = t.finish(ms(1_000)).unwrap();
        assert_eq!(c.start, 960);
        assert_eq!(t.end(), RATE as u64);
        assert_eq!(t.finish(ms(500)), None);
    }

    #[test]
    fn packets_are_converted_to_stereo_or_refused() {
        let stereo = format_pb();
        assert_eq!(
            to_stereo(Some(&stereo), &[1, 2, 3, 4, 5]),
            Some(vec![1, 2, 3, 4])
        );
        assert_eq!(to_stereo(None, &[1, 2, 3, 4]), Some(vec![1, 2, 3, 4]));
        let mono = pb::AudioFormat {
            channels: pb::audio_format::Channels::Mono as i32,
            ..format_pb()
        };
        assert_eq!(to_stereo(Some(&mono), &[1, 2, 3]), Some(vec![1, 2, 1, 2]));
        let other_rate = pb::AudioFormat {
            sampling_rate: 44_100,
            ..format_pb()
        };
        assert_eq!(to_stereo(Some(&other_rate), &[0; 4]), None);
        let u8_fmt = pb::AudioFormat {
            format: pb::audio_format::SampleFormat::AudFmtU8 as i32,
            ..format_pb()
        };
        assert_eq!(to_stereo(Some(&u8_fmt), &[0; 4]), None);
    }
}
