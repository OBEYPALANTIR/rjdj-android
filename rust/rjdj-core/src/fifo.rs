//! Single-callback-owned burst/tick adaptation; no synchronization or runtime allocation.

use crate::resample::Resampler;

const TICK: usize = 64;

struct Ring {
    samples: Box<[f32]>,
    head: usize,
    len: usize,
}

impl Ring {
    fn new(samples: usize) -> Self {
        Self {
            samples: vec![0.0; samples].into_boxed_slice(),
            head: 0,
            len: 0,
        }
    }

    #[inline]
    fn push(&mut self, samples: &[f32]) {
        let capacity = self.samples.len();
        let samples = &samples[samples.len().saturating_sub(capacity)..];
        let discard = (self.len + samples.len()).saturating_sub(capacity);
        self.head = (self.head + discard) % capacity;
        self.len -= discard;
        let tail = (self.head + self.len) % capacity;
        let first = samples.len().min(capacity - tail);
        self.samples[tail..tail + first].copy_from_slice(&samples[..first]);
        self.samples[..samples.len() - first].copy_from_slice(&samples[first..]);
        self.len += samples.len();
    }

    #[inline]
    fn pop(&mut self, output: &mut [f32]) -> usize {
        let count = self.len.min(output.len());
        let first = count.min(self.samples.len() - self.head);
        output[..first].copy_from_slice(&self.samples[self.head..self.head + first]);
        output[first..count].copy_from_slice(&self.samples[..count - first]);
        output[count..].fill(0.0);
        self.head = (self.head + count) % self.samples.len();
        self.len -= count;
        count
    }
}

fn rate_frames(frames: usize, from: u32, to: u32) -> usize {
    (frames as u64 * to as u64).div_ceil(from as u64) as usize
}

/// Mono stream input -> 64-frame Pd ticks -> stereo stream output, all at fixed rates.
/// Ring capacity reserves at least two bursts plus two ticks on each side, not a
/// mandatory latency cushion. Burst/tick alignment retains less than one tick per
/// side (up to two ticks round-trip), in addition to the Resampler FIR group delays:
/// about one further Pd tick round-trip at 48000<->22050 (about three at 8000).
/// Startup/short input is zero-filled; input underruns count affected ticks and output
/// underruns count short output chunks. An input producer that outruns this bounded
/// queue drops the oldest input, preserving frame alignment and bounded latency.
/// The supplied tick must itself obey the audio callback's realtime rules.
pub struct BurstTickFifo {
    input_resampler: Resampler,
    output_resampler: Resampler,
    input_ring: Ring,
    output_ring: Ring,
    input_scratch: Box<[f32]>,
    output_scratch: Box<[f32]>,
    tick_input: [f32; TICK],
    tick_output: [f32; TICK * 2],
    frames_per_burst: usize,
    pub input_underruns: u64,
    pub output_underruns: u64,
}

impl BurstTickFifo {
    /// This host uses pd_rate=22050, in_channels=1 and out_channels=2. A nonzero
    /// nominal burst and supported stream rate are checked once before the callback.
    pub fn new(
        stream_rate: u32,
        pd_rate: u32,
        frames_per_burst: usize,
        in_channels: usize,
        out_channels: usize,
    ) -> Self {
        assert!((8000..=192000).contains(&stream_rate));
        assert_eq!(pd_rate, 22050);
        assert!(frames_per_burst > 0);
        assert_eq!(in_channels, 1);
        assert_eq!(out_channels, 2);
        Self {
            input_resampler: Resampler::new(stream_rate, pd_rate, in_channels),
            output_resampler: Resampler::new(pd_rate, stream_rate, out_channels),
            input_ring: Ring::new(
                rate_frames(frames_per_burst * 2, stream_rate, pd_rate) + TICK * 2,
            ),
            output_ring: Ring::new(
                (frames_per_burst * 2 + rate_frames(TICK * 2, pd_rate, stream_rate)) * 2,
            ),
            input_scratch: vec![0.0; rate_frames(frames_per_burst, stream_rate, pd_rate) + 1]
                .into_boxed_slice(),
            output_scratch: vec![0.0; (rate_frames(TICK, pd_rate, stream_rate) + 1) * 2]
                .into_boxed_slice(),
            tick_input: [0.0; TICK],
            tick_output: [0.0; TICK * 2],
            frames_per_burst,
            input_underruns: 0,
            output_underruns: 0,
        }
    }

    /// Accept any number of complete mono stream frames, resampling into a bounded ring.
    #[inline]
    pub fn push_input(&mut self, stream_frames: &[f32]) {
        let mut consumed = 0;
        loop {
            let (used, produced) = self
                .input_resampler
                .process(&stream_frames[consumed..], &mut self.input_scratch);
            consumed += used;
            self.input_ring.push(&self.input_scratch[..produced]);
            if consumed == stream_frames.len() && produced < self.input_scratch.len() {
                break;
            }
        }
    }

    /// Pull complete stereo frames. Large requests are handled in nominal-burst chunks
    /// so the output ring never has to hold an arbitrarily large caller buffer.
    #[inline]
    pub fn pull_output(&mut self, out: &mut [f32], mut tick: impl FnMut(&[f32], &mut [f32])) {
        assert_eq!(
            out.len() % 2,
            0,
            "output must contain complete stereo frames"
        );
        for chunk in out.chunks_mut(self.frames_per_burst * 2) {
            while self.output_ring.len < chunk.len() {
                if self.input_ring.pop(&mut self.tick_input) < TICK {
                    self.input_underruns = self.input_underruns.saturating_add(1);
                }
                self.tick_output.fill(0.0);
                tick(&self.tick_input, &mut self.tick_output);
                let (_, produced) = self
                    .output_resampler
                    .process(&self.tick_output, &mut self.output_scratch);
                self.output_ring.push(&self.output_scratch[..produced * 2]);
                if produced == 0 {
                    break;
                }
            }
            if self.output_ring.pop(chunk) < chunk.len() {
                self.output_underruns = self.output_underruns.saturating_add(1);
            }
        }
    }
}
