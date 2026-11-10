use std::collections::VecDeque;
use std::f64::consts::TAU;

const SCAN_STEP_HZ: f32 = 0.5;

/// Steady tones between `lo` and `hi` Hz as (hz, dB below the loudest), loudest first.
pub fn loudest_tones(samples: &[f32], sample_rate: f32, lo: f32, hi: f32) -> Vec<(f32, f32)> {
    let len = samples.len() as f64;
    // Hann window keeps low-frequency rumble from leaking into the scan range.
    let windowed: Vec<f64> = samples
        .iter()
        .enumerate()
        .map(|(i, &s)| f64::from(s) * (0.5 - 0.5 * (TAU * i as f64 / len).cos()))
        .collect();
    let freqs: Vec<f32> = (0..)
        .map(|i| lo + i as f32 * SCAN_STEP_HZ)
        .take_while(|&f| f <= hi)
        .collect();
    let power: Vec<f64> = freqs
        .iter()
        .map(|&f| goertzel_power(&windowed, f64::from(f / sample_rate)))
        .collect();

    let mut peaks: Vec<(f32, f64)> = (1..power.len().saturating_sub(1))
        .filter(|&i| power[i] > power[i - 1] && power[i] >= power[i + 1])
        .map(|i| (freqs[i], power[i]))
        .collect();
    peaks.sort_by(|a, b| b.1.total_cmp(&a.1));
    let loudest = peaks.first().map_or(1.0, |&(_, p)| p);
    peaks
        .into_iter()
        .map(|(hz, p)| (hz, (10.0 * (p / loudest).log10()) as f32))
        .collect()
}

/// Settles in ~4000 samples (80 ms at 48 kHz) and follows a hum up to ~1 Hz off the set pitch.
const LMS_STEP: f32 = 0.0005;

/// Locks onto one hum tone in the mic signal and replays it phase-shifted.
pub struct HumTracker {
    phase: f32,
    phase_step: f32,
    sin_weight: f32,
    cos_weight: f32,
}

impl HumTracker {
    pub fn new(hz: f32, sample_rate: f32) -> Self {
        Self {
            phase: 0.0,
            phase_step: std::f32::consts::TAU * hz / sample_rate,
            sin_weight: 0.0,
            cos_weight: 0.0,
        }
    }

    /// Feeds one mic sample and returns the tracked hum shifted by `shift` radians.
    pub fn track(&mut self, mic: f32, shift: f32) -> f32 {
        let (sin, cos) = self.phase.sin_cos();
        let error = mic - (self.sin_weight * sin + self.cos_weight * cos);
        self.sin_weight += LMS_STEP * error * sin;
        self.cos_weight += LMS_STEP * error * cos;

        let (sin, cos) = (self.phase + shift).sin_cos();
        self.phase = (self.phase + self.phase_step) % std::f32::consts::TAU;
        self.sin_weight * sin + self.cos_weight * cos
    }
}

/// Per output callback; at 100 callbacks/s this averages timestamp noise over ~2 s.
const DELAY_SMOOTHING: f64 = 0.005;
const DELAY_TOLERANCE: f64 = 2.0;

/// Carries samples from the mic's clock to the output's clock at a fixed mic-to-ear delay.
pub struct ClockBridge {
    queue: VecDeque<f32>,
    sample_rate: f64,
    target_delay: f64,
    smoothed_delay: f64,
    delay_readings: u32,
    newest_captured_at: f64,
}

impl ClockBridge {
    pub fn new(sample_rate: f64, target_delay: usize) -> Self {
        Self {
            queue: VecDeque::from(vec![0.0; target_delay]),
            sample_rate,
            target_delay: target_delay as f64,
            smoothed_delay: target_delay as f64,
            delay_readings: 0,
            newest_captured_at: 0.0,
        }
    }

    /// Queues one input callback's samples. Timestamps are seconds on the audio devices' shared clock.
    pub fn push(&mut self, samples: impl IntoIterator<Item = f32>, newest_captured_at: f64) {
        self.queue.extend(samples);
        self.newest_captured_at = newest_captured_at;
    }

    /// Call once per output callback before `pop`, with the time the next popped sample will play.
    pub fn correct_drift(&mut self, plays_at: f64) {
        let delay =
            self.queue.len() as f64 + (plays_at - self.newest_captured_at) * self.sample_rate;
        // Plain running average until it is as steady as the slow smoother, so startup settles fast.
        self.delay_readings = self.delay_readings.saturating_add(1);
        let weight = (1.0 / f64::from(self.delay_readings)).max(DELAY_SMOOTHING);
        self.smoothed_delay += weight * (delay - self.smoothed_delay);

        let excess = (self.smoothed_delay - self.target_delay).round();
        if excess.abs() <= DELAY_TOLERANCE {
            return;
        }
        if excess > 0.0 {
            let surplus = (excess as usize).min(self.queue.len());
            self.queue.drain(..surplus);
        } else {
            let repeated = self.queue.front().copied().unwrap_or(0.0);
            for _ in 0..(-excess) as usize {
                self.queue.push_front(repeated);
            }
        }
        self.smoothed_delay -= excess;
    }

    pub fn pop(&mut self) -> f32 {
        self.queue.pop_front().unwrap_or(0.0)
    }
}

/// The leak keeps the integrator from drifting; it puts the bass corner near 15 Hz at 48 kHz.
const BROWN_LEAK: f32 = 0.998;

/// Deep "waterfall" masking noise: power falls 6 dB per octave.
#[derive(Default)]
pub struct BrownNoise {
    seed: u32,
    value: f32,
}

impl BrownNoise {
    /// Next sample at roughly unit RMS.
    pub fn next_sample(&mut self) -> f32 {
        self.seed = self
            .seed
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223);
        let white = self.seed as f32 / u32::MAX as f32 * 2.0 - 1.0;
        self.value = BROWN_LEAK * self.value + white;
        // Uniform white noise has variance 1/3; this scales the leaky sum back to unit RMS.
        self.value * (3.0 * (1.0 - BROWN_LEAK * BROWN_LEAK)).sqrt()
    }
}

/// Rumble and desk thumps below this corner don't count toward room loudness.
const METER_HIGHPASS_HZ: f32 = 150.0;
const SLICE_SECONDS: f32 = 0.05;
/// 3 s of slices: typing and speech always pause within that, so the quietest slice is background.
const FLOOR_WINDOW_SLICES: usize = 60;
const FLOOR_SMOOTHING_SECONDS: f32 = 0.5;
pub const SILENCE_DB: f32 = -120.0;

/// Background loudness in dBFS: the quietest 50 ms slice of the last 3 s, lightly smoothed.
pub struct RoomMeter {
    highpass: [Highpass; 2],
    slice_len: usize,
    slice_energy: f32,
    slice_fill: usize,
    recent_slices_db: VecDeque<f32>,
    smoothing: f32,
    level_db: f32,
}

impl RoomMeter {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            highpass: [Highpass::new(METER_HIGHPASS_HZ, sample_rate); 2],
            slice_len: (SLICE_SECONDS * sample_rate) as usize,
            slice_energy: 0.0,
            slice_fill: 0,
            recent_slices_db: VecDeque::with_capacity(FLOOR_WINDOW_SLICES),
            smoothing: -(-SLICE_SECONDS / FLOOR_SMOOTHING_SECONDS).exp_m1(),
            level_db: SILENCE_DB,
        }
    }

    pub fn measure(&mut self, mic: f32) {
        let audible = self
            .highpass
            .iter_mut()
            .fold(mic, |x, stage| stage.process(x));
        self.slice_energy += audible * audible;
        self.slice_fill += 1;
        if self.slice_fill == self.slice_len {
            self.close_slice();
        }
    }

    fn close_slice(&mut self) {
        let slice_db = 10.0
            * (self.slice_energy / self.slice_len as f32)
                .max(1e-12)
                .log10();
        self.slice_energy = 0.0;
        self.slice_fill = 0;
        if self.recent_slices_db.len() == FLOOR_WINDOW_SLICES {
            self.recent_slices_db.pop_front();
        }
        self.recent_slices_db.push_back(slice_db);
        let floor_db = self
            .recent_slices_db
            .iter()
            .copied()
            .fold(f32::INFINITY, f32::min);
        if self.recent_slices_db.len() == 1 {
            // Start at the first reading instead of crawling up from silence.
            self.level_db = floor_db;
        } else {
            self.level_db += self.smoothing * (floor_db - self.level_db);
        }
    }

    pub fn level_db(&self) -> f32 {
        self.level_db
    }
}

#[derive(Clone, Copy)]
struct Highpass {
    coeff: f32,
    prev_in: f32,
    prev_out: f32,
}

impl Highpass {
    fn new(corner_hz: f32, sample_rate: f32) -> Self {
        let rc = 1.0 / (std::f32::consts::TAU * corner_hz);
        Self {
            coeff: rc / (rc + 1.0 / sample_rate),
            prev_in: 0.0,
            prev_out: 0.0,
        }
    }

    fn process(&mut self, x: f32) -> f32 {
        self.prev_out = self.coeff * (self.prev_out + x - self.prev_in);
        self.prev_in = x;
        self.prev_out
    }
}

fn goertzel_power(samples: &[f64], cycles_per_sample: f64) -> f64 {
    let coeff = 2.0 * (TAU * cycles_per_sample).cos();
    let (mut s1, mut s2) = (0.0, 0.0);
    for &x in samples {
        (s1, s2) = (x + coeff * s1 - s2, s1);
    }
    s1 * s1 + s2 * s2 - coeff * s1 * s2
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    const RATE: f32 = 48_000.0;

    /// Deterministic white noise in [-amp, amp].
    fn noise(seed: &mut u32, amp: f32) -> f32 {
        *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (*seed as f32 / u32::MAX as f32 * 2.0 - 1.0) * amp
    }

    #[test]
    fn finds_a_hum_buried_in_noise() {
        let mut seed = 7;
        let samples: Vec<f32> = (0..RATE as usize)
            .map(|n| 0.05 * (TAU * 175.0 * n as f32 / RATE).sin() + noise(&mut seed, 0.2))
            .collect();

        let (hz, _) = loudest_tones(&samples, RATE, 100.0, 300.0)[0];

        assert!((hz - 175.0).abs() <= 0.5, "found {hz} Hz");
    }

    #[test]
    fn best_phase_cuts_leaked_hum_by_20_db_when_tracking_slightly_off_pitch() {
        let best_db = (0..360)
            .step_by(5)
            .map(|deg| residual_db(175.4, 175.0, (deg as f32).to_radians()))
            .fold(f32::INFINITY, f32::min);

        assert!(best_db <= -20.0, "best residual {best_db:.1} dB");
    }

    /// Hum left at the ear for one phase setting, in dB relative to no anti-hum.
    fn residual_db(hum_hz: f32, tracked_hz: f32, shift: f32) -> f32 {
        // Simulated rig: the ear hears the hum 30 samples after the mic at half level,
        // and the anti-hum plays 2400 samples after the mic sample it was built from.
        const EAR_DELAY: usize = 30;
        const EAR_GAIN: f32 = 0.5;
        const LATENCY: usize = 2400;
        let total = (1.5 * RATE) as usize;
        let settled = RATE as usize;
        let hum = |n: usize| {
            0.1 * (std::f64::consts::TAU * f64::from(hum_hz) * n as f64 / f64::from(RATE)).sin()
                as f32
        };

        let mut tracker = HumTracker::new(tracked_hz, RATE);
        let mut seed = 3;
        let mut anti = Vec::with_capacity(total);
        let (mut leaked_power, mut residual_power) = (0.0, 0.0);
        for n in 0..total {
            let mic = hum(n + EAR_DELAY) + noise(&mut seed, 0.01);
            anti.push(EAR_GAIN * tracker.track(mic, shift));
            if n >= settled {
                let leaked = EAR_GAIN * hum(n);
                let residual = leaked + anti[n - LATENCY];
                leaked_power += leaked * leaked;
                residual_power += residual * residual;
            }
        }
        10.0 * (residual_power / leaked_power).log10()
    }

    #[test]
    fn bridge_holds_delay_steady_when_mic_clock_drifts_either_way() {
        for ppm in [100.0, -100.0] {
            let spread = delay_spread_samples(ppm);
            assert!(
                spread <= 6.0,
                "{ppm} ppm: delay wandered {spread:.1} samples"
            );
        }
    }

    #[test]
    fn brown_noise_is_deep_and_near_unit_level() {
        let mut brown = BrownNoise::default();
        let samples: Vec<f64> = (0..2 * RATE as usize)
            .map(|_| f64::from(brown.next_sample()))
            .collect();
        let rms_db =
            10.0 * (samples.iter().map(|s| s * s).sum::<f64>() / samples.len() as f64).log10();
        let band_power = |low_hz: f64| -> f64 {
            (0..21)
                .map(|i| goertzel_power(&samples, (low_hz + f64::from(i)) / f64::from(RATE)))
                .sum()
        };
        let tilt_db = 10.0 * (band_power(90.0) / band_power(990.0)).log10();

        assert!(rms_db.abs() <= 1.0, "level {rms_db:.1} dB");
        assert!(tilt_db >= 15.0, "100 Hz only {tilt_db:.1} dB above 1 kHz");
    }

    #[test]
    fn background_follows_a_steady_change_after_a_few_seconds() {
        let mut meter = RoomMeter::new(RATE);
        let mut seed = 5;
        let louder = 0.01 * 10f32.sqrt();

        let quiet = feed(&mut meter, 6.0, |_| noise(&mut seed, 0.01));
        let after_one_second = feed(&mut meter, 1.0, |_| noise(&mut seed, louder)) - quiet;
        let after_five_seconds = feed(&mut meter, 4.0, |_| noise(&mut seed, louder)) - quiet;
        let quiet_two_seconds = feed(&mut meter, 2.0, |_| noise(&mut seed, 0.01)) - quiet;

        assert!(
            after_one_second < 2.0,
            "moved {after_one_second:.1} dB after 1 s"
        );
        assert!(
            (after_five_seconds - 10.0).abs() <= 1.5,
            "at {after_five_seconds:.1} of 10 dB after 5 s"
        );
        assert!(
            quiet_two_seconds <= 1.5,
            "still {quiet_two_seconds:.1} dB up 2 s after the room quieted"
        );
    }

    #[test]
    fn background_ignores_typing_and_talking() {
        // (burst, period) in seconds: key clicks every 0.3 s, then 1.5 s phrases with 0.3 s pauses.
        for (burst_seconds, period_seconds) in [(0.1, 0.3), (1.5, 1.8)] {
            let mut meter = RoomMeter::new(RATE);
            let mut seed = 21;
            let burst = (burst_seconds * RATE) as usize;
            let period = (period_seconds * RATE) as usize;
            let thirty_db_louder = 0.01 * 31.6;

            let room = feed(&mut meter, 6.0, |_| noise(&mut seed, 0.01));
            let with_bursts = feed(&mut meter, 6.0, |n| {
                let amp = if n % period < burst {
                    thirty_db_louder
                } else {
                    0.01
                };
                noise(&mut seed, amp)
            }) - room;

            assert!(
                with_bursts < 3.0,
                "{burst_seconds} s bursts raised the background {with_bursts:.1} dB"
            );
        }
    }

    #[test]
    fn room_meter_ignores_low_rumble() {
        let mut meter = RoomMeter::new(RATE);
        let mut seed = 9;
        let rumble = |n: usize| 0.05 * (TAU * 40.0 * n as f32 / RATE).sin();

        let room = feed(&mut meter, 6.0, |_| noise(&mut seed, 0.01));
        let with_rumble = feed(&mut meter, 6.0, |n| noise(&mut seed, 0.01) + rumble(n)) - room;

        assert!(
            with_rumble < 2.0,
            "rumble raised the level {with_rumble:.1} dB"
        );
    }

    /// Runs `seconds` of `signal` through the meter and returns its level.
    fn feed(meter: &mut RoomMeter, seconds: f32, mut signal: impl FnMut(usize) -> f32) -> f32 {
        for n in 0..(seconds * RATE) as usize {
            meter.measure(signal(n));
        }
        meter.level_db()
    }

    /// How far the mic-to-ear delay wanders over 50 s once settled, in samples.
    fn delay_spread_samples(mic_ppm: f64) -> f64 {
        const BLOCK: usize = 480;
        // Device timestamps count from boot, not from stream start.
        const BOOT_OFFSET: f64 = 5000.0;
        let out_rate = f64::from(RATE);
        let mic_rate = out_rate * (1.0 + mic_ppm * 1e-6);
        let mut bridge = ClockBridge::new(out_rate, 2400);
        let mut seed = 11;
        let (mut captured, mut played) = (0, 0);
        let mut delays = Vec::new();

        while played < 60 * RATE as usize {
            // As measured on the Leviosa and Celest Sol: within 0.05 ms, about one in ten off by up to 0.5 ms.
            let outlier = noise(&mut seed, 1.0) > 0.8;
            let max_error = if outlier { 0.0005 } else { 0.00005 };
            let timestamp_error = f64::from(noise(&mut seed, max_error));
            let block_ready_at = (captured + BLOCK) as f64 / mic_rate;
            let plays_at = played as f64 / out_rate;
            if block_ready_at <= plays_at {
                let block = captured..captured + BLOCK;
                captured += BLOCK;
                let newest_captured_at = (captured - 1) as f64 / mic_rate;
                bridge.push(
                    block.map(|i| i as f32),
                    BOOT_OFFSET + newest_captured_at + timestamp_error,
                );
            } else {
                bridge.correct_drift(BOOT_OFFSET + plays_at + timestamp_error);
                for _ in 0..BLOCK {
                    let captured_at = f64::from(bridge.pop()) / mic_rate;
                    if played > 10 * RATE as usize {
                        delays.push(played as f64 / out_rate - captured_at);
                    }
                    played += 1;
                }
            }
        }
        let max = delays.iter().copied().fold(f64::MIN, f64::max);
        let min = delays.iter().copied().fold(f64::MAX, f64::min);
        (max - min) * out_rate
    }
}
