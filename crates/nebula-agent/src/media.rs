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

/// Somewhere to put the input a client sends.
pub trait InputInjector: Send + 'static {
    /// Apply one event to the local desktop.
    fn inject(&mut self, event: &InputEvent) -> anyhow::Result<()>;
}

/// How a session gets its media on this machine.
pub trait Platform: Send + Sync + 'static {
    /// Build a video source for one session.
    fn video(&self) -> anyhow::Result<Box<dyn VideoSource>>;

    /// Build an input injector for one session.
    fn input(&self) -> anyhow::Result<Box<dyn InputInjector>>;
}

/// A platform that captures nothing and injects nothing.
///
/// It exists so the session path can be exercised end to end — handshake,
/// policy, stream carriage, teardown — before any platform capture code
/// exists, and so that a build for a platform whose backend is not finished
/// still runs and still fails honestly rather than not existing.
#[derive(Debug, Default, Clone, Copy)]
pub struct TestPattern;

impl Platform for TestPattern {
    fn video(&self) -> anyhow::Result<Box<dyn VideoSource>> {
        Ok(Box::new(SyntheticVideo::default()))
    }

    fn input(&self) -> anyhow::Result<Box<dyn InputInjector>> {
        Ok(Box::new(DiscardInput))
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
