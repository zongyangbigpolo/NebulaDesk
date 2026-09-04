//! Playing the audio a session carries.
//!
//! Two things happen here that have to stay apart: Opus packets arrive from
//! the network whenever the agent's mixer had something to say, and the
//! output device asks for samples on its own real-time thread whenever its
//! buffer runs low. Neither can wait for the other — a decode on the audio
//! callback would glitch, and a blocking send from the network loop would
//! stall the session — so a ring of decoded samples sits between them.
//!
//! # Latency
//!
//! The ring is deliberately shallow. Audio that has queued up is latency
//! that has already been committed to, and in a remote desktop the sound is
//! meant to line up with the picture. When the ring overflows the oldest
//! samples are dropped, which is heard as a click and then correct audio,
//! rather than as everything arriving progressively later.

use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ndp_proto::AudioFrameInfo;

/// How much decoded audio may wait for the output device.
///
/// 200 ms. Long enough to ride out a scheduling hiccup on either side, short
/// enough that the sound cannot drift audibly behind the picture.
const RING_MS: usize = 200;

/// Sample rate everything here runs at. Opus is defined at 48 kHz and the
/// agent encodes there, so resampling would be pure loss.
const SAMPLE_RATE: u32 = 48_000;

/// Decodes Opus packets and plays them on the default output device.
pub struct Playback {
    decoder: opus::Decoder,
    channels: usize,
    ring: Arc<Mutex<Ring>>,
    /// Set once anything has been queued, so that the device's complaint
    /// about being started with nothing to play is not reported as a fault.
    started: Arc<std::sync::atomic::AtomicBool>,
    scratch: Vec<f32>,
    // Held only to keep the device open: dropping a cpal stream stops it.
    _stream: cpal::Stream,
}

impl Playback {
    /// Open the default output device and get ready to decode.
    pub fn start(channels: u16) -> anyhow::Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow::anyhow!("this machine has no audio output device"))?;

        let channels = usize::from(channels).clamp(1, 2);
        let config = cpal::StreamConfig {
            channels: channels as u16,
            sample_rate: SAMPLE_RATE,
            // Let the host pick: asking for a specific size is how you get a
            // device that refuses to open, and the ring already bounds the
            // latency that matters.
            buffer_size: cpal::BufferSize::Default,
        };

        let ring = Arc::new(Mutex::new(Ring::new(
            SAMPLE_RATE as usize * channels * RING_MS / 1000,
        )));
        let playing = Arc::clone(&ring);
        // Opening a stream before there is any audio to put in it means the
        // device reports an underrun straight away, every single time. The
        // callback fills with silence, so nothing was lost and there is
        // nothing to act on — and a warning that always appears and never
        // matters is how people learn to stop reading warnings.
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let running = Arc::clone(&started);
        let stream = device.build_output_stream(
            config,
            move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                // This runs on the device's real-time thread. It must not
                // allocate, block, or decode — only copy out what is ready
                // and fill the rest with silence.
                match playing.lock() {
                    Ok(mut ring) => ring.drain_into(out),
                    Err(_) => out.fill(0.0),
                }
            },
            move |error| {
                if running.load(std::sync::atomic::Ordering::Relaxed) {
                    tracing::warn!(%error, "the audio output device reported an error");
                } else {
                    tracing::debug!(%error, "the audio device complained before it had anything to play");
                }
            },
            None,
        )?;
        stream.play()?;

        let decoder = opus::Decoder::new(
            SAMPLE_RATE,
            if channels == 1 {
                opus::Channels::Mono
            } else {
                opus::Channels::Stereo
            },
        )?;

        Ok(Self {
            decoder,
            channels,
            ring,
            started,
            // Opus packets top out at 120 ms; sized for the largest one so
            // decoding never needs to grow this.
            scratch: vec![0.0; SAMPLE_RATE as usize * channels * 120 / 1000],
            _stream: stream,
        })
    }

    /// Decode one payload from the audio channel and queue it for playback.
    pub fn play(&mut self, payload: &[u8]) -> anyhow::Result<()> {
        let (info, packet) = AudioFrameInfo::split(payload)?;
        if usize::from(info.channels) != self.channels {
            anyhow::bail!(
                "the agent is sending {} channels and this device was opened for {}",
                info.channels,
                self.channels
            );
        }

        let frames = self
            .decoder
            .decode_float(packet, &mut self.scratch, false)?;
        let samples = frames * self.channels;
        if let Ok(mut ring) = self.ring.lock() {
            ring.push(&self.scratch[..samples]);
        }
        self.started
            .store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
}

/// A fixed-capacity queue of interleaved samples.
struct Ring {
    samples: std::collections::VecDeque<f32>,
    capacity: usize,
    /// Samples dropped because the ring was full, since the last report.
    dropped: usize,
}

impl Ring {
    fn new(capacity: usize) -> Self {
        Self {
            samples: std::collections::VecDeque::with_capacity(capacity),
            capacity,
            dropped: 0,
        }
    }

    /// Add samples, discarding the oldest if there is no room.
    fn push(&mut self, samples: &[f32]) {
        let overflow = (self.samples.len() + samples.len()).saturating_sub(self.capacity);
        if overflow > 0 {
            self.samples.drain(..overflow.min(self.samples.len()));
            self.dropped += overflow;
            if self.dropped >= self.capacity {
                // Constant overflow means the network is delivering faster
                // than the device consumes, which is a clock mismatch worth
                // seeing rather than a hiccup.
                tracing::debug!(
                    dropped = self.dropped,
                    "audio is arriving faster than it plays"
                );
                self.dropped = 0;
            }
        }
        self.samples.extend(samples.iter().copied());
    }

    /// Fill `out` with whatever is ready, and silence after that.
    fn drain_into(&mut self, out: &mut [f32]) {
        for slot in out.iter_mut() {
            *slot = self.samples.pop_front().unwrap_or(0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_gives_back_what_it_was_given() {
        let mut ring = Ring::new(8);
        ring.push(&[1.0, 2.0, 3.0]);

        let mut out = [0.0; 4];
        ring.drain_into(&mut out);
        assert_eq!(out, [1.0, 2.0, 3.0, 0.0], "past the end must be silence");
    }

    #[test]
    fn a_full_ring_drops_the_oldest() {
        let mut ring = Ring::new(4);
        ring.push(&[1.0, 2.0, 3.0, 4.0]);
        ring.push(&[5.0, 6.0]);

        let mut out = [0.0; 4];
        ring.drain_into(&mut out);
        assert_eq!(
            out,
            [3.0, 4.0, 5.0, 6.0],
            "the newest audio is the audio worth playing"
        );
    }
}
