use std::{marker::PhantomData, mem::ManuallyDrop, rc::Rc};

use anyhow::{anyhow, ensure, Context};
use windows::{
    core::{Interface, GUID},
    Win32::{
        Foundation::{E_NOTIMPL, HMODULE},
        Graphics::{Direct3D::D3D_DRIVER_TYPE_HARDWARE, Direct3D11::*, Dxgi::Common::*},
        Media::MediaFoundation::*,
        System::{Com::CoTaskMemFree, Variant::VARIANT, WinRT::*},
    },
};

pub struct Runtime(PhantomData<Rc<()>>);

impl Runtime {
    pub fn new() -> anyhow::Result<Self> {
        unsafe {
            RoInitialize(RO_INIT_MULTITHREADED)?;
            if let Err(error) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
                RoUninitialize();
                return Err(error).context(
                    "Media Foundation unavailable; install Media Feature Pack on Windows N",
                );
            }
        }
        Ok(Self(PhantomData))
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        unsafe {
            if let Err(error) = MFShutdown() {
                tracing::warn!(%error, "MFShutdown failed");
            }
            RoUninitialize();
        }
    }
}

pub struct D3d {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub manager: IMFDXGIDeviceManager,
}

impl D3d {
    pub fn new() -> anyhow::Result<Self> {
        unsafe {
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                None, D3D_DRIVER_TYPE_HARDWARE, HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None, D3D11_SDK_VERSION, Some(&mut device), None, Some(&mut context),
            ).context("D3D11 hardware device unavailable; install the GPU vendor driver and use an interactive desktop")?;
            let device = device.ok_or_else(|| anyhow!("D3D11 returned no device"))?;
            let context = context.ok_or_else(|| anyhow!("D3D11 returned no context"))?;
            let multithread: ID3D11Multithread = context.cast()?;
            let _previous = multithread.SetMultithreadProtected(true);
            let mut manager = None;
            let mut token = 0;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.ok_or_else(|| anyhow!("MF returned no DXGI device manager"))?;
            manager.ResetDevice(&device, token)?;
            Ok(Self {
                device,
                context,
                manager,
            })
        }
    }

    pub fn texture(
        &self,
        width: u32,
        height: u32,
        staging: bool,
    ) -> anyhow::Result<ID3D11Texture2D> {
        unsafe {
            let mut texture = None;
            self.device.CreateTexture2D(
                &D3D11_TEXTURE2D_DESC {
                    Width: width,
                    Height: height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: if staging {
                        D3D11_USAGE_STAGING
                    } else {
                        D3D11_USAGE_DEFAULT
                    },
                    BindFlags: if staging {
                        0
                    } else {
                        D3D11_BIND_RENDER_TARGET.0 as u32
                    },
                    CPUAccessFlags: if staging {
                        D3D11_CPU_ACCESS_READ.0 as u32
                    } else {
                        0
                    },
                    MiscFlags: 0,
                },
                None,
                Some(&mut texture),
            )?;
            texture.ok_or_else(|| anyhow!("D3D11 returned no NV12 texture"))
        }
    }
}

pub fn media_type(
    subtype: &GUID,
    width: u32,
    height: u32,
    fps: u32,
) -> anyhow::Result<IMFMediaType> {
    unsafe {
        let ty = MFCreateMediaType()?;
        ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        ty.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        ty.SetUINT64(
            &MF_MT_FRAME_SIZE,
            (u64::from(width) << 32) | u64::from(height),
        )?;
        ty.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(fps) << 32) | 1)?;
        ty.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
        ty.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        ty.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        ty.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        ty.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
        Ok(ty)
    }
}

pub fn codec_u32(codec: &ICodecAPI, key: &GUID, value: u32) -> anyhow::Result<()> {
    unsafe {
        codec.SetValue(key, &VARIANT::from(value))?;
    }
    Ok(())
}

pub fn codec_bool(codec: &ICodecAPI, key: &GUID, value: bool) -> anyhow::Result<()> {
    unsafe {
        codec.SetValue(key, &VARIANT::from(value))?;
    }
    Ok(())
}

/// Releases event/sample interfaces even on ProcessOutput failure.
pub fn output(transform: &IMFTransform, stream: u32) -> windows::core::Result<Option<IMFSample>> {
    unsafe {
        let info = transform.GetOutputStreamInfo(stream)?;
        let sample = if info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                as u32
            != 0
        {
            None
        } else {
            let sample = MFCreateSample()?;
            let buffer =
                MFCreateAlignedMemoryBuffer(info.cbSize, info.cbAlignment.saturating_sub(1))?;
            sample.AddBuffer(&buffer)?;
            Some(sample)
        };
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: stream,
            pSample: ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0;
        let result = transform.ProcessOutput(0, &mut buffers, &mut status);
        let sample = ManuallyDrop::take(&mut buffers[0].pSample);
        let events = ManuallyDrop::take(&mut buffers[0].pEvents);
        result?;
        if let Some(events) = events {
            for index in 0..events.GetElementCount()? {
                let event: IMFMediaEvent = events.GetElement(index)?.cast()?;
                event.GetStatus()?.ok()?;
            }
        }
        if buffers[0].dwStatus & MFT_OUTPUT_DATA_BUFFER_NO_SAMPLE.0 as u32
            == MFT_OUTPUT_DATA_BUFFER_NO_SAMPLE.0 as u32
        {
            Ok(None)
        } else {
            Ok(sample)
        }
    }
}

pub fn stream_ids(transform: &IMFTransform) -> anyhow::Result<(u32, u32)> {
    let mut input = [0];
    let mut output = [0];
    unsafe {
        match transform.GetStreamIDs(&mut input, &mut output) {
            Ok(()) => {}
            Err(error) if error.code() == E_NOTIMPL => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok((input[0], output[0]))
}

pub fn bytes(sample: &IMFSample) -> anyhow::Result<Vec<u8>> {
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer()?;
        let mut ptr = std::ptr::null_mut();
        let mut length = 0;
        buffer.Lock(&mut ptr, None, Some(&mut length))?;
        let data = if length == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(ptr, length as usize).to_vec()
        };
        buffer.Unlock()?;
        Ok(data)
    }
}

pub fn sample(data: &[u8], time: i64) -> anyhow::Result<IMFSample> {
    unsafe {
        let length = u32::try_from(data.len())?;
        let buffer = MFCreateMemoryBuffer(length)?;
        let mut ptr = std::ptr::null_mut();
        buffer.Lock(&mut ptr, None, None)?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(length)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(time)?;
        Ok(sample)
    }
}

pub fn hardware_encoders() -> anyhow::Result<Vec<IMFActivate>> {
    unsafe {
        let mut ptr = std::ptr::null_mut();
        let mut count = 0;
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut ptr,
            &mut count,
        )?;
        let mut activations = Vec::new();
        if !ptr.is_null() {
            for item in std::slice::from_raw_parts_mut(ptr, count as usize) {
                if let Some(item) = item.take() {
                    activations.push(item);
                }
            }
            CoTaskMemFree(Some(ptr.cast()));
        }
        ensure!(!activations.is_empty(), "no hardware H.264 encoder found; install Intel/NVIDIA/AMD GPU drivers with Media Foundation encoding support (software encoding is disabled)");
        Ok(activations)
    }
}
