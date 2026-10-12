use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::f64::consts::TAU;

use rjdjcore::fifo::BurstTickFifo;
use rjdjcore::resample::Resampler;

struct CountingAllocator;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

fn count_allocation() {
    if COUNTING.with(Cell::get) {
        ALLOCATIONS.with(|count| count.set(count.get() + 1));
    }
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count_allocation();
        // SAFETY: layout comes from the allocator caller and is passed through unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count_allocation();
        // SAFETY: layout comes from the allocator caller and is passed through unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        count_allocation();
        // SAFETY: allocation ownership and the old layout come from the allocator caller.
        unsafe { System.realloc(ptr, layout, size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: allocation ownership and layout come from the allocator caller.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

struct AllocationGuard;

impl AllocationGuard {
    fn start() -> Self {
        ALLOCATIONS.with(|count| count.set(0));
        COUNTING.with(|count| count.set(true));
        Self
    }

    fn finish(self) -> usize {
        COUNTING.with(|count| count.set(false));
        ALLOCATIONS.with(Cell::get)
    }
}

impl Drop for AllocationGuard {
    fn drop(&mut self) {
        COUNTING.with(|count| count.set(false));
    }
}

fn sine(rate: u32, frames: usize, frequency: f64, amplitude: f64) -> Vec<f32> {
    (0..frames)
        .map(|n| (amplitude * (TAU * frequency * n as f64 / rate as f64).sin()) as f32)
        .collect()
}

fn convert(
    input: &[f32],
    from: u32,
    to: u32,
    channels: usize,
    input_chunk: usize,
    output_chunk: usize,
) -> Vec<f32> {
    let mut converter = Resampler::new(from, to, channels);
    let mut output = Vec::new();
    let mut scratch = vec![0.0; output_chunk * channels];
    let mut at = 0;
    let frames = input.len() / channels;
    while at < frames {
        let end = (at + input_chunk).min(frames);
        let (consumed, produced) =
            converter.process(&input[at * channels..end * channels], &mut scratch);
        assert!(
            consumed > 0 || produced > 0,
            "converter must make progress with nonempty buffers"
        );
        assert!(consumed <= end - at);
        assert!(produced <= output_chunk);
        at += consumed;
        output.extend_from_slice(&scratch[..produced * channels]);
    }
    loop {
        let (consumed, produced) = converter.process(&[], &mut scratch);
        assert_eq!(consumed, 0);
        output.extend_from_slice(&scratch[..produced * channels]);
        if produced == 0 {
            break;
        }
    }
    output
}

fn fundamental(samples: &[f32], rate: u32, frequency: f64) -> (f64, f64) {
    let (mut sin, mut cos) = (0.0, 0.0);
    for (n, &sample) in samples.iter().enumerate() {
        let phase = TAU * frequency * n as f64 / rate as f64;
        sin += sample as f64 * phase.sin();
        cos += sample as f64 * phase.cos();
    }
    let scale = 2.0 / samples.len() as f64;
    (sin * scale, cos * scale)
}

fn amplitude(samples: &[f32], rate: u32, frequency: f64) -> f64 {
    let (sin, cos) = fundamental(samples, rate, frequency);
    sin.hypot(cos)
}

fn dbfs(amplitude: f64) -> f64 {
    20.0 * amplitude.max(1e-15).log10()
}

fn residual_dbfs(samples: &[f32], rate: u32, frequency: f64) -> f64 {
    let (sin, cos) = fundamental(samples, rate, frequency);
    let power: f64 = samples
        .iter()
        .enumerate()
        .map(|(n, &sample)| {
            let phase = TAU * frequency * n as f64 / rate as f64;
            let error = sample as f64 - sin * phase.sin() - cos * phase.cos();
            error * error
        })
        .sum();
    dbfs((2.0 * power / samples.len() as f64).sqrt())
}

#[test]
fn round_trip_preserves_440_hz_amplitude_and_rejects_images_and_aliases() {
    let input = sine(22050, 44100, 440.0, 0.8);
    let up = convert(&input, 22050, 48000, 1, 64, 192);
    let down = convert(&up, 48000, 22050, 1, 192, 64);
    assert_eq!(up.len(), 96000);
    assert_eq!(down.len(), 44100);
    let gain_db = dbfs(amplitude(&down[22050..], 22050, 440.0) / 0.8);
    let round_trip_residual = residual_dbfs(&down[22050..], 22050, 440.0);
    let image_db = [22050.0 - 440.0, 22050.0 + 440.0]
        .into_iter()
        .map(|frequency| dbfs(amplitude(&up[48000..], 48000, frequency)))
        .fold(f64::NEG_INFINITY, f64::max);
    // A 21610 Hz input at 48 kHz would alias to 440 Hz without the decimation filter.
    let high = sine(48000, 96000, 22050.0 - 440.0, 0.8);
    let rejected = convert(&high, 48000, 22050, 1, 192, 64);
    let alias_db = dbfs(amplitude(&rejected[22050..], 22050, 440.0));
    eprintln!("440 Hz round trip: gain={gain_db:.4} dB, residual={round_trip_residual:.2} dBFS, image={image_db:.2} dBFS, alias={alias_db:.2} dBFS");
    // Measured host test: gain 0.0000 dB, residual -135.00 dBFS, images -124.23 dBFS,
    // injected alias -129.52 dBFS. Keep a conservative -100 dBFS regression ceiling,
    // comfortably below the required -50 dBFS even with platform rounding differences.
    let rejection_limit = -100.0;
    assert!(gain_db.abs() <= 0.5, "gain {gain_db:.4} dB");
    assert!(
        round_trip_residual < rejection_limit,
        "residual {round_trip_residual:.2} dBFS"
    );
    assert!(image_db < rejection_limit, "image {image_db:.2} dBFS");
    assert!(alias_db < rejection_limit, "alias {alias_db:.2} dBFS");
}

#[test]
fn arbitrary_rates_and_tiny_buffers_do_not_change_samples_or_lose_frames() {
    for stream_rate in [8000, 22050, 44100, 44117, 48000, 47999, 192000] {
        for (from, to) in [(22050, stream_rate), (stream_rate, 22050)] {
            for channels in [1, 2] {
                let mono = sine(from, 997, 440.0, 0.8);
                let input: Vec<f32> = mono
                    .iter()
                    .flat_map(|&sample| [sample, -sample * 0.5].into_iter().take(channels))
                    .collect();
                let whole = convert(&input, from, to, channels, 997, 10000);
                let split = convert(&input, from, to, channels, 7, 1);
                assert_eq!(
                    split, whole,
                    "chunking changed {from}->{to}, {channels} channels"
                );
                let expected = (997u64 * to as u64).div_ceil(from as u64) as usize;
                assert_eq!(whole.len(), expected * channels);
                if channels == 2 {
                    for frame in whole.chunks_exact(2) {
                        assert_eq!(frame[1], -frame[0] * 0.5, "channels were mixed");
                    }
                }
                if from == to {
                    assert_eq!(whole, input);
                }
            }
        }
    }
}

#[test]
fn reset_removes_history_and_phase_without_rebuilding_the_filter() {
    let input = sine(22050, 997, 440.0, 0.8);
    let mut converter = Resampler::new(22050, 48000, 1);
    let mut first = [0.0; 3000];
    let count = converter.process(&input, &mut first);
    assert_eq!(count.0, input.len());
    converter.reset();
    let mut second = [0.0; 3000];
    assert_eq!(converter.process(&input, &mut second), count);
    assert_eq!(first, second);
    assert_eq!(converter.process(&input, &mut []), (0, 0));
}

fn positive_crossings(samples: &[f32]) -> usize {
    samples
        .windows(2)
        .filter(|pair| pair[0] <= 0.0 && pair[1] > 0.0)
        .count()
}

#[test]
fn fifo_copies_48000_hz_bursts_through_64_frame_pd_ticks_without_steady_underruns() {
    let mut fifo = BurstTickFifo::new(48000, 22050, 192, 1, 2);
    let mut input = [0.0; 192];
    let mut output = [0.0; 384];
    let mut captured = vec![0.0; 48000];
    let mut warm_counters = (0, 0);
    for burst in 0..500 {
        for (n, sample) in input.iter_mut().enumerate() {
            *sample = (0.8 * (TAU * 440.0 * (burst * 192 + n) as f64 / 48000.0).sin()) as f32;
        }
        fifo.push_input(&input);
        fifo.pull_output(&mut output, |in64, out128| {
            assert_eq!(in64.len(), 64);
            assert_eq!(out128.len(), 128);
            for (&sample, frame) in in64.iter().zip(out128.chunks_exact_mut(2)) {
                frame.fill(sample);
            }
        });
        for frame in output.chunks_exact(2) {
            assert_eq!(frame[0], frame[1]);
        }
        if burst == 249 {
            warm_counters = (fifo.input_underruns, fifo.output_underruns);
        }
        if burst >= 250 {
            for (n, frame) in output.chunks_exact(2).enumerate() {
                captured[(burst - 250) * 192 + n] = frame[0];
            }
        }
    }
    assert_eq!((fifo.input_underruns, fifo.output_underruns), warm_counters);
    assert!(positive_crossings(&captured).abs_diff(440) <= 1);
    assert!(dbfs(amplitude(&captured, 48000, 440.0) / 0.8).abs() < 0.5);
    assert!(residual_dbfs(&captured, 48000, 440.0) < -50.0);
}

#[test]
fn fifo_pd_generated_sine_has_stream_rate_frequency_including_large_pulls() {
    for (rate, burst) in [
        (8000u32, 32usize),
        (44100, 147),
        (48000, 192),
        (192000, 768),
    ] {
        let mut fifo = BurstTickFifo::new(rate, 22050, burst, 1, 2);
        let mut input = vec![0.0; burst];
        let mut output = vec![0.0; burst * 2];
        let mut captured = vec![0.0; rate as usize];
        let mut pd_frame = 0;
        let bursts_per_second = rate as usize / burst;
        for block in 0..bursts_per_second * 2 {
            input.fill(0.0);
            fifo.push_input(&input);
            fifo.pull_output(&mut output, |_, out128| {
                for frame in out128.chunks_exact_mut(2) {
                    let sample = (0.8 * (TAU * 440.0 * pd_frame as f64 / 22050.0).sin()) as f32;
                    frame[0] = sample;
                    frame[1] = -sample;
                    pd_frame += 1;
                }
            });
            for frame in output.chunks_exact(2) {
                assert_eq!(frame[0], -frame[1]);
            }
            if block >= bursts_per_second {
                for (n, frame) in output.chunks_exact(2).enumerate() {
                    captured[(block - bursts_per_second) * burst + n] = frame[0];
                }
            }
        }
        assert!(
            positive_crossings(&captured).abs_diff(440) <= 1,
            "rate {rate}"
        );
        assert!(
            dbfs(amplitude(&captured, rate, 440.0) / 0.8).abs() < 0.5,
            "rate {rate}"
        );
        // A caller may request more than the constructor's nominal burst: it must not spin
        // forever trying to fill a bounded ring beyond its capacity.
        let mut large = vec![0.0; burst * 20];
        fifo.pull_output(&mut large, |_, out128| out128.fill(0.25));
        assert!(large.iter().all(|sample| sample.is_finite()));
    }
}

#[test]
fn fifo_zero_fills_short_input_and_counts_ticks_not_samples() {
    let mut fifo = BurstTickFifo::new(22050, 22050, 64, 1, 2);
    fifo.push_input(&[0.5; 10]);
    let mut output = [1.0; 128];
    fifo.pull_output(&mut output, |input, output| {
        assert_eq!(&input[..10], &[0.5; 10]);
        assert!(input[10..].iter().all(|&sample| sample == 0.0));
        for (&sample, frame) in input.iter().zip(output.chunks_exact_mut(2)) {
            frame.fill(sample);
        }
    });
    assert_eq!(fifo.input_underruns, 1);
    assert_eq!(fifo.output_underruns, 0);
    assert_eq!(&output[..20], &[0.5; 20]);
    assert!(output[20..].iter().all(|&sample| sample == 0.0));
    fifo.pull_output(&mut output, |input, output| {
        assert!(input.iter().all(|&sample| sample == 0.0));
        output.fill(0.0);
    });
    assert_eq!(fifo.input_underruns, 2);
    assert!(output.iter().all(|&sample| sample == 0.0));
}

#[test]
fn realtime_resampling_and_fifo_calls_allocate_nothing_even_during_warmup_and_wraparound() {
    let mut mono = Resampler::new(48000, 22050, 1);
    let mut stereo = Resampler::new(22050, 48000, 2);
    let mut fifo = BurstTickFifo::new(48000, 22050, 192, 1, 2);
    let input = [0.25; 192];
    let pd_output = [0.25; 128];
    let mut resampled_input = [0.0; 192];
    let mut resampled_output = [0.0; 384];
    let mut stream_output = [0.0; 384];
    let guard = AllocationGuard::start();
    for _ in 0..1000 {
        mono.process(&input, &mut resampled_input);
        stereo.process(&pd_output, &mut resampled_output);
        fifo.push_input(&input);
        fifo.pull_output(&mut stream_output, |input, output| {
            for (&sample, frame) in input.iter().zip(output.chunks_exact_mut(2)) {
                frame.fill(sample);
            }
        });
    }
    mono.reset();
    stereo.reset();
    assert_eq!(guard.finish(), 0, "realtime path allocated");
}
