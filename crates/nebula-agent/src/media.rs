//! The boundary between session plumbing and platform media.
//!
//! Everything above this line — tunnels, tickets, relays, policy — is done and
//! is identical on every platform. Everything below it is per-platform and
//! arrives in the media phase: ScreenCaptureKit and VideoToolbox on macOS,
//! Windows.Graphics.Capture and Media Foundation on Windows, PipeWire and
//! VA-API on Linux.
//!
//! The interface is a push sink rather than a pull iterator because that is
//! what every capture API on every platform actually is: a callback fired by
//! the compositor when a frame is ready. Modelling it as a stream the agent
//! polls would mean a queue and a thread in between, for nothing.

use crate::clipboard::ClipboardAccess;
use ndp_proto::InputEvent;
use tokio::sync::mpsc;

/// One encoded video frame, ready to put on the wire.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    /// Whether this frame can be decoded without any earlier one.
    pub keyframe: bool,
    /// Capture time in microseconds, on the agent's clock.
    pub timestamp_us: u64,
    /// The encoded bitstream.
    pub data: Vec<u8>,
}

/// Where encoded frames are delivered.
///
/// Bounded: if the network cannot keep up, the right thing is to drop frames
/// at the source, where the encoder can be told to make the next one a
/// keyframe, rather than to accumulate a queue of stale ones that will be
/// decoded into visible lag.
pub type FrameSink = mpsc::Sender<EncodedFrame>;

/// What the client asked for.
#[derive(Debug, Clone, Copy)]
pub struct VideoConfig {
    /// Target width in pixels.
    pub width: u32,
    /// Target height in pixels.
    pub height: u32,
    /// Target frame rate.
    pub fps: u32,
    /// Starting bitrate in bits per second.
    pub bitrate: u32,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate: 12_000_000,
        }
    }
}

/// A source of encoded video for one session.
pub trait VideoSource: Send + 'static {
    /// Begin capturing and encoding into `sink`.
    fn start(&mut self, config: VideoConfig, sink: FrameSink) -> anyhow::Result<()>;

    /// Ask for the next frame to be a keyframe, after a loss or a new viewer.
    fn request_keyframe(&mut self);

    /// Change the encoder's target bitrate, driven by the client's reports.
    fn set_bitrate(&mut self, bits_per_second: u32);

    /// Stop capturing and release the display.
    fn stop(&mut self);
}

/// One encoded packet of audio, ready to put on the wire.
#[derive(Debug, Clone)]
pub struct EncodedAudio {
    /// Capture time in microseconds, on the agent's clock.
    pub timestamp_us: u64,
    /// One Opus packet.
    pub data: Vec<u8>,
}

/// Where encoded audio packets are delivered.
///
/// Bounded like video, and for a sharper reason: audio that arrives late is
/// worse than audio that never arrives. A listener notices a gap far less
/// than they notice speech drifting seconds behind the picture.
pub type AudioSink = mpsc::Sender<EncodedAudio>;

/// What the audio stream is expected to carry.
#[derive(Debug, Clone, Copy)]
pub struct AudioConfig {
    /// Sample rate in hertz.
    pub sample_rate: u32,
    /// Channel count.
    pub channels: u16,
    /// Target bitrate in bits per second.
    pub bitrate: u32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            // Opus is defined at 48 kHz and every platform's system mixer
            // already runs there, so anything else would mean resampling
            // twice to end up where this started.
            sample_rate: 48_000,
            channels: 2,
            bitrate: 128_000,
        }
    }
}

impl AudioConfig {
    /// Samples per channel in one packet.
    ///
    /// 20 ms is Opus's default and the point where its rate/latency curve
    /// stops being worth trading: 10 ms costs noticeably more bitrate for
    /// 10 ms, and 40 ms is audible as lag in an interactive session.
    #[must_use]
    pub fn frame_samples(&self) -> usize {
        self.sample_rate as usize / 50
    }
}

/// A source of encoded audio for one session.
pub trait AudioSource: Send + 'static {
    /// Begin capturing and encoding into `sink`.
    fn start(&mut self, config: AudioConfig, sink: AudioSink) -> anyhow::Result<()>;

    /// Stop capturing and release the device.
    fn stop(&mut self);
}

/// An audio source that captures nothing.
///
/// Sessions to a machine whose platform backend has no audio yet are still
/// worth having, so a missing source is silence rather than a failure.
#[derive(Debug, Default, Clone, Copy)]
pub struct SilentAudio;

impl AudioSource for SilentAudio {
    fn start(&mut self, _config: AudioConfig, _sink: AudioSink) -> anyhow::Result<()> {
        Ok(())
    }

    fn stop(&mut self) {}
}

/// Somewhere to put the input a client sends.
pub trait InputInjector: Send + 'static {
    /// Apply one event to the local desktop.
    fn inject(&mut self, event: &InputEvent) -> anyhow::Result<()>;
}

/// How a session gets its media on this machine.
pub trait Platform: Send + Sync + 'static {
    /// Optionally allocate a per-session factory. Capture and input can share
    /// one desktop-portal consent session without sharing it with other peers.
    /// Stateless backends return `None` and keep using the original factory.
    fn session_scope(&self, _allow_input: bool) -> anyhow::Result<Option<std::sync::Arc<dyn Platform>>> {
        Ok(None)
    }

    /// Build a video source for one session.
    fn video(&self) -> anyhow::Result<Box<dyn VideoSource>>;

    /// Build an input injector for one session.
    fn input(&self) -> anyhow::Result<Box<dyn InputInjector>>;

    /// Build an audio source for one session.
    ///
    /// Defaulted because a platform backend is useful long before it has
    /// audio, and a session with a picture and no sound beats no session.
    fn audio(&self) -> anyhow::Result<Box<dyn AudioSource>> {
        Ok(Box::new(SilentAudio))
    }

    /// Open this machine's clipboard.
    ///
    /// Defaulted to one held in memory: a machine with no desktop session
    /// has no clipboard to open, and a session that shares one existing only
    /// inside itself is better than one that refuses to start.
    fn clipboard(&self) -> anyhow::Result<Box<dyn ClipboardAccess>> {
        Ok(Box::new(crate::clipboard::MemoryClipboard::default()))
    }
}

/// A platform that captures nothing and injects nothing.
///
/// It exists so the session path can be exercised end to end — handshake,
/// policy, stream carriage, teardown — before any platform capture code
/// exists, and so that a build for a platform whose backend is not finished
/// still runs and still fails honestly rather than not existing.
#[derive(Default, Clone)]
pub struct TestPattern {
    /// The clipboard this platform shares, so a test can watch what a
    /// session put on it.
    pub clipboard: crate::clipboard::MemoryClipboard,
}

impl Platform for TestPattern {
    fn video(&self) -> anyhow::Result<Box<dyn VideoSource>> {
        Ok(Box::new(SyntheticVideo::default()))
    }

    fn input(&self) -> anyhow::Result<Box<dyn InputInjector>> {
        Ok(Box::new(DiscardInput))
    }

    fn audio(&self) -> anyhow::Result<Box<dyn AudioSource>> {
        Ok(Box::new(SyntheticAudio::default()))
    }

    fn clipboard(&self) -> anyhow::Result<Box<dyn ClipboardAccess>> {
        Ok(Box::new(self.clipboard.clone()))
    }
}

/// Emits synthetic frames at the requested rate.
#[derive(Debug, Default)]
pub struct SyntheticVideo {
    task: Option<tokio::task::JoinHandle<()>>,
    keyframe: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl VideoSource for SyntheticVideo {
    fn start(&mut self, config: VideoConfig, sink: FrameSink) -> anyhow::Result<()> {
        let keyframe = std::sync::Arc::clone(&self.keyframe);
        let interval = std::time::Duration::from_micros(1_000_000 / config.fps.max(1) as u64);
        // Roughly the size a real encoder would produce at this bitrate, so
        // the transport is exercised with realistic frames rather than toys.
        let bytes = (config.bitrate as usize / 8 / config.fps.max(1) as usize).max(1024);

        self.task = Some(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let started = std::time::Instant::now();
            let mut n: u64 = 0;
            loop {
                ticker.tick().await;
                let want_key = n == 0
                    || keyframe.swap(false, std::sync::atomic::Ordering::Relaxed)
                    || n % (config.fps as u64 * 2) == 0;
                let frame = EncodedFrame {
                    keyframe: want_key,
                    timestamp_us: started.elapsed().as_micros() as u64,
                    data: vec![(n % 251) as u8; if want_key { bytes * 4 } else { bytes }],
                };
                // A full sink means the client is behind; dropping here is
                // the point of the bound.
                if sink.try_send(frame).is_err() && sink.is_closed() {
                    return;
                }
                n += 1;
            }
        }));
        Ok(())
    }

    fn request_keyframe(&mut self) {
        self.keyframe
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn set_bitrate(&mut self, _bits_per_second: u32) {}

    fn stop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Drop for SyntheticVideo {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Emits a tone, encoded exactly as a real source would.
///
/// A silent test source would prove that the audio path carries nothing,
/// which is not the thing worth proving. This produces real Opus packets, so
/// the whole path — encode, datagram, decode, play — is exercised.
#[derive(Default)]
pub struct SyntheticAudio {
    task: Option<tokio::task::JoinHandle<()>>,
}

impl AudioSource for SyntheticAudio {
    fn start(&mut self, config: AudioConfig, sink: AudioSink) -> anyhow::Result<()> {
        let channels = match config.channels {
            1 => opus::Channels::Mono,
            2 => opus::Channels::Stereo,
            n => anyhow::bail!("Opus carries one or two channels, not {n}"),
        };
        let mut encoder =
            opus::Encoder::new(config.sample_rate, channels, opus::Application::Audio)?;
        encoder.set_bitrate(opus::Bitrate::Bits(config.bitrate as i32))?;

        let samples = config.frame_samples();
        let width = config.channels as usize;
        let step = std::f32::consts::TAU * 440.0 / config.sample_rate as f32;

        self.task = Some(tokio::spawn(async move {
            let mut ticker =
                tokio::time::interval(std::time::Duration::from_millis(SYNTHETIC_PACKET_MS));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let started = std::time::Instant::now();
            let mut pcm = vec![0.0f32; samples * width];
            let mut packet = vec![0u8; 4000];
            let mut phase = 0.0f32;

            loop {
                ticker.tick().await;
                for frame in pcm.chunks_mut(width) {
                    // Quiet on purpose: this may end up in somebody's ears.
                    let value = phase.sin() * 0.05;
                    frame.fill(value);
                    phase += step;
                }
                let Ok(n) = encoder.encode_float(&pcm, &mut packet) else {
                    return;
                };
                let frame = EncodedAudio {
                    timestamp_us: started.elapsed().as_micros() as u64,
                    data: packet[..n].to_vec(),
                };
                if sink.try_send(frame).is_err() && sink.is_closed() {
                    return;
                }
            }
        }));
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Drop for SyntheticAudio {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Packet duration of the synthetic source, matching what Opus is configured
/// for elsewhere.
const SYNTHETIC_PACKET_MS: u64 = 20;

/// Accepts input and does nothing with it.
#[derive(Debug, Default, Clone, Copy)]
pub struct DiscardInput;

impl InputInjector for DiscardInput {
    fn inject(&mut self, event: &InputEvent) -> anyhow::Result<()> {
        tracing::trace!(kind = ?event.kind, "input discarded: no platform backend");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_synthetic_source_produces_decodable_audio() {
        let (tx, mut rx) = mpsc::channel(8);
        let config = AudioConfig::default();
        let mut audio = SyntheticAudio::default();
        audio.start(config, tx).unwrap();

        let packet = rx.recv().await.expect("a packet should arrive");
        assert!(!packet.data.is_empty());

        // The point of the source is that a real decoder accepts it.
        let mut decoder = opus::Decoder::new(config.sample_rate, opus::Channels::Stereo).unwrap();
        let mut pcm = vec![0.0f32; config.frame_samples() * 2];
        let frames = decoder
            .decode_float(&packet.data, &mut pcm, false)
            .expect("a real decoder should accept it");
        assert_eq!(
            frames,
            config.frame_samples(),
            "a 20 ms packet should decode to 20 ms of audio"
        );
    }

    #[tokio::test]
    async fn the_synthetic_source_starts_with_a_keyframe() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut video = SyntheticVideo::default();
        video
            .start(
                VideoConfig {
                    fps: 120,
                    ..VideoConfig::default()
                },
                tx,
            )
            .unwrap();

        let first = rx.recv().await.expect("a frame should arrive");
        assert!(
            first.keyframe,
            "a decoder joining mid-stream has nothing to reference"
        );
        assert!(!first.data.is_empty());

        let second = rx.recv().await.expect("a second frame should arrive");
        assert!(second.timestamp_us >= first.timestamp_us);
    }

    #[tokio::test]
    async fn stopping_ends_the_stream() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut video = SyntheticVideo::default();
        video.start(VideoConfig::default(), tx).unwrap();
        rx.recv().await.unwrap();

        video.stop();
        // Drain whatever was already queued, then the channel must end.
        while let Ok(Some(_)) =
            tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await
        {}
    }
}
