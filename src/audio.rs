use std::error::Error;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    Device, ErrorKind, InputCallbackInfo, OutputCallbackInfo, SampleFormat, StreamConfig,
    StreamInstant,
};

use crate::dsp::{ClockBridge, HumTracker, Noise, NoiseColor, RoomMeter, SILENCE_DB, steady_tones};

pub type AppResult<T> = Result<T, Box<dyn Error>>;
/// The latest audio problem worth showing, shared between the streams and the window.
pub type StatusSlot = Arc<Mutex<Option<String>>>;

const SCAN_SECONDS: u64 = 3;
const SCAN_LOW_HZ: f32 = 30.0;
const SCAN_HIGH_HZ: f32 = 1000.0;
const SCAN_TONES_SHOWN: usize = 3;
/// About -30 dBFS. IEMs sit in the ear canal, so the anti-hum never gets louder than this.
const OUTPUT_CEILING: f32 = 0.03;
/// Mic-to-ear delay to hold; it must cover the output buffer plus one input packet.
const BRIDGE_DELAY_SECONDS: f64 = 0.1;
/// Masking noise RMS never goes above this, for the same in-ear reason.
pub const MASK_CEILING_DB: f32 = -30.0;
/// Hard clip for brown-noise peaks, about -20 dBFS.
const MASK_PEAK: f32 = 0.1;

#[derive(Default)]
pub struct Tuning {
    pub phase_deg: f32,
    pub gain_db: f32,
}

impl Tuning {
    fn shift_and_gain(&self) -> (f32, f32) {
        (self.phase_deg.to_radians(), 10f32.powf(self.gain_db / 20.0))
    }
}

pub struct Masking {
    pub room_db: f32,
    pub offset_db: f32,
    /// The user's own ceiling; the hard cap still applies above it.
    pub max_db: f32,
    pub color: NoiseColor,
}

impl Default for Masking {
    fn default() -> Self {
        Self {
            room_db: SILENCE_DB,
            offset_db: 0.0,
            max_db: MASK_CEILING_DB,
            color: NoiseColor::Brown,
        }
    }
}

impl Masking {
    pub fn level_db(&self) -> f32 {
        (self.room_db + self.offset_db)
            .min(self.max_db)
            .min(MASK_CEILING_DB)
    }
}

/// Running audio; dropping it stops both streams.
pub struct Engine {
    _input: cpal::Stream,
    _output: cpal::Stream,
}

pub fn mic_names() -> Vec<String> {
    names(cpal::default_host().input_devices())
}

pub fn output_names() -> Vec<String> {
    names(cpal::default_host().output_devices())
}

pub fn default_mic_name() -> Option<String> {
    name_of(&cpal::default_host().default_input_device()?)
}

pub fn default_output_name() -> Option<String> {
    name_of(&cpal::default_host().default_output_device()?)
}

fn names<E>(devices: Result<impl Iterator<Item = Device>, E>) -> Vec<String> {
    devices
        .map(|devices| devices.filter_map(|device| name_of(&device)).collect())
        .unwrap_or_default()
}

fn name_of(device: &Device) -> Option<String> {
    Some(device.description().ok()?.name().to_owned())
}

fn find_device(mut devices: impl Iterator<Item = Device>, name: &str) -> AppResult<Device> {
    let device = devices.find(|device| name_of(device).as_deref() == Some(name));
    Ok(device.ok_or(format!("{name} is not connected"))?)
}

struct DevicePair {
    mic: Device,
    mic_config: StreamConfig,
    out: Device,
    out_config: StreamConfig,
}

fn open_pair(mic_name: &str, out_name: &str) -> AppResult<DevicePair> {
    let host = cpal::default_host();
    let mic = find_device(host.input_devices()?, mic_name)?;
    let out = find_device(host.output_devices()?, out_name)?;
    let mic_config = mic.default_input_config()?;
    let out_config = out.default_output_config()?;
    if mic_config.sample_rate() != out_config.sample_rate() {
        return Err(format!(
            "{mic_name} runs at {} Hz but {out_name} at {} Hz; set both to 48000 Hz in Windows Sound settings",
            mic_config.sample_rate(),
            out_config.sample_rate()
        )
        .into());
    }
    if mic_config.sample_format() != SampleFormat::F32
        || out_config.sample_format() != SampleFormat::F32
    {
        return Err("expected 32-bit float audio, the Windows shared-mode default".into());
    }
    Ok(DevicePair {
        mic,
        mic_config: mic_config.into(),
        out,
        out_config: out_config.into(),
    })
}

pub fn start_masking(
    mic_name: &str,
    out_name: &str,
    masking: Arc<Mutex<Masking>>,
    status: StatusSlot,
) -> AppResult<Engine> {
    let pair = open_pair(mic_name, out_name)?;
    let input = start_room_meter(
        &pair.mic,
        pair.mic_config,
        Arc::clone(&masking),
        Arc::clone(&status),
    )?;
    let output = start_masking_noise(&pair.out, pair.out_config, masking, status)?;
    input.play()?;
    output.play()?;
    Ok(Engine {
        _input: input,
        _output: output,
    })
}

pub fn start_hum(
    mic_name: &str,
    out_name: &str,
    hum_hz: f32,
    tuning: Arc<Mutex<Tuning>>,
    status: StatusSlot,
) -> AppResult<Engine> {
    let pair = open_pair(mic_name, out_name)?;
    let sample_rate = f64::from(pair.mic_config.sample_rate);
    let bridge = Arc::new(Mutex::new(ClockBridge::new(
        sample_rate,
        (BRIDGE_DELAY_SECONDS * sample_rate) as usize,
    )));
    let input = start_anti_hum(
        &pair.mic,
        pair.mic_config,
        hum_hz,
        tuning,
        Arc::clone(&bridge),
        Arc::clone(&status),
    )?;
    let output = start_playback(&pair.out, pair.out_config, bridge, status)?;
    input.play()?;
    output.play()?;
    Ok(Engine {
        _input: input,
        _output: output,
    })
}

/// Listens for a few seconds and returns the clearest steady tones as (hz, dB above their surroundings).
pub fn scan_for_hum(mic_name: &str) -> AppResult<Vec<(f32, f32)>> {
    let mic = find_device(cpal::default_host().input_devices()?, mic_name)?;
    let config: StreamConfig = mic.default_input_config()?.into();
    let recording = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&recording);
    let channels = usize::from(config.channels);
    let stream = mic.build_input_stream(
        config,
        move |data: &[f32], _: &InputCallbackInfo| {
            sink.lock().unwrap().extend(data.iter().step_by(channels));
        },
        |_| {},
        None,
    )?;
    stream.play()?;
    thread::sleep(Duration::from_secs(SCAN_SECONDS));
    drop(stream);

    let samples = recording.lock().unwrap();
    let tones = steady_tones(
        &samples,
        config.sample_rate as f32,
        SCAN_LOW_HZ,
        SCAN_HIGH_HZ,
    );
    if tones.is_empty() {
        return Err("no hum found: nothing stands 20 dB above the background noise".into());
    }
    Ok(tones.into_iter().take(SCAN_TONES_SHOWN).collect())
}

fn start_room_meter(
    mic: &Device,
    config: StreamConfig,
    masking: Arc<Mutex<Masking>>,
    status: StatusSlot,
) -> AppResult<cpal::Stream> {
    let channels = usize::from(config.channels);
    let mut meter = RoomMeter::new(config.sample_rate as f32);
    let stream = mic.build_input_stream(
        config,
        move |data: &[f32], _: &InputCallbackInfo| {
            for &sample in data.iter().step_by(channels) {
                meter.measure(if sample.is_finite() { sample } else { 0.0 });
            }
            masking.lock().unwrap().room_db = meter.level_db();
        },
        report_to(status),
        None,
    )?;
    Ok(stream)
}

fn start_masking_noise(
    out: &Device,
    config: StreamConfig,
    masking: Arc<Mutex<Masking>>,
    status: StatusSlot,
) -> AppResult<cpal::Stream> {
    let channels = usize::from(config.channels);
    let mut noise = Noise::default();
    let stream = out.build_output_stream(
        config,
        move |data: &mut [f32], _: &OutputCallbackInfo| {
            let (amplitude, color) = {
                let masking = masking.lock().unwrap();
                (10f32.powf(masking.level_db() / 20.0), masking.color)
            };
            for frame in data.chunks_mut(channels) {
                let sample = amplitude * noise.next_sample(color);
                frame.fill(sample.clamp(-MASK_PEAK, MASK_PEAK));
            }
        },
        report_to(status),
        None,
    )?;
    Ok(stream)
}

fn start_anti_hum(
    mic: &Device,
    config: StreamConfig,
    hum_hz: f32,
    tuning: Arc<Mutex<Tuning>>,
    bridge: Arc<Mutex<ClockBridge>>,
    status: StatusSlot,
) -> AppResult<cpal::Stream> {
    let channels = usize::from(config.channels);
    let sample_rate = f64::from(config.sample_rate);
    let mut tracker = HumTracker::new(hum_hz, config.sample_rate as f32);
    let stream = mic.build_input_stream(
        config,
        move |data: &[f32], info: &InputCallbackInfo| {
            let (shift, gain) = tuning.lock().unwrap().shift_and_gain();
            let frames = data.len() / channels;
            let newest_captured_at =
                seconds(info.timestamp().capture) + frames.saturating_sub(1) as f64 / sample_rate;
            let anti_hum = data.iter().step_by(channels).map(|&sample| {
                let sample = if sample.is_finite() { sample } else { 0.0 };
                gain * tracker.track(sample, shift)
            });
            bridge.lock().unwrap().push(anti_hum, newest_captured_at);
        },
        report_to(status),
        None,
    )?;
    Ok(stream)
}

fn start_playback(
    out: &Device,
    config: StreamConfig,
    bridge: Arc<Mutex<ClockBridge>>,
    status: StatusSlot,
) -> AppResult<cpal::Stream> {
    let channels = usize::from(config.channels);
    let stream = out.build_output_stream(
        config,
        move |data: &mut [f32], info: &OutputCallbackInfo| {
            let mut bridge = bridge.lock().unwrap();
            bridge.correct_drift(seconds(info.timestamp().playback));
            for frame in data.chunks_mut(channels) {
                frame.fill(bridge.pop().clamp(-OUTPUT_CEILING, OUTPUT_CEILING));
            }
        },
        report_to(status),
        None,
    )?;
    Ok(stream)
}

fn seconds(instant: StreamInstant) -> f64 {
    instant.as_nanos() as f64 * 1e-9
}

fn report_to(status: StatusSlot) -> impl FnMut(cpal::Error) + Send + 'static {
    move |err| {
        // Every stream start logs one underrun; it is harmless.
        if !matches!(err.kind(), ErrorKind::Xrun) {
            *status.lock().unwrap() = Some(err.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masking_never_exceeds_the_users_max_or_the_hard_cap() {
        let loud_room = Masking {
            room_db: -40.0,
            offset_db: 30.0,
            max_db: -45.0,
            ..Masking::default()
        };
        let max_set_above_cap = Masking {
            max_db: -10.0,
            ..loud_room
        };

        assert_eq!(loud_room.level_db(), -45.0);
        assert_eq!(max_set_above_cap.level_db(), MASK_CEILING_DB);
    }
}
