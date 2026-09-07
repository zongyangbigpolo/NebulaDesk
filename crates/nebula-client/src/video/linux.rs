use anyhow::Context;
use gstreamer::{self as gst, prelude::*};
use gstreamer_app::{AppSink, AppSrc};
use gstreamer_video::{prelude::*, VideoFrameRef, VideoInfo};
use nebula_agent::platform::linux::{h264, pipeline::Pipeline};

use super::{Parameters, Picture, VideoDecoder};

/// Hardware-only decoder. No autoplugger can substitute a software codec.
pub(super) struct VaApi {
    pipeline: Option<DecodePipeline>,
    parameters: Parameters,
}

impl VaApi {
    pub fn new() -> anyhow::Result<Self> {
        gst::init()?;
        anyhow::ensure!(
            gst::ElementFactory::find("vah264dec").is_some(),
            "VA-API H.264 decoder unavailable; install the GStreamer va plugin and a \
             supported VA driver, and grant the desktop user access to /dev/dri/renderD*"
        );
        Ok(Self {
            pipeline: None,
            parameters: Parameters::default(),
        })
    }

    fn decode_frame(&mut self, bytes: &[u8]) -> anyhow::Result<Option<Picture>> {
        anyhow::ensure!(
            bytes.len() <= 16 * 1024 * 1024,
            "H.264 access unit exceeds 16 MiB"
        );
        let units = h264::avcc_units(bytes)?;
        let idr = units.iter().any(|unit| unit[0] & 0x1f == 5);
        let picture = units.iter().any(|unit| matches!(unit[0] & 0x1f, 1..=5));
        let changed = self.parameters.absorb(bytes);
        if changed {
            self.pipeline = None;
        }
        if !picture {
            return Ok(None);
        }
        if self.pipeline.is_none() {
            if !idr || !self.parameters.complete() {
                tracing::debug!("VA-API decoder waiting for IDR with SPS/PPS");
                return Ok(None);
            }
            self.pipeline = Some(DecodePipeline::new()?);
        }
        let pipeline = self
            .pipeline
            .as_mut()
            .context("missing VA-API decode pipeline")?;
        let mut data = Vec::new();
        // A decoder reset also resets h264parse; seed it from the cached headers.
        if pipeline.frames == 0 {
            for parameter in [&self.parameters.sps, &self.parameters.pps] {
                data.extend_from_slice(&[0, 0, 0, 1]);
                data.extend_from_slice(parameter);
            }
        }
        data.extend_from_slice(&h264::annex_b(bytes)?);
        pipeline.decode(data).map(Some)
    }
}

impl VideoDecoder for VaApi {
    fn decode(&mut self, frame: &[u8]) -> anyhow::Result<Option<Picture>> {
        let result = self.decode_frame(frame);
        if result.is_err() {
            // Returning an error asks the shared decode loop for an IDR. Never
            // retain a decoder whose input/output reference chain is uncertain.
            self.pipeline = None;
        }
        result
    }
}

struct DecodePipeline {
    pipeline: Pipeline,
    input: AppSrc,
    output: AppSink,
    frames: u64,
}

impl DecodePipeline {
    fn new() -> anyhow::Result<Self> {
        let pipeline = Pipeline::new(
            "appsrc name=encoded is-live=true format=time block=false max-buffers=2 max-bytes=16777216 \
             caps=video/x-h264,stream-format=byte-stream,alignment=au \
             ! h264parse \
             ! vah264dec \
             ! video/x-raw,format=NV12 \
             ! appsink name=pictures max-buffers=1 drop=false sync=false wait-on-eos=false enable-last-sample=false"
        )?;
        let input = pipeline
            .element("encoded")?
            .downcast::<AppSrc>()
            .map_err(|_| anyhow::anyhow!("decoder appsrc has the wrong type"))?;
        let output = pipeline
            .element("pictures")?
            .downcast::<AppSink>()
            .map_err(|_| anyhow::anyhow!("decoder appsink has the wrong type"))?;
        pipeline.play()?;
        Ok(Self {
            pipeline,
            input,
            output,
            frames: 0,
        })
    }

    fn decode(&mut self, bytes: Vec<u8>) -> anyhow::Result<Picture> {
        self.pipeline.check()?;
        anyhow::ensure!(
            self.input.current_level_buffers() < 2,
            "VA-API input queue stalled"
        );
        let mut buffer = gst::Buffer::from_mut_slice(bytes);
        let buffer_ref = buffer
            .get_mut()
            .context("encoded buffer is unexpectedly shared")?;
        // The interface carries ordered AUs, not PTS. Explicit monotonic DTS=PTS
        // and a live source prevent a non-reordering stream from accumulating.
        let timestamp = gst::ClockTime::from_nseconds(self.frames * 1_000_000_000 / 60);
        buffer_ref.set_pts(timestamp);
        buffer_ref.set_dts(timestamp);
        self.frames += 1;
        self.input
            .push_buffer(buffer)
            .context("VA-API decoder rejected H.264 input")?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            self.pipeline.check()?;
            if let Some(sample) = self
                .output
                .try_pull_sample(gst::ClockTime::from_mseconds(50))
            {
                return picture(&sample);
            }
            anyhow::ensure!(!self.output.is_eos(), "VA-API decoder ended unexpectedly");
            anyhow::ensure!(std::time::Instant::now() < deadline,
                "VA-API produced no picture; GPU decode failed or stream requires unsupported frame reordering");
        }
    }
}

fn picture(sample: &gst::Sample) -> anyhow::Result<Picture> {
    let info = VideoInfo::from_caps(sample.caps().context("decoded picture has no caps")?)?;
    anyhow::ensure!(
        info.format() == gstreamer_video::VideoFormat::Nv12,
        "VA-API output is not NV12"
    );
    let (width, height) = (info.width(), info.height());
    anyhow::ensure!(
        (1..=8192).contains(&width) && (1..=8192).contains(&height),
        "decoded picture exceeds supported dimensions"
    );
    let buffer = sample
        .buffer()
        .context("decoded picture contains no buffer")?;
    let frame = VideoFrameRef::from_buffer_ref_readable(buffer, &info)?;
    let plane = |index: usize, width: usize, height: usize| -> anyhow::Result<(&[u8], usize)> {
        let stride =
            usize::try_from(frame.plane_stride()[index]).context("negative video stride")?;
        let data = frame.plane_data(index as u32)?;
        anyhow::ensure!(
            stride >= width && data.len() >= (height - 1) * stride + width,
            "VA-API returned a truncated video plane"
        );
        Ok((data, stride))
    };
    let (y, y_stride) = plane(0, width as usize, height as usize)?;
    let (chroma_width, chroma_height) = (width.div_ceil(2) as usize, height.div_ceil(2) as usize);
    let (uv, uv_stride) = plane(1, chroma_width * 2, chroma_height)?;
    let (u, v) = super::split_chroma(uv, uv_stride, chroma_width, chroma_height);
    Ok(Picture {
        width,
        height,
        y: super::pack(y, y_stride, width as usize, height as usize),
        u,
        v,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoded_planes_remove_padding() {
        gst::init().unwrap();
        let info = VideoInfo::builder(gstreamer_video::VideoFormat::Nv12, 6, 4)
            .build()
            .unwrap();
        let buffer = gst::Buffer::from_mut_slice(vec![42; info.size()]);
        let sample = gst::Sample::builder()
            .buffer(&buffer)
            .caps(&info.to_caps().unwrap())
            .build();
        let picture = picture(&sample).unwrap();
        assert_eq!(picture.y, vec![42; 24]);
        assert_eq!(picture.u, vec![42; 6]);
        assert_eq!(picture.v, vec![42; 6]);
    }
}
