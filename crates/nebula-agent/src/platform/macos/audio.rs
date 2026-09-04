//! System audio capture and Opus encoding on macOS.
//!
//! ScreenCaptureKit is the only supported way to tap the system mixer since
//! macOS 13: there is no public device to open, and the kernel extensions
//! people used to install for this stopped loading. So audio comes from the
//! same framework as the picture, on its own stream.
//!
//! # Why a second stream
//!
//! A `SCStream` can carry both, but the two have nothing else in common:
//! audio must not stop when the screen is still, and video must not wait for
//! a mixer that has nothing to say. Keeping them apart means neither can
//! stall the other, at the cost of a second capture session whose video side
//! is configured down to almost nothing.
//!
//! # What goes on the wire
//!
//! Opus at 48 kHz stereo, 20 ms per packet. Not raw PCM: stereo float at
//! 48 kHz is 3 Mbit/s, which is a quarter of the video budget spent on
//! something Opus carries indistinguishably at 128 kbit/s.
//!
//! # Permission
//!
//! The same Screen Recording grant as capture — macOS treats system audio as
//! part of it, which is why the settings pane is called "Screen & System
//! Audio Recording".

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Instant;

use opus::{Application, Channels};
use screencapturekit::cm::{CMSampleBuffer, CMSampleBufferExt};
use screencapturekit::shareable_content::SCShareableContent;
use screencapturekit::stream::configuration::SCStreamConfiguration;
use screencapturekit::stream::content_filter::SCContentFilter;
use screencapturekit::stream::output_type::SCStreamOutputType;
use screencapturekit::stream::SCStream;

use crate::media::{AudioConfig, AudioSink, AudioSource, EncodedAudio};

/// How many capture buffers may wait for the encoder.
///
/// Deeper than video's queue because dropping audio is far more noticeable
/// than dropping a frame, and shallow enough that a stalled encoder cannot
/// build up a backlog anyone would rather hear than skip: eight buffers is
/// under 200 ms.
const QUEUE_DEPTH: usize = 8;

/// The smallest video ScreenCaptureKit will agree to produce for a stream
/// that only wants the audio.
const DUMMY_PIXELS: u32 = 2;

/// What to tell an operator when the mixer cannot be tapped.
const PERMISSION: &str = "cannot capture system audio. Grant Screen Recording to this binary \
     in System Settings > Privacy & Security > Screen & System Audio Recording, then run the \
     agent again";

/// Captures the system mixer and encodes it with Opus.
pub struct MacAudio {
    running: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Default for MacAudio {
    fn default() -> Self {
        Self::new()
    }
}

impl MacAudio {
    /// Create a source that has not started capturing yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
    }
}

impl AudioSource for MacAudio {
    fn start(&mut self, config: AudioConfig, sink: AudioSink) -> anyhow::Result<()> {
        let content = SCShareableContent::get()
            .map_err(|error| anyhow::anyhow!("{PERMISSION}: {error:?}"))?;
        let displays = content.displays();
        let display = displays
            .first()
            .ok_or_else(|| anyhow::anyhow!(PERMISSION))?;

        let filter = SCContentFilter::create().with_display(display).build();
        let stream_config = SCStreamConfiguration::new()
            .with_captures_audio(true)
            .with_sample_rate(config.sample_rate as i32)
            .with_channel_count(i32::from(config.channels))
            // Without this the agent would capture, send, and hear back its
            // own output on any machine where someone is also running the
            // client — a feedback loop that starts the moment both run.
            .with_excludes_current_process_audio(true)
            .with_width(DUMMY_PIXELS)
            .with_height(DUMMY_PIXELS)
            .with_fps(1);

        let (buffers, incoming) = mpsc::sync_channel::<Vec<f32>>(QUEUE_DEPTH);
        let channels = config.channels;
        let mut stream = SCStream::new(&filter, &stream_config);
        stream.add_output_handler(
            move |sample: CMSampleBuffer, kind: SCStreamOutputType| {
                if kind != SCStreamOutputType::Audio {
                    return;
                }
                if let Some(pcm) = interleave(&sample, channels) {
                    // A full queue means the encoder is behind. Discarding
                    // the newest buffer keeps the delay bounded; queueing it
                    // would only move the same gap later and add latency.
                    let _ = buffers.try_send(pcm);
                }
            },
            SCStreamOutputType::Audio,
        );

        let running = Arc::clone(&self.running);
        running.store(true, Ordering::Relaxed);

        let (ready, started) = mpsc::channel::<anyhow::Result<()>>();
        let worker = std::thread::Builder::new()
            .name("nebula-audio".into())
            .spawn(move || {
                let encoder = match encoder_for(config) {
                    Ok(encoder) => encoder,
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                if let Err(error) = stream.start_capture() {
                    let _ = ready.send(Err(anyhow::anyhow!(
                        "{PERMISSION}: ScreenCaptureKit would not start: {error:?}"
                    )));
                    return;
                }
                let _ = ready.send(Ok(()));
                pump(encoder, config, &incoming, &sink, &running);
                let _ = stream.stop_capture();
                running.store(false, Ordering::Relaxed);
            })?;

        started
            .recv()
            .map_err(|_| anyhow::anyhow!("the audio thread died before it started"))??;
        self.worker = Some(worker);
        Ok(())
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for MacAudio {
    fn drop(&mut self) {
        self.stop();
    }
}

fn encoder_for(config: AudioConfig) -> anyhow::Result<opus::Encoder> {
    let channels = match config.channels {
        1 => Channels::Mono,
        2 => Channels::Stereo,
        n => anyhow::bail!("Opus carries one or two channels, not {n}"),
    };
    // `Audio` rather than `Voip`: a remote desktop carries whatever the
    // machine is playing, and Voip mode's speech tuning audibly damages
    // music and system sounds.
    let mut encoder = opus::Encoder::new(config.sample_rate, channels, Application::Audio)?;
    encoder.set_bitrate(opus::Bitrate::Bits(config.bitrate as i32))?;
    Ok(encoder)
}

/// Encode captured audio until capture stops or the client goes away.
fn pump(
    mut encoder: opus::Encoder,
    config: AudioConfig,
    incoming: &mpsc::Receiver<Vec<f32>>,
    sink: &AudioSink,
    running: &AtomicBool,
) {
    let started = Instant::now();
    let per_packet = config.frame_samples() * config.channels as usize;
    // ScreenCaptureKit's buffers do not line up with Opus's packet size —
    // the mixer hands out 1024 samples at a time and Opus wants 960 — so
    // whatever is left over waits here for the next buffer.
    let mut pending: Vec<f32> = Vec::with_capacity(per_packet * 2);
    let mut packet = vec![0u8; 4000];

    while running.load(Ordering::Relaxed) {
        let Ok(pcm) = incoming.recv() else { return };
        pending.extend_from_slice(&pcm);

        while pending.len() >= per_packet {
            let n = match encoder.encode_float(&pending[..per_packet], &mut packet) {
                Ok(n) => n,
                Err(error) => {
                    tracing::warn!(%error, "the Opus encoder rejected a packet");
                    pending.drain(..per_packet);
                    continue;
                }
            };
            pending.drain(..per_packet);

            let frame = EncodedAudio {
                timestamp_us: started.elapsed().as_micros() as u64,
                data: packet[..n].to_vec(),
            };
            if sink.try_send(frame).is_err() && sink.is_closed() {
                return;
            }
        }
    }
}

/// Turn one capture buffer into interleaved float samples.
///
/// ScreenCaptureKit delivers planar float — one buffer per channel — but
/// Opus wants the channels interleaved, and a machine with a mono mixer
/// delivers a single buffer that is already in the right shape.
fn interleave(sample: &CMSampleBuffer, channels: u16) -> Option<Vec<f32>> {
    let list = sample.audio_buffer_list()?;
    let mut planes: Vec<Vec<f32>> = (0..list.num_buffers())
        .filter_map(|i| list.get(i))
        .map(|buffer| as_floats(buffer.data()))
        .collect();
    if planes.first().is_none_or(Vec::is_empty) {
        return None;
    }

    // One buffer holding every channel is already interleaved.
    if planes.len() == 1 {
        return Some(planes.remove(0));
    }

    let frames = planes.iter().map(|plane| plane.len()).min()?;
    let wanted = usize::from(channels);
    let mut out = Vec::with_capacity(frames * wanted);
    for frame in 0..frames {
        for channel in 0..wanted {
            // A machine with fewer channels than asked for repeats the last
            // one, which turns mono into centred stereo rather than silence
            // out of one side.
            let plane = &planes[channel.min(planes.len() - 1)];
            out.push(plane[frame]);
        }
    }
    Some(out)
}

/// Read a capture buffer's bytes as the floats they are.
///
/// Copied rather than reinterpreted in place: the pointer Core Audio hands
/// over is aligned in practice, but "in practice" is not something to build
/// a `from_raw_parts` on when the copy costs a memcpy per 20 ms of audio.
fn as_floats(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|b| f32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}
