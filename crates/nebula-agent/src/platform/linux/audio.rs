use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::time::{Duration, Instant};

use anyhow::Context;
use gstreamer::{self as gst, prelude::*};
use gstreamer_app::AppSink;

use super::pipeline::Pipeline;
use crate::media::{AudioConfig, AudioSink, AudioSource, EncodedAudio};

#[derive(Default)]
pub(super) struct LinuxAudio {
    running: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl AudioSource for LinuxAudio {
    fn start(&mut self, config: AudioConfig, sink: AudioSink) -> anyhow::Result<()> {
        anyhow::ensure!(self.worker.is_none(), "Linux audio already started");
        anyhow::ensure!(
            config.sample_rate == 48_000 && matches!(config.channels, 1 | 2),
            "Linux system audio requires 48 kHz mono or stereo"
        );
        let channels = if config.channels == 1 {
            opus::Channels::Mono
        } else {
            opus::Channels::Stereo
        };
        let mut encoder =
            opus::Encoder::new(config.sample_rate, channels, opus::Application::Audio)?;
        encoder.set_bitrate(opus::Bitrate::Bits(i32::try_from(config.bitrate)?))?;
        encoder.set_inband_fec(true)?;
        encoder.set_packet_loss_perc(10)?;
        let pipeline = Pipeline::new(&format!(
            "pipewiresrc name=monitor do-timestamp=true \
             ! queue max-size-buffers=4 max-size-bytes=0 max-size-time=0 leaky=downstream \
             ! audioconvert ! audioresample \
             ! audio/x-raw,format=F32LE,layout=interleaved,rate=48000,channels={} \
             ! appsink name=pcm max-buffers=4 drop=false sync=false wait-on-eos=false enable-last-sample=false",
            config.channels,
        ))?;
        // WirePlumber routes Stream/Input/Audio to the default sink's monitor
        // when stream.capture.sink is true. Never fall back to a microphone.
        let properties = gst::Structure::builder("props")
            .field("media.type", "Audio")
            .field("media.category", "Capture")
            .field("media.role", "Screen")
            .field("stream.capture.sink", true)
            .build();
        pipeline
            .element("monitor")?
            .set_property("stream-properties", properties);
        let output = pipeline
            .element("pcm")?
            .downcast::<AppSink>()
            .map_err(|_| anyhow::anyhow!("audio appsink has the wrong type"))?;
        pipeline.play()?;
        self.running.store(true, Ordering::Release);
        let running = self.running.clone();
        let (ready, started) = mpsc::sync_channel(1);
        self.worker = Some(
            std::thread::Builder::new()
                .name("nebula-linux-audio".into())
                .spawn(move || {
                    if let Err(error) = pump(
                        &pipeline,
                        &output,
                        &mut encoder,
                        config,
                        &sink,
                        &running,
                        &ready,
                    ) {
                        tracing::error!(%error, "Linux system audio stopped");
                        let _ = ready.try_send(Err(error));
                    }
                    running.store(false, Ordering::Release);
                })?,
        );
        let result = started
            .recv_timeout(Duration::from_secs(7))
            .context("PipeWire default sink monitor did not produce system audio")
            .and_then(|result| result);
        if result.is_err() {
            self.stop();
        }
        result
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                tracing::error!("Linux audio worker panicked");
            }
        }
    }
}

impl Drop for LinuxAudio {
    fn drop(&mut self) {
        self.stop();
    }
}

fn pump(
    pipeline: &Pipeline,
    output: &AppSink,
    encoder: &mut opus::Encoder,
    config: AudioConfig,
    sink: &AudioSink,
    running: &AtomicBool,
    ready: &mpsc::SyncSender<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    let packet_samples = config.frame_samples() * usize::from(config.channels);
    let mut pcm = Vec::with_capacity(packet_samples);
    let mut packet = [0; 4000];
    let mut first = true;
    let mut timestamp_us = 0;
    let started = Instant::now();
    while running.load(Ordering::Acquire) && !sink.is_closed() {
        pipeline.check()?;
        let Some(sample) = output.try_pull_sample(gst::ClockTime::from_mseconds(100)) else {
            anyhow::ensure!(!output.is_eos(), "PipeWire audio monitor ended");
            anyhow::ensure!(
                !first || started.elapsed() < Duration::from_secs(5),
                "no PipeWire sink monitor audio; check WirePlumber and the default output device"
            );
            continue;
        };
        let buffer = sample.buffer().context("audio sample contains no buffer")?;
        if buffer.flags().contains(gst::BufferFlags::DISCONT) {
            pcm.clear();
        }
        let pts = buffer
            .pts()
            .context("system audio has no timestamp")?
            .useconds();
        let bytes = buffer.map_readable()?;
        anyhow::ensure!(
            bytes.len() % (4 * usize::from(config.channels)) == 0,
            "unaligned PCM audio buffer"
        );
        for (index, bytes) in bytes.as_slice().chunks_exact(4).enumerate() {
            if pcm.is_empty() {
                timestamp_us =
                    pts + (index / usize::from(config.channels)) as u64 * 1_000_000 / 48_000;
            }
            pcm.push(f32::from_le_bytes(bytes.try_into()?));
            if pcm.len() == packet_samples {
                let size = encoder.encode_float(&pcm, &mut packet)?;
                match sink.try_send(EncodedAudio {
                    timestamp_us,
                    data: packet[..size].to_vec(),
                }) {
                    Ok(()) => {
                        if first {
                            let _ = ready.send(Ok(()));
                            first = false;
                        }
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return Ok(()),
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        tracing::debug!("dropping late system audio packet");
                    }
                }
                pcm.clear();
            }
        }
    }
    Ok(())
}
