use anyhow::{ensure, Context};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use windows::Win32::{
    Media::Audio::*,
    System::Com::{CoCreateInstance, CLSCTX_ALL},
};

use super::mf::Runtime;
use crate::media::{AudioConfig, AudioSink, AudioSource, EncodedAudio};

#[derive(Default)]
pub struct WindowsAudio {
    running: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl AudioSource for WindowsAudio {
    fn start(&mut self, config: AudioConfig, sink: AudioSink) -> anyhow::Result<()> {
        ensure!(self.worker.is_none(), "WASAPI loopback is already started");
        ensure!(
            config.sample_rate == 48_000 && matches!(config.channels, 1 | 2),
            "WASAPI Opus capture requires 48000 Hz mono or stereo"
        );
        ensure!(
            config.bitrate > 0 && config.bitrate <= i32::MAX as u32,
            "invalid Opus bitrate"
        );
        let running = self.running.clone();
        running.store(true, Ordering::Release);
        let (ready, started) = mpsc::sync_channel(1);
        self.worker = Some(std::thread::Builder::new().name("nebula-wasapi".into()).spawn(move || {
            let result = (|| {
                let _runtime = Runtime::new()?;
                let channels = if config.channels == 1 { opus::Channels::Mono } else { opus::Channels::Stereo };
                let mut encoder = opus::Encoder::new(config.sample_rate, channels, opus::Application::Audio)?;
                encoder.set_bitrate(opus::Bitrate::Bits(config.bitrate as i32))?;
                encoder.set_inband_fec(true)?;
                encoder.set_packet_loss_perc(5)?;
                let audio = Loopback::new(config).context(
                    "WASAPI system loopback unavailable; enable a default playback device and run in the signed-in user session")?;
                let _ = ready.send(Ok(()));
                audio.pump(config, &mut encoder, &sink, &running)
            })();
            if let Err(error) = result {
                tracing::error!(%error, "Windows audio stopped");
                let _ = ready.send(Err(error));
            }
            running.store(false, Ordering::Release);
        })?);
        match started
            .recv()
            .context("WASAPI worker exited during startup")?
        {
            Ok(()) => Ok(()),
            Err(error) => {
                self.stop();
                Err(error)
            }
        }
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                tracing::error!("WASAPI worker panicked");
            }
        }
    }
}

impl Drop for WindowsAudio {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Loopback {
    client: IAudioClient,
    capture: IAudioCaptureClient,
}

impl Loopback {
    fn new(config: AudioConfig) -> anyhow::Result<Self> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let endpoint = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let client: IAudioClient = endpoint.Activate(CLSCTX_ALL, None)?;
            let format = WAVEFORMATEX {
                wFormatTag: 3,
                nChannels: config.channels,
                nSamplesPerSec: config.sample_rate,
                nAvgBytesPerSec: config.sample_rate * u32::from(config.channels) * 4,
                nBlockAlign: config.channels * 4,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            // Windows' shared engine does the native remix/resample, including multichannel endpoints.
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                200_000,
                0,
                &format,
                None,
            )?;
            let capture = client.GetService()?;
            client.Start()?;
            Ok(Self { client, capture })
        }
    }

    fn pump(
        &self,
        config: AudioConfig,
        encoder: &mut opus::Encoder,
        sink: &AudioSink,
        running: &AtomicBool,
    ) -> anyhow::Result<()> {
        let mut pcm = VecDeque::new();
        let samples = config.frame_samples() * config.channels as usize;
        let mut packet = vec![0; 4000];
        let mut frame = vec![0.0f32; samples];
        let started = Instant::now();
        let mut timestamp = 0;
        while running.load(Ordering::Acquire) && !sink.is_closed() {
            unsafe {
                while self.capture.GetNextPacketSize()? > 0 {
                    let mut ptr = std::ptr::null_mut();
                    let mut count = 0;
                    let mut flags = 0;
                    self.capture.GetBuffer(&mut ptr, &mut count, &mut flags, None, None)
                        .context("WASAPI capture device was removed or invalidated; reconnect after selecting a playback device")?;
                    if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
                        pcm.clear();
                    }
                    if pcm.is_empty() {
                        timestamp = (started.elapsed().as_micros() as u64).saturating_sub(
                            u64::from(count) * 1_000_000 / u64::from(config.sample_rate),
                        );
                    }
                    let length = count as usize * config.channels as usize;
                    if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                        pcm.extend(std::iter::repeat_n(0.0, length));
                    } else if length > 0 {
                        // The negotiated format is interleaved IEEE float, not the endpoint's mix format.
                        pcm.extend(
                            std::slice::from_raw_parts(ptr.cast::<f32>(), length)
                                .iter()
                                .copied(),
                        );
                    }
                    self.capture.ReleaseBuffer(count)?;
                    if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
                        encoder.reset_state()?;
                    }
                    while pcm.len() >= samples {
                        for value in &mut frame {
                            *value = pcm.pop_front().expect("packet length checked");
                        }
                        let bytes = encoder.encode_float(&frame, &mut packet)?;
                        match sink.try_send(EncodedAudio {
                            timestamp_us: timestamp,
                            data: packet[..bytes].to_vec(),
                        }) {
                            Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {}
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return Ok(()),
                        }
                        timestamp += 20_000;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        if let Err(error) = unsafe { self.client.Stop() } {
            tracing::warn!(%error, "WASAPI stop failed");
        }
    }
}
