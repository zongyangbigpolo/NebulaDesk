use std::{
    mem::ManuallyDrop,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc, Arc,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, ensure, Context};
use windows::{
    core::{factory, Interface},
    Foundation::TypedEventHandler,
    Graphics::{
        Capture::*,
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
        SizeInt32,
    },
    Win32::{
        Foundation::{POINT, RECT},
        Graphics::{
            Direct3D11::*,
            Dxgi::IDXGIDevice,
            Gdi::{MonitorFromPoint, MONITOR_DEFAULTTOPRIMARY},
        },
        Media::MediaFoundation::*,
        System::WinRT::{Direct3D11::*, Graphics::Capture::IGraphicsCaptureItemInterop},
    },
};

use super::{
    bitstream::ParameterSets,
    mf::{self, D3d, Runtime},
};
use crate::media::{EncodedFrame, FrameSink, VideoConfig, VideoSource};

#[derive(Default)]
pub struct WindowsVideo {
    running: Arc<AtomicBool>,
    keyframe: Arc<AtomicBool>,
    bitrate: Arc<AtomicU32>,
    worker: Option<JoinHandle<()>>,
}

impl VideoSource for WindowsVideo {
    fn start(&mut self, config: VideoConfig, sink: FrameSink) -> anyhow::Result<()> {
        ensure!(self.worker.is_none(), "Windows capture is already started");
        ensure!(
            config.width >= 2
                && config.height >= 2
                && config.width <= 8192
                && config.height <= 8192,
            "capture dimensions must be between 2 and 8192"
        );
        ensure!(
            (1..=240).contains(&config.fps) && config.bitrate > 0,
            "invalid capture frame rate or bitrate"
        );
        self.running.store(true, Ordering::Release);
        self.keyframe.store(true, Ordering::Release);
        self.bitrate.store(config.bitrate, Ordering::Release);
        let (ready, started) = mpsc::sync_channel(1);
        let running = self.running.clone();
        let keyframe = self.keyframe.clone();
        let bitrate = self.bitrate.clone();
        self.worker = Some(
            std::thread::Builder::new()
                .name("nebula-wgc-mf".into())
                .spawn(move || {
                    let result = (|| {
                        let _runtime = Runtime::new()?;
                        let d3d = D3d::new()?;
                        let capture = Capture::new(&d3d)?;
                        let size = capture.item.Size()?;
                        ensure!(
                            size.Width > 0 && size.Height > 0,
                            "primary display is unavailable"
                        );
                        let scale = (config.width as f64 / size.Width as f64)
                            .min(config.height as f64 / size.Height as f64)
                            .min(1.0);
                        let width = ((size.Width as f64 * scale) as u32 & !1).max(2);
                        let height = ((size.Height as f64 * scale) as u32 & !1).max(2);
                        let converter = Converter::new(&d3d, size, width, height, config.fps)?;
                        let mut encoder = Encoder::new(&d3d, width, height, config)?;
                        capture.session.StartCapture().context(CAPTURE_HELP)?;
                        pump(
                            &capture,
                            &converter,
                            &d3d,
                            &mut encoder,
                            config,
                            &sink,
                            &running,
                            &keyframe,
                            &bitrate,
                            &ready,
                        )
                    })();
                    if let Err(error) = result {
                        tracing::error!(%error, "Windows video stopped");
                        let _ = ready.send(Err(error));
                    }
                    running.store(false, Ordering::Release);
                })?,
        );
        match started
            .recv()
            .context("Windows capture worker exited during startup")?
        {
            Ok(()) => Ok(()),
            Err(error) => {
                self.stop();
                Err(error)
            }
        }
    }

    fn request_keyframe(&mut self) {
        self.keyframe.store(true, Ordering::Release);
    }
    fn set_bitrate(&mut self, value: u32) {
        self.bitrate.store(value, Ordering::Release);
    }
    fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                tracing::error!("Windows capture worker panicked");
            }
        }
    }
}

impl Drop for WindowsVideo {
    fn drop(&mut self) {
        self.stop();
    }
}

const CAPTURE_HELP: &str = "Windows Graphics Capture could not open the primary display; run the agent in the signed-in interactive user session, unlock the desktop, and allow screen capture in Windows privacy settings (Session 0/services and the secure desktop are not capturable)";

struct Capture {
    session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
    item: GraphicsCaptureItem,
    closed: Arc<AtomicBool>,
    closed_token: i64,
}

impl Capture {
    fn new(d3d: &D3d) -> anyhow::Result<Self> {
        ensure!(GraphicsCaptureSession::IsSupported()?, "{CAPTURE_HELP}");
        unsafe {
            let interop: IGraphicsCaptureItemInterop =
                factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
            let monitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
            let item: GraphicsCaptureItem =
                interop.CreateForMonitor(monitor).context(CAPTURE_HELP)?;
            let dxgi: IDXGIDevice = d3d.device.cast()?;
            let device: IDirect3DDevice = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()?;
            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                &device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                2,
                item.Size()?,
            )
            .context(CAPTURE_HELP)?;
            let session = pool.CreateCaptureSession(&item)?;
            session.SetIsCursorCaptureEnabled(true)?;
            let closed = Arc::new(AtomicBool::new(false));
            let flag = closed.clone();
            let token = item.Closed(&TypedEventHandler::new(move |_, _| {
                flag.store(true, Ordering::Release);
                Ok(())
            }))?;
            Ok(Self {
                session,
                pool,
                item,
                closed,
                closed_token: token,
            })
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        for result in [
            self.item.RemoveClosed(self.closed_token),
            self.session.Close(),
            self.pool.Close(),
        ] {
            if let Err(error) = result {
                tracing::warn!(%error, "WGC teardown failed");
            }
        }
    }
}

struct CapturedFrame(Direct3D11CaptureFrame);
impl Drop for CapturedFrame {
    fn drop(&mut self) {
        if let Err(error) = self.0.Close() {
            tracing::warn!(%error, "WGC frame release failed");
        }
    }
}

struct Converter {
    device: ID3D11VideoDevice,
    context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    source: SizeInt32,
    width: u32,
    height: u32,
}

impl Converter {
    fn new(
        d3d: &D3d,
        source: SizeInt32,
        width: u32,
        height: u32,
        fps: u32,
    ) -> anyhow::Result<Self> {
        unsafe {
            let device: ID3D11VideoDevice = d3d.device.cast()?;
            let context: ID3D11VideoContext = d3d.context.cast()?;
            let enumerator = device
                .CreateVideoProcessorEnumerator(&D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                    InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                    InputFrameRate: windows::Win32::Graphics::Dxgi::Common::DXGI_RATIONAL {
                        Numerator: fps,
                        Denominator: 1,
                    },
                    InputWidth: source.Width as u32,
                    InputHeight: source.Height as u32,
                    OutputFrameRate: windows::Win32::Graphics::Dxgi::Common::DXGI_RATIONAL {
                        Numerator: fps,
                        Denominator: 1,
                    },
                    OutputWidth: width,
                    OutputHeight: height,
                    Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
                })
                .context("GPU cannot scale/convert WGC BGRA to NV12")?;
            let processor = device.CreateVideoProcessor(&enumerator, 0)?;
            context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            context.VideoProcessorSetStreamSourceRect(
                &processor,
                0,
                true,
                Some(&RECT {
                    left: 0,
                    top: 0,
                    right: source.Width,
                    bottom: source.Height,
                }),
            );
            let target = RECT {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            };
            context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&target));
            context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&target));
            // RGB full-range input; BT.709 limited-range NV12 output.
            context.VideoProcessorSetStreamColorSpace(
                &processor,
                0,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 },
            );
            context.VideoProcessorSetOutputColorSpace(
                &processor,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                    _bitfield: (1 << 2) | (1 << 4),
                },
            );
            Ok(Self {
                device,
                context,
                enumerator,
                processor,
                source,
                width,
                height,
            })
        }
    }

    fn convert(
        &self,
        d3d: &D3d,
        frame: &Direct3D11CaptureFrame,
    ) -> anyhow::Result<ID3D11Texture2D> {
        ensure!(
            frame.ContentSize()? == self.source,
            "display resolution changed; reconnect to reconfigure Windows capture"
        );
        unsafe {
            let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
            let texture: ID3D11Texture2D = access.GetInterface()?;
            let output = d3d.texture(self.width, self.height, false)?;
            let mut input_view = None;
            self.device.CreateVideoProcessorInputView(
                &texture,
                &self.enumerator,
                &D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                    FourCC: 0,
                    ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPIV {
                            MipSlice: 0,
                            ArraySlice: 0,
                        },
                    },
                },
                Some(&mut input_view),
            )?;
            let mut output_view = None;
            self.device.CreateVideoProcessorOutputView(
                &output,
                &self.enumerator,
                &D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                    },
                },
                Some(&mut output_view),
            )?;
            let output_view = output_view.ok_or_else(|| anyhow!("missing D3D11 output view"))?;
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(input_view),
                ..Default::default()
            };
            let result = self.context.VideoProcessorBlt(
                &self.processor,
                &output_view,
                0,
                std::slice::from_ref(&stream),
            );
            ManuallyDrop::drop(&mut stream.pInputSurface);
            result?;
            Ok(output)
        }
    }
}

struct Encoder {
    transform: IMFTransform,
    activation: IMFActivate,
    events: IMFMediaEventGenerator,
    codec: ICodecAPI,
    input: u32,
    output: u32,
    input_credit: u32,
    outstanding: u32,
    parameters: ParameterSets,
    last_progress: Instant,
}

impl Encoder {
    fn new(d3d: &D3d, width: u32, height: u32, config: VideoConfig) -> anyhow::Result<Self> {
        let mut failures = Vec::new();
        for activation in mf::hardware_encoders()? {
            let result = Self::activate(activation.clone(), d3d, width, height, config);
            match result {
                Ok(encoder) => return Ok(encoder),
                Err(error) => {
                    failures.push(error.to_string());
                    unsafe {
                        if let Err(error) = activation.ShutdownObject() {
                            tracing::debug!(%error, "rejected hardware encoder shutdown");
                        }
                    }
                }
            }
        }
        bail!("no hardware H.264 encoder accepted D3D11 NV12, low latency and zero B frames: {}; update the GPU driver or choose a supported resolution", failures.join("; "))
    }

    fn activate(
        activation: IMFActivate,
        d3d: &D3d,
        width: u32,
        height: u32,
        config: VideoConfig,
    ) -> anyhow::Result<Self> {
        unsafe {
            let transform: IMFTransform = activation.ActivateObject()?;
            let attributes = transform.GetAttributes()?;
            ensure!(
                attributes.GetUINT32(&MF_TRANSFORM_ASYNC)? != 0,
                "hardware MFT is not asynchronous"
            );
            attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            ensure!(
                attributes.GetUINT32(&MF_SA_D3D11_AWARE)? != 0,
                "hardware MFT is not D3D11 aware"
            );
            attributes.SetUINT32(&MF_LOW_LATENCY, 1)?;
            transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, d3d.manager.as_raw() as usize)?;
            let codec: ICodecAPI = transform.cast()?;
            mf::codec_bool(&codec, &CODECAPI_AVLowLatencyMode, true)?;
            mf::codec_u32(&codec, &CODECAPI_AVEncMPVDefaultBPictureCount, 0)?;
            mf::codec_u32(
                &codec,
                &CODECAPI_AVEncCommonRateControlMode,
                eAVEncCommonRateControlMode_CBR.0 as u32,
            )?;
            mf::codec_u32(&codec, &CODECAPI_AVEncCommonMeanBitRate, config.bitrate)?;
            mf::codec_u32(&codec, &CODECAPI_AVEncMPVGOPSize, config.fps * 2)?;
            let (input, output) = mf::stream_ids(&transform)?;
            let encoded = mf::media_type(&MFVideoFormat_H264, width, height, config.fps)?;
            encoded.SetUINT32(&MF_MT_AVG_BITRATE, config.bitrate)?;
            encoded.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
            transform.SetOutputType(output, &encoded, 0)?;
            let raw = mf::media_type(&MFVideoFormat_NV12, width, height, config.fps)?;
            transform.SetInputType(input, &raw, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            let events = transform.cast()?;
            Ok(Self {
                transform,
                activation,
                codec,
                events,
                input,
                output,
                input_credit: 0,
                outstanding: 0,
                parameters: ParameterSets::default(),
                last_progress: Instant::now(),
            })
        }
    }

    fn poll(&mut self) -> anyhow::Result<Vec<EncodedFrame>> {
        let mut frames = Vec::new();
        unsafe {
            loop {
                let event = match self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                    Ok(event) => event,
                    Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => break,
                    Err(error) => return Err(error.into()),
                };
                event.GetStatus()?.ok()?;
                match event.GetType()? {
                    value if value == METransformNeedInput.0 as u32 => self.input_credit += 1,
                    value if value == METransformHaveOutput.0 as u32 => {
                        let Some(sample) = mf::output(&self.transform, self.output)? else {
                            continue;
                        };
                        let data = mf::bytes(&sample)?;
                        // Some drivers provide SPS/PPS only in the media type.
                        let ty = self.transform.GetOutputCurrentType(self.output)?;
                        match ty.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) {
                            Ok(size) if size > 0 => {
                                let mut header = vec![0u8; size as usize];
                                ty.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut header, None)?;
                                self.parameters.absorb(&header)?;
                            }
                            Ok(_) => {}
                            Err(error) if error.code() == MF_E_ATTRIBUTENOTFOUND => {}
                            Err(error) => return Err(error.into()),
                        }
                        let (keyframe, data) = self.parameters.frame(&data)?;
                        frames.push(EncodedFrame {
                            keyframe,
                            timestamp_us: (sample.GetSampleTime()?.max(0) / 10) as u64,
                            data,
                        });
                        self.outstanding = self.outstanding.saturating_sub(1);
                        self.last_progress = Instant::now();
                    }
                    _ => {}
                }
            }
        }
        ensure!(
            self.outstanding == 0 || self.last_progress.elapsed() < Duration::from_secs(5),
            "hardware encoder stalled for five seconds; reconnect or update/reset the GPU driver"
        );
        Ok(frames)
    }

    fn submit(
        &mut self,
        texture: &ID3D11Texture2D,
        time: i64,
        fps: u32,
        keyframe: bool,
    ) -> anyhow::Result<()> {
        unsafe {
            if keyframe {
                mf::codec_u32(&self.codec, &CODECAPI_AVEncVideoForceKeyFrame, 1)?;
            }
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)?;
            let buffer2d: IMF2DBuffer = buffer.cast()?;
            buffer.SetCurrentLength(buffer2d.GetContiguousLength()?)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time)?;
            sample.SetSampleDuration(10_000_000 / fps as i64)?;
            self.transform.ProcessInput(self.input, &sample, 0)?;
            self.input_credit -= 1;
            if self.outstanding == 0 {
                self.last_progress = Instant::now();
            }
            self.outstanding += 1;
        }
        Ok(())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            for result in [
                self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0),
                self.transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0),
                self.activation.ShutdownObject(),
            ] {
                if let Err(error) = result {
                    tracing::warn!(%error, "hardware encoder teardown failed");
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn pump(
    capture: &Capture,
    converter: &Converter,
    d3d: &D3d,
    encoder: &mut Encoder,
    config: VideoConfig,
    sink: &FrameSink,
    running: &AtomicBool,
    keyframe: &AtomicBool,
    bitrate: &AtomicU32,
    ready: &mpsc::SyncSender<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    let started = Instant::now();
    let mut last_frame = started - Duration::from_secs(1);
    let interval = Duration::from_secs_f64(1.0 / config.fps as f64);
    let mut current_bitrate = config.bitrate;
    let mut delivery = Delivery::default();
    let mut announced = false;
    let mut recovery_since = Instant::now();
    // Retain the last real capture so a keyframe request can recover an entirely static desktop.
    let mut latest = None;
    while running.load(Ordering::Acquire) && !sink.is_closed() {
        ensure!(
            !capture.closed.load(Ordering::Acquire),
            "captured display closed; reconnect after restoring the display"
        );
        for frame in encoder.poll()? {
            if !delivery.accepts(frame.keyframe) {
                continue;
            }
            match sink.try_send(frame) {
                Ok(()) => {
                    delivery.delivered();
                    if !announced {
                        ready
                            .send(Ok(()))
                            .context("capture startup caller disappeared")?;
                        announced = true;
                    }
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    if delivery.accepts(false) {
                        recovery_since = Instant::now();
                    }
                    delivery.dropped();
                    keyframe.store(true, Ordering::Release);
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    ensure!(announced, "capture consumer closed during startup");
                    return Ok(());
                }
            }
        }
        let requested = bitrate.load(Ordering::Acquire);
        if sink.capacity() == 0 {
            recovery_since = Instant::now();
        }
        ensure!(delivery.accepts(false) || recovery_since.elapsed() < Duration::from_secs(5),
            "hardware encoder did not produce a recovery IDR within five seconds; update the GPU driver");
        if requested != current_bitrate {
            ensure!(requested > 0, "requested Windows encoder bitrate is zero");
            mf::codec_u32(&encoder.codec, &CODECAPI_AVEncCommonMeanBitRate, requested)
                .context("hardware encoder rejected bitrate update")?;
            current_bitrate = requested;
        }
        // Drain raw frames before encoding. Dropping raw frames cannot break an H.264 reference chain.
        for _ in 0..2 {
            match capture.pool.TryGetNextFrame() {
                Ok(frame) => {
                    latest = Some(CapturedFrame(frame));
                }
                Err(error) if error.code() == windows::Win32::Foundation::E_POINTER => break,
                Err(error) => return Err(error).context("WGC frame acquisition failed"),
            }
        }
        ensure!(
            latest.is_some() || started.elapsed() < Duration::from_secs(5),
            "WGC produced no initial frame; unlock the desktop and check screen capture permission"
        );
        ensure!(
            encoder.input_credit > 0
                || encoder.outstanding > 0
                || started.elapsed() < Duration::from_secs(5),
            "hardware encoder did not request input; update the GPU driver"
        );
        if let Some(frame) = latest.as_ref() {
            if last_frame.elapsed() >= interval
                && encoder.input_credit > 0
                && encoder.outstanding < 3
                && sink.capacity() > 0
            {
                let texture = converter.convert(d3d, &frame.0)?;
                encoder.submit(
                    &texture,
                    started.elapsed().as_micros() as i64 * 10,
                    config.fps,
                    keyframe.swap(false, Ordering::AcqRel),
                )?;
                last_frame = Instant::now();
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    ensure!(
        announced,
        "Windows capture ended before producing its first IDR"
    );
    Ok(())
}

struct Delivery {
    waiting_for_idr: bool,
}

impl Default for Delivery {
    fn default() -> Self {
        Self {
            waiting_for_idr: true,
        }
    }
}

impl Delivery {
    fn accepts(&self, idr: bool) -> bool {
        idr || !self.waiting_for_idr
    }
    fn delivered(&mut self) {
        self.waiting_for_idr = false;
    }
    fn dropped(&mut self) {
        self.waiting_for_idr = true;
    }
}

#[cfg(test)]
mod tests {
    use super::Delivery;

    #[test]
    fn a_dropped_encoded_frame_requires_a_delivered_idr() {
        let mut delivery = Delivery::default();
        assert!(!delivery.accepts(false));
        assert!(delivery.accepts(true));
        delivery.delivered();
        assert!(delivery.accepts(false));
        delivery.dropped();
        assert!(!delivery.accepts(false));
        assert!(delivery.accepts(true));
        // A full queue can also drop the recovery IDR.
        delivery.dropped();
        assert!(!delivery.accepts(false));
        delivery.delivered();
        assert!(delivery.accepts(false));
    }
}
