//! Android `cpal` audio output (AAudio / Oboe backend): the playback master clock.
//!
//! Settings ▸ Audio Hardware picks the host ("Device Class"), the output device, the I/O buffer
//! size, and the sample rate. A stream the device drops while playing (Bluetooth headphone switch,
//! Android audio focus interruption, or underrun) is restarted by [`AudioOut::recover`], keeping
//! the played-frame clock and the play-ahead mixer so sound resumes cleanly.

#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)
)]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, TryLockError};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use filmcraft_engine::settings::AudioHardwarePrefs;
use filmcraft_ui_egui::{AudioDevices, AudioOut};

/// The playback mixer callback handed to [`AudioOut::start`].
type Fill = Box<dyn FnMut(&mut [f32], usize) + Send>;

pub struct CpalOut {
    stream: Option<cpal::Stream>,
    fill: Option<Arc<Mutex<Fill>>>,
    played: Arc<AtomicU64>,
    health: Arc<Health>,
    watchdog: Watchdog,
    epoch: Instant,
    rate: u32,
    channels: u16,
    hw: AudioHardwarePrefs,
    document_rate: Option<u32>,
    note: Option<Note>,
    fallback: Mutex<Option<(String, String)>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Note {
    Fallback(String),
    NoDevice,
}

impl Note {
    fn text(&self) -> String {
        use filmcraft_ui_egui::i18n::{fmt, t};
        match self {
            Note::Fallback(device) => fmt(
                t("Audio hardware: the default output device cannot be opened; using '{device}'"),
                &[("device", device)],
            ),
            Note::NoDevice => t("No audio output device found: playing without sound (check Settings ▸ Audio Hardware)").to_string(),
        }
    }
}

fn host(name: &str) -> cpal::Host {
    if !name.is_empty()
        && let Some(id) = cpal::available_hosts().into_iter().find(|h| h.name() == name)
        && let Ok(h) = cpal::host_from_id(id)
    {
        return h;
    }
    cpal::default_host()
}

struct DeviceChoice {
    device: cpal::Device,
    fallback: Option<Note>,
}

fn fallback_rank(name: &str) -> u8 {
    let n = name.to_ascii_lowercase();
    if n == "aaudio" || n == "oboe" || n == "pipewire" || n.starts_with("pipewire:") {
        0
    } else if n == "pulse" || n.starts_with("pulse:") {
        1
    } else if n.starts_with("hw:") || n.starts_with("plughw:") {
        3
    } else {
        2
    }
}

fn output_device(
    h: &cpal::Host,
    name: &str,
    cache: &Mutex<Option<(String, String)>>,
) -> Option<DeviceChoice> {
    if !name.is_empty()
        && let Ok(mut devs) = h.output_devices()
        && let Some(d) = devs.find(|d| d.name().is_ok_and(|n| n == name))
    {
        return Some(DeviceChoice { device: d, fallback: None });
    }
    if let Some(d) = h.default_output_device()
        && d.default_output_config().is_ok()
    {
        return Some(DeviceChoice { device: d, fallback: None });
    }
    let host_name = h.id().name().to_string();
    let mut cache = cache.lock().unwrap_or_else(PoisonError::into_inner);
    let mut devs: Vec<(String, cpal::Device)> = h
        .output_devices()
        .ok()?
        .filter_map(|d| d.name().ok().map(|n| (n, d)))
        .collect();
    if let Some((cached_host, cached)) = cache.as_ref()
        && *cached_host == host_name
        && let Some(i) = devs.iter().position(|(n, _)| n == cached)
    {
        let (found, d) = devs.swap_remove(i);
        return Some(DeviceChoice {
            device: d,
            fallback: Some(Note::Fallback(found)),
        });
    }
    devs.sort_by_key(|(n, _)| fallback_rank(n));
    let (found, d) = devs.into_iter().find(|(_, d)| d.default_output_config().is_ok())?;
    log::warn!("the default audio output device cannot be opened; falling back to '{found}'");
    *cache = Some((host_name, found.clone()));
    Some(DeviceChoice {
        device: d,
        fallback: Some(Note::Fallback(found)),
    })
}

impl Default for CpalOut {
    fn default() -> Self {
        Self::new()
    }
}

impl CpalOut {
    #[must_use]
    pub fn new() -> Self {
        let host = cpal::default_host();
        let fallback = Mutex::new(None);
        let choice = output_device(&host, "", &fallback);
        let cfg = choice.as_ref().and_then(|c| c.device.default_output_config().ok());
        let note = match choice {
            Some(c) => c.fallback,
            None => {
                log::warn!("no audio output device found");
                Some(Note::NoDevice)
            }
        };
        Self {
            stream: None,
            fill: None,
            played: Arc::new(AtomicU64::new(0)),
            health: Arc::new(Health::default()),
            watchdog: Watchdog::default(),
            epoch: Instant::now(),
            rate: cfg.as_ref().map_or(48_000, |c| c.sample_rate().0),
            channels: cfg.as_ref().map_or(2, |c| c.channels()),
            hw: AudioHardwarePrefs::default(),
            document_rate: None,
            note,
            fallback,
        }
    }

    fn config(&self, dev: &cpal::Device) -> Result<cpal::SupportedStreamConfig, String> {
        let default = dev.default_output_config().map_err(|e| e.to_string())?;
        let want = if self.hw.force_document_rate {
            self.document_rate.unwrap_or(self.hw.sample_rate)
        } else {
            self.hw.sample_rate
        };
        let exact = dev.supported_output_configs().ok().and_then(|it| {
            it.filter(|c| c.channels() > 0 && (c.min_sample_rate().0..=c.max_sample_rate().0).contains(&want))
                .max_by_key(|c| {
                    (
                        c.channels() == default.channels(),
                        c.sample_format() == cpal::SampleFormat::F32,
                        c.sample_format() == default.sample_format(),
                    )
                })
                .and_then(|c| c.try_with_sample_rate(cpal::SampleRate(want)))
        });
        Ok(exact.unwrap_or(default))
    }

    fn converted_stream<T>(
        &self,
        dev: &cpal::Device,
        config: &cpal::StreamConfig,
        fill: Arc<Mutex<Fill>>,
    ) -> Result<cpal::Stream, cpal::BuildStreamError>
    where
        T: cpal::SizedSample + cpal::FromSample<f32>,
    {
        let channels = usize::from(config.channels);
        let played = self.played.clone();
        let health = self.health.clone();
        let error_health = health.clone();
        let mut scratch = Vec::new();
        dev.build_output_stream(
            config,
            move |buf: &mut [T], _| {
                if buf.len() > 1_048_576 || scratch.try_reserve(buf.len().saturating_sub(scratch.len())).is_err() {
                    buf.fill(T::EQUILIBRIUM);
                    if !health.failed.swap(true, Ordering::SeqCst) {
                        log::error!("filmcraft-android: unable to allocate the audio conversion buffer");
                    }
                    return;
                }
                scratch.resize(buf.len(), 0.0);
                render(&fill, &mut scratch, channels, &played, &health);
                convert_samples(&scratch, buf);
            },
            move |e| stream_error(&error_health, e),
            None,
        )
    }

    fn open(&mut self, fill: Arc<Mutex<Fill>>, expect: Option<(u32, u16)>) -> Result<(), String> {
        let host = host(&self.hw.device_class);
        let DeviceChoice { device: dev, fallback } =
            output_device(&host, &self.hw.default_output, &self.fallback).ok_or("no output device")?;
        self.note = fallback;
        let cfg = self.config(&dev)?;
        let channels = cfg.channels() as usize;
        if channels == 0 || cfg.sample_rate().0 == 0 {
            return Err("the output device returned an invalid channel count or sample rate".into());
        }
        if let Some((rate, ch)) = expect
            && (rate, ch) != (cfg.sample_rate().0, cfg.channels())
        {
            return Err(format!(
                "the output device now runs at {} Hz with {} channels",
                cfg.sample_rate().0,
                cfg.channels()
            ));
        }
        let mut config: cpal::StreamConfig = cfg.clone().into();
        if let cpal::SupportedBufferSize::Range { min, max } = cfg.buffer_size()
            && (*min..=*max).contains(&self.hw.buffer_size)
        {
            config.buffer_size = cpal::BufferSize::Fixed(self.hw.buffer_size);
        }
        self.rate = cfg.sample_rate().0;
        self.channels = cfg.channels();
        let stream = match cfg.sample_format() {
            cpal::SampleFormat::F32 => {
                let played = self.played.clone();
                let health = self.health.clone();
                let error_health = health.clone();
                dev.build_output_stream(
                    &config,
                    move |buf: &mut [f32], _| render(&fill, buf, channels, &played, &health),
                    move |e| stream_error(&error_health, e),
                    None,
                )
            }
            cpal::SampleFormat::I8 => self.converted_stream::<i8>(&dev, &config, fill),
            cpal::SampleFormat::I16 => self.converted_stream::<i16>(&dev, &config, fill),
            cpal::SampleFormat::I24 => self.converted_stream::<cpal::I24>(&dev, &config, fill),
            cpal::SampleFormat::I32 => self.converted_stream::<i32>(&dev, &config, fill),
            cpal::SampleFormat::I64 => self.converted_stream::<i64>(&dev, &config, fill),
            cpal::SampleFormat::U8 => self.converted_stream::<u8>(&dev, &config, fill),
            cpal::SampleFormat::U16 => self.converted_stream::<u16>(&dev, &config, fill),
            cpal::SampleFormat::U32 => self.converted_stream::<u32>(&dev, &config, fill),
            cpal::SampleFormat::U64 => self.converted_stream::<u64>(&dev, &config, fill),
            cpal::SampleFormat::F64 => self.converted_stream::<f64>(&dev, &config, fill),
            other => return Err(format!("unsupported sample format {other:?}")),
        }
        .map_err(|e| e.to_string())?;
        stream.play().map_err(|e| e.to_string())?;
        self.stream = Some(stream);
        Ok(())
    }

    fn restart(&mut self) -> Result<(), String> {
        let fill = self.fill.clone().ok_or("nothing is playing")?;
        let expect = (self.rate, self.channels);
        self.stream = None;
        self.health.clear_errors();
        self.open(fill, Some(expect))
    }
}

#[derive(Default)]
struct Health {
    lost: AtomicBool,
    errored: AtomicBool,
    failed: AtomicBool,
    reported: AtomicBool,
}

impl Health {
    fn clear_errors(&self) {
        self.lost.store(false, Ordering::SeqCst);
        self.errored.store(false, Ordering::SeqCst);
        self.reported.store(false, Ordering::SeqCst);
    }
}

const STALL_RESTART: Duration = Duration::from_millis(300);
const MAX_RESTARTS: usize = 3;
const RESTART_WINDOW: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    Healthy,
    Restart,
    GiveUp,
}

#[derive(Default)]
struct Watchdog {
    seen: (u64, Duration),
    pending_error: bool,
    restarts: Vec<Duration>,
}

impl Watchdog {
    fn reset(&mut self, now: Duration) {
        *self = Self {
            seen: (0, now),
            ..Self::default()
        };
    }

    fn check(&mut self, now: Duration, played: u64, errored: bool, lost: bool) -> Verdict {
        if played != self.seen.0 {
            self.seen = (played, now);
            self.pending_error = false;
        }
        self.pending_error |= errored;
        let stalled = now.saturating_sub(self.seen.1) >= STALL_RESTART;
        if !lost && !(self.pending_error && stalled) {
            return Verdict::Healthy;
        }
        self.restarts.retain(|t| now.saturating_sub(*t) < RESTART_WINDOW);
        if self.restarts.len() >= MAX_RESTARTS {
            return Verdict::GiveUp;
        }
        self.restarts.push(now);
        self.seen = (played, now);
        self.pending_error = false;
        Verdict::Restart
    }
}

fn render(fill: &Mutex<Fill>, buf: &mut [f32], channels: usize, played: &AtomicU64, health: &Health) {
    let mut guard = match fill.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(TryLockError::WouldBlock) => {
            buf.fill(0.0);
            return;
        }
    };
    if health.failed.load(Ordering::SeqCst) || catch_unwind(AssertUnwindSafe(|| (*guard)(buf, channels))).is_err() {
        buf.fill(0.0);
        if !health.failed.swap(true, Ordering::SeqCst) {
            log::error!("filmcraft-android: the audio mixer failed; playing without sound");
        }
        return;
    }
    played.fetch_add((buf.len() / channels.max(1)) as u64, Ordering::SeqCst);
}

fn stream_error(health: &Health, e: cpal::StreamError) {
    match e {
        cpal::StreamError::DeviceNotAvailable => health.lost.store(true, Ordering::SeqCst),
        cpal::StreamError::BackendSpecific { .. } => health.errored.store(true, Ordering::SeqCst),
    }
    if !health.reported.swap(true, Ordering::SeqCst) {
        log::warn!("filmcraft-android: audio stream error: {e}");
    }
}

fn convert_samples<T: cpal::Sample + cpal::FromSample<f32>>(input: &[f32], output: &mut [T]) {
    let upper = f32::from_bits(1.0_f32.to_bits() - 1);
    for (sample, converted) in input.iter().zip(output) {
        *converted = T::from_sample(if sample.is_finite() { sample.clamp(-1.0, upper) } else { 0.0 });
    }
}

impl AudioOut for CpalOut {
    fn start(&mut self, fill: Box<dyn FnMut(&mut [f32], usize) + Send>) -> Result<u32, String> {
        self.stop();
        self.played.store(0, Ordering::SeqCst);
        self.health = Arc::new(Health::default());
        self.watchdog.reset(self.epoch.elapsed());
        let fill = Arc::new(Mutex::new(fill));
        self.open(fill.clone(), None)?;
        self.fill = Some(fill);
        Ok(self.rate)
    }

    fn stop(&mut self) {
        self.stream = None;
        self.fill = None;
    }

    fn recover(&mut self) {
        if self.stream.is_none() || self.health.failed.load(Ordering::SeqCst) {
            return;
        }
        let lost = self.health.lost.swap(false, Ordering::SeqCst);
        let errored = self.health.errored.swap(false, Ordering::SeqCst);
        match self.watchdog.check(
            self.epoch.elapsed(),
            self.played.load(Ordering::SeqCst),
            errored,
            lost,
        ) {
            Verdict::Healthy => {}
            Verdict::Restart => {
                log::warn!(
                    "the audio output stream {}; restarting it",
                    if lost { "lost its device" } else { "stopped after an error" }
                );
                if let Err(e) = self.restart() {
                    log::warn!("the audio output stream could not be restarted: {e}");
                    self.stream = None;
                    self.health.failed.store(true, Ordering::SeqCst);
                }
            }
            Verdict::GiveUp => {
                log::warn!(
                    "the audio output stream keeps failing ({MAX_RESTARTS} restarts within {} s)",
                    RESTART_WINDOW.as_secs()
                );
                self.health.failed.store(true, Ordering::SeqCst);
            }
        }
    }

    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn channels(&self) -> usize {
        self.channels as usize
    }

    fn played_frames(&self) -> Option<u64> {
        self.stream
            .as_ref()
            .filter(|_| !self.health.failed.load(Ordering::SeqCst))
            .map(|_| self.played.load(Ordering::SeqCst))
    }

    fn devices(&self) -> AudioDevices {
        let h = host(&self.hw.device_class);
        let outputs = h
            .output_devices()
            .map(|d| d.filter_map(|x| x.name().ok()).collect())
            .unwrap_or_default();
        let inputs = h
            .input_devices()
            .map(|d| d.filter_map(|x| x.name().ok()).collect())
            .unwrap_or_default();
        let output_channels = output_device(&h, &self.hw.default_output, &self.fallback)
            .and_then(|c| self.config(&c.device).ok())
            .map_or(0, |cfg| cfg.channels());
        AudioDevices {
            hosts: cpal::available_hosts().iter().map(|h| h.name().to_string()).collect(),
            inputs,
            outputs,
            output_channels,
        }
    }

    fn configure(&mut self, hw: &AudioHardwarePrefs, document_rate: Option<u32>) {
        self.hw = hw.clone();
        self.document_rate = document_rate;
        let host = host(&self.hw.device_class);
        if let Some(choice) = output_device(&host, &self.hw.default_output, &self.fallback)
            && let Ok(cfg) = self.config(&choice.device)
        {
            self.rate = cfg.sample_rate().0;
            self.channels = cfg.channels();
            self.note = choice.fallback;
        }
    }

    fn note(&self) -> Option<String> {
        self.note.as_ref().map(Note::text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_conversion_clips_and_preserves_channels() {
        let mut signed = [0_i16; 4];
        convert_samples(&[-2.0, 2.0, 0.0, f32::NAN], &mut signed);
        assert_eq!(signed, [i16::MIN, i16::MAX, 0, 0]);
    }
}
