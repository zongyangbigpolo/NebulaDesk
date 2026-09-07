//! Native D3D11VA H.264 decoder. Only owned planes cross the COM worker boundary.
use super::{
    bitstream,
    mf::{self, D3d, Runtime},
};
use anyhow::{anyhow, bail, ensure, Context};
use std::{sync::mpsc, thread::JoinHandle};
use windows::{
    core::Interface,
    Win32::{
        Graphics::{Direct3D11::*, Dxgi::Common::DXGI_FORMAT_NV12},
        Media::MediaFoundation::*,
        System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER},
    },
};

pub struct Planes {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

struct Request {
    frame: Vec<u8>,
    response: mpsc::SyncSender<anyhow::Result<Option<Planes>>>,
}

pub struct WindowsDecoder {
    requests: Option<mpsc::SyncSender<Request>>,
    worker: Option<JoinHandle<()>>,
}

impl WindowsDecoder {
    pub fn new() -> anyhow::Result<Self> {
        let (requests, incoming) = mpsc::sync_channel::<Request>(1);
        let (ready, started) = mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("nebula-d3d11va".into())
            .spawn(move || {
                let result = (|| {
                    let _runtime = Runtime::new()?;
                    let d3d = D3d::new()?;
                    // Probe hardware-aware MF support before telling the UI decoding is available.
                    let _probe = create_transform(&d3d)?;
                    let _ = ready.send(Ok(()));
                    let mut state = Stream::default();
                    while let Ok(request) = incoming.recv() {
                        let result = state.decode(&d3d, &request.frame);
                        if result.is_err() {
                            state.decoder = None;
                            state.waiting_idr = true;
                        }
                        if request.response.send(result).is_err() {
                            break;
                        }
                    }
                    Ok::<_, anyhow::Error>(())
                })();
                if let Err(error) = result {
                    let _ = ready.send(Err(error));
                }
            })?;
        let mut decoder = Self {
            requests: Some(requests),
            worker: Some(worker),
        };
        if let Err(error) = started
            .recv()
            .context("Windows decoder worker exited during startup")?
        {
            decoder.requests.take();
            return Err(error);
        }
        Ok(decoder)
    }

    pub fn decode(&mut self, frame: &[u8]) -> anyhow::Result<Option<Planes>> {
        ensure!(
            frame.len() <= 64 * 1024 * 1024,
            "H.264 frame exceeds 64 MiB"
        );
        let (response, received) = mpsc::sync_channel(1);
        self.requests
            .as_ref()
            .ok_or_else(|| anyhow!("Windows decoder stopped"))?
            .send(Request {
                frame: frame.to_vec(),
                response,
            })
            .context("Windows decoder worker stopped")?;
        received.recv().context("Windows decoder worker stopped")?
    }
}

impl Drop for WindowsDecoder {
    fn drop(&mut self) {
        self.requests.take();
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                tracing::error!("Windows decoder worker panicked");
            }
        }
    }
}

fn create_transform(d3d: &D3d) -> anyhow::Result<IMFTransform> {
    unsafe {
        let video: ID3D11VideoDevice = d3d.device.cast()?;
        let mut supported = false;
        for index in 0..video.GetVideoDecoderProfileCount() {
            let profile = video.GetVideoDecoderProfile(index)?;
            if profile == D3D11_DECODER_PROFILE_H264_VLD_NOFGT
                && video
                    .CheckVideoDecoderFormat(&profile, DXGI_FORMAT_NV12)?
                    .as_bool()
            {
                supported = true;
                break;
            }
        }
        ensure!(supported, "GPU exposes no hardware H.264 VLD/NV12 decoder; install a DXVA-capable GPU driver (software decoding is disabled)");
        let transform: IMFTransform =
            CoCreateInstance(&CMSH264DecoderMFT, None, CLSCTX_INPROC_SERVER).context(
                "Windows H.264 decoder unavailable; install Media Feature Pack on Windows N",
            )?;
        let attributes = transform.GetAttributes()?;
        ensure!(
            attributes.GetUINT32(&MF_SA_D3D11_AWARE)? != 0,
            "H.264 decoder does not support D3D11VA"
        );
        attributes.SetUINT32(&MF_LOW_LATENCY, 1)?;
        let codec: ICodecAPI = transform.cast()?;
        // Microsoft's H.264 decoder takes VT_UI4 here, unlike encoder MFTs (VT_BOOL).
        mf::codec_u32(&codec, &CODECAPI_AVLowLatencyMode, 1)?;
        // Never clear this manager on MF_E_UNSUPPORTED_D3D_TYPE: that would enable software fallback.
        transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, d3d.manager.as_raw() as usize)?;
        Ok(transform)
    }
}

#[derive(Default)]
struct Stream {
    sps: Vec<u8>,
    pps: Vec<u8>,
    decoder: Option<Decoder>,
    waiting_idr: bool,
    time: i64,
}

impl Stream {
    fn decode(&mut self, d3d: &D3d, frame: &[u8]) -> anyhow::Result<Option<Planes>> {
        let annex_b = bitstream::annex_b(frame)?;
        let mut rest = frame;
        let mut changed = false;
        let mut idr = false;
        while !rest.is_empty() {
            let length = u32::from_be_bytes(rest[..4].try_into()?) as usize;
            let nal = &rest[4..4 + length];
            match nal[0] & 31 {
                7 if self.sps != nal => {
                    self.sps = nal.to_vec();
                    changed = true;
                }
                8 if self.pps != nal => {
                    self.pps = nal.to_vec();
                    changed = true;
                }
                5 => idr = true,
                _ => {}
            }
            rest = &rest[4 + length..];
        }
        if changed {
            self.decoder = None;
            self.waiting_idr = true;
        }
        if self.sps.is_empty() || self.pps.is_empty() || (self.waiting_idr && !idr) {
            return Ok(None);
        }
        if self.decoder.is_none() {
            if !idr {
                return Ok(None);
            }
            let (width, height) = bitstream::dimensions(&self.sps)?;
            self.decoder = Some(Decoder::new(d3d, width, height)?);
        }
        self.waiting_idr = false;
        self.time += 166_667;
        self.decoder
            .as_mut()
            .expect("decoder created")
            .decode(d3d, &annex_b, self.time)
    }
}

struct Decoder {
    transform: IMFTransform,
    input: u32,
    output: u32,
    width: u32,
    height: u32,
    staging: Option<ID3D11Texture2D>,
}

impl Decoder {
    fn new(d3d: &D3d, width: u32, height: u32) -> anyhow::Result<Self> {
        unsafe {
            let transform = create_transform(d3d)?;
            let (input, output) = mf::stream_ids(&transform)?;
            transform.SetInputType(
                input,
                &mf::media_type(&MFVideoFormat_H264, width, height, 60)?,
                0,
            )?;
            let mut decoder = Self {
                transform,
                input,
                output,
                width,
                height,
                staging: None,
            };
            decoder.output_type()?;
            decoder
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            decoder
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(decoder)
        }
    }

    fn output_type(&mut self) -> anyhow::Result<()> {
        unsafe {
            for index in 0..64 {
                let ty = self.transform.GetOutputAvailableType(self.output, index)?;
                if ty.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_NV12 {
                    self.transform.SetOutputType(self.output, &ty, 0)?;
                    let size = ty.GetUINT64(&MF_MT_FRAME_SIZE)?;
                    ensure!(
                        (size >> 32) as u32 >= self.width && size as u32 >= self.height,
                        "MF output dimensions are smaller than the SPS display geometry"
                    );
                    return Ok(());
                }
            }
        }
        bail!("D3D11VA decoder exposes no NV12 output; software decoding is disabled")
    }

    fn decode(&mut self, d3d: &D3d, data: &[u8], time: i64) -> anyhow::Result<Option<Planes>> {
        unsafe {
            let sample = mf::sample(data, time)?;
            sample.SetSampleDuration(166_667)?;
            self.transform.ProcessInput(self.input, &sample, 0)?;
            let mut picture = None;
            for _ in 0..16 {
                match mf::output(&self.transform, self.output) {
                    Ok(sample) => picture = Some(self.copy(d3d, &sample)?),
                    Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                        return Ok(picture)
                    }
                    Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        self.output_type()?;
                        self.staging = None;
                    }
                    Err(error) => return Err(error).context("D3D11VA rejected the H.264 frame"),
                }
            }
            bail!("MF decoder produced too many outputs for a single frame")
        }
    }

    fn copy(&mut self, d3d: &D3d, sample: &IMFSample) -> anyhow::Result<Planes> {
        unsafe {
            let buffer: IMFDXGIBuffer = sample.GetBufferByIndex(0)?.cast()
                .context("MF returned a CPU/software-decoded buffer; hardware-only decoding requires a D3D11VA surface")?;
            let mut ptr = std::ptr::null_mut();
            buffer.GetResource(&ID3D11Texture2D::IID, &mut ptr)?;
            ensure!(!ptr.is_null(), "MF returned a null decoded texture");
            let texture = ID3D11Texture2D::from_raw(ptr);
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut desc);
            ensure!(
                desc.Format == DXGI_FORMAT_NV12
                    && desc.Width >= self.width
                    && desc.Height >= self.height
                    && desc.Width <= 8192
                    && desc.Height <= 8192,
                "invalid hardware decoder texture geometry/format"
            );
            if self.staging.is_none() {
                self.staging = Some(d3d.texture(desc.Width, desc.Height, true)?);
            }
            let staging = self.staging.as_ref().expect("staging texture created");
            d3d.context.CopySubresourceRegion(
                staging,
                0,
                0,
                0,
                0,
                &texture,
                buffer.GetSubresourceIndex()?,
                None,
            );
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            d3d.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let result = (|| {
                let stride = mapped.RowPitch as usize;
                ensure!(
                    stride >= desc.Width as usize && !mapped.pData.is_null(),
                    "invalid mapped NV12 surface"
                );
                let bytes = std::slice::from_raw_parts(
                    mapped.pData.cast::<u8>(),
                    stride * (desc.Height as usize + desc.Height as usize / 2),
                );
                let (width, height) = (self.width as usize, self.height as usize);
                let mut y = Vec::with_capacity(width * height);
                let mut u = Vec::with_capacity(width * height / 4);
                let mut v = Vec::with_capacity(width * height / 4);
                for row in 0..height {
                    y.extend_from_slice(&bytes[row * stride..row * stride + width]);
                }
                let uv = &bytes[stride * desc.Height as usize..];
                for row in 0..height / 2 {
                    for column in 0..width / 2 {
                        u.push(uv[row * stride + column * 2]);
                        v.push(uv[row * stride + column * 2 + 1]);
                    }
                }
                Ok(Planes {
                    width: self.width,
                    height: self.height,
                    y,
                    u,
                    v,
                })
            })();
            d3d.context.Unmap(staging, 0);
            result
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            for result in [
                self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0),
                self.transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0),
            ] {
                if let Err(error) = result {
                    tracing::warn!(%error, "MF decoder teardown failed");
                }
            }
        }
    }
}
