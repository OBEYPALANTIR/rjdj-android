//! Streaming fixed-ratio resampling with precomputed, Blackman-windowed sinc phases.

use std::f64::consts::{PI, TAU};

const PHASES: usize = 256;
const BASE_TAPS: usize = 64;

/// A causal low-pass interpolator for interleaved audio. Construction alone allocates.
/// The integer phase accumulator is exact even for coprime sample rates; linear
/// interpolation between 256 FIR phases bounds the table size for every stream rate.
/// Downsampling expands the 64-tap window by the rate ratio to retain its rejection.
/// Group delay is `taps / 2 / from_rate` seconds (about 1.45 ms for 22050<->48000);
/// initial history is silence, and an equal-rate conversion is a zero-delay copy.
///
/// `process` accepts complete frames, retains filter history across arbitrary slices,
/// and reports frames, not samples. If the output fills, pass the unconsumed input on
/// the next call. An empty input can drain already-ready interpolation phases; it
/// does not flush the FIR tail. Supply silence explicitly when a tail is wanted.
pub struct Resampler {
    from_rate: u32,
    to_rate: u32,
    channels: usize,
    taps: usize,
    coefficients: Box<[f32]>,
    history: Box<[f32]>,
    cursor: usize,
    phase: u32,
    input_needed: u32,
}

impl Resampler {
    /// Rates must be 8000..=192000 and channels nonzero; these are setup-time contracts.
    pub fn new(from_rate: u32, to_rate: u32, channels: usize) -> Self {
        assert!((8000..=192000).contains(&from_rate));
        assert!((8000..=192000).contains(&to_rate));
        assert!(channels > 0);
        let taps = if from_rate == to_rate {
            0
        } else {
            ((BASE_TAPS as f64 * (from_rate as f64 / to_rate as f64).max(1.0)).ceil() as usize + 1)
                & !1
        };
        let mut coefficients = vec![0.0; (PHASES + 1) * taps];
        if taps != 0 {
            let cutoff = 0.45 * (to_rate as f64 / from_rate as f64).min(1.0);
            let delay = (taps / 2) as f64;
            for phase in 0..=PHASES {
                let fraction = phase as f64 / PHASES as f64;
                let row = &mut coefficients[phase * taps..(phase + 1) * taps];
                let mut sum = 0.0;
                for (tap, coefficient) in row.iter_mut().enumerate() {
                    let x = tap as f64 + fraction - delay;
                    let sinc = if x.abs() < 1e-12 {
                        2.0 * cutoff
                    } else {
                        (TAU * cutoff * x).sin() / (PI * x)
                    };
                    let window =
                        0.42 + 0.5 * (PI * x / delay).cos() + 0.08 * (TAU * x / delay).cos();
                    let value = sinc * window;
                    *coefficient = value as f32;
                    sum += value;
                }
                for coefficient in row {
                    *coefficient /= sum as f32;
                }
            }
        }
        Self {
            from_rate,
            to_rate,
            channels,
            taps,
            coefficients: coefficients.into_boxed_slice(),
            history: vec![0.0; taps * channels].into_boxed_slice(),
            cursor: 0,
            phase: 0,
            input_needed: 1,
        }
    }

    /// Consume/produce complete interleaved frames without allocating, locking or I/O.
    #[inline]
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> (usize, usize) {
        let input_frames = input.len() / self.channels;
        let output_frames = output.len() / self.channels;
        if self.taps == 0 {
            let frames = input_frames.min(output_frames);
            output[..frames * self.channels].copy_from_slice(&input[..frames * self.channels]);
            return (frames, frames);
        }
        let (mut consumed, mut produced) = (0, 0);
        while produced < output_frames {
            while self.input_needed != 0 && consumed < input_frames {
                let history_start = self.cursor * self.channels;
                let input_start = consumed * self.channels;
                self.history[history_start..history_start + self.channels]
                    .copy_from_slice(&input[input_start..input_start + self.channels]);
                self.cursor += 1;
                if self.cursor == self.taps {
                    self.cursor = 0;
                }
                consumed += 1;
                self.input_needed -= 1;
            }
            if self.input_needed != 0 {
                break;
            }
            let scaled_phase = self.phase as usize * PHASES;
            let row = scaled_phase / self.to_rate as usize;
            let fraction = (scaled_phase % self.to_rate as usize) as f32 / self.to_rate as f32;
            let first = &self.coefficients[row * self.taps..(row + 1) * self.taps];
            let second = &self.coefficients[(row + 1) * self.taps..(row + 2) * self.taps];
            let frame = &mut output[produced * self.channels..(produced + 1) * self.channels];
            frame.fill(0.0);
            let mut history_frame = self.cursor;
            for tap in 0..self.taps {
                if history_frame == 0 {
                    history_frame = self.taps;
                }
                history_frame -= 1;
                let weight = first[tap] + (second[tap] - first[tap]) * fraction;
                let history_start = history_frame * self.channels;
                for (channel, sample) in frame.iter_mut().enumerate() {
                    *sample += self.history[history_start + channel] * weight;
                }
            }
            produced += 1;
            let next = self.phase + self.from_rate;
            self.input_needed = next / self.to_rate;
            self.phase = next % self.to_rate;
        }
        (consumed, produced)
    }

    /// Reset time and history without reallocating the fixed filter or its buffers.
    #[inline]
    pub fn reset(&mut self) {
        self.history.fill(0.0);
        self.cursor = 0;
        self.phase = 0;
        self.input_needed = 1;
    }
}
