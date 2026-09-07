//! Small, Linux-only GStreamer lifecycle helpers shared with the Linux client.

use anyhow::Context;
use gstreamer::{self as gst, prelude::*};

/// A native pipeline that is stopped even on failed construction or decode.
pub struct Pipeline(
    /// The underlying native pipeline.
    pub gst::Pipeline,
);

impl Pipeline {
    /// Parse a trusted, application-owned pipeline description.
    pub fn new(description: &str) -> anyhow::Result<Self> {
        gst::init().context("could not initialize native GStreamer")?;
        let pipeline = gst::parse::launch(description)
            .context("Linux media plugin unavailable or incompatible; see docs/linux-media.md")?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow::anyhow!("expected a media pipeline"))?;
        Ok(Self(pipeline))
    }

    /// Resolve an element in the application-owned pipeline.
    pub fn element(&self, name: &str) -> anyhow::Result<gst::Element> {
        self.0
            .by_name(name)
            .with_context(|| format!("missing media element {name}"))
    }

    /// Begin streaming; asynchronous negotiation failures are read from the bus.
    pub fn play(&self) -> anyhow::Result<()> {
        self.0
            .set_state(gst::State::Playing)
            .context("native media pipeline refused to start")?;
        Ok(())
    }

    /// Report plugin errors and unexpected EOS rather than rendering silence.
    pub fn check(&self) -> anyhow::Result<()> {
        let bus = self.0.bus().context("media pipeline has no bus")?;
        while let Some(message) = bus.pop() {
            match message.view() {
                gst::MessageView::Error(error) => anyhow::bail!(
                    "Linux media error from {:?}: {} ({:?})",
                    error.src().map(|src| src.path_string()),
                    error.error(),
                    error.debug()
                ),
                gst::MessageView::Eos(_) => anyhow::bail!("Linux media stream ended unexpectedly"),
                gst::MessageView::Warning(warning) => {
                    tracing::warn!(error = %warning.error(), debug = ?warning.debug(), "Linux media warning");
                }
                _ => {}
            }
        }
        Ok(())
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        if let Err(error) = self.0.set_state(gst::State::Null) {
            tracing::error!(%error, "could not stop native media pipeline");
        }
    }
}
