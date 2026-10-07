//! H.264 decoding with Microsoft's decoder MFT in low-latency mode (frames
//! come out the moment they go in — the stream has no B-frames).
//!
//! Decoding runs on the GPU (DXVA) when a D3D11 device is available; the
//! NV12 result is read back through a staging texture and uploaded to the
//! UI's renderer, which converts to RGB while sampling. Falls back to the
//! software decoder otherwise.

use std::mem::ManuallyDrop;

use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_FLAG, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, ID3D11Texture2D,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::core::{Interface, Result};

use super::d3d::D3d;
use super::encoder::{startup, var_bool};

pub struct Nv12 {
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    /// Rows of the luma plane in `data` (coded height, may exceed `height`).
    pub rows: usize,
    pub data: Vec<u8>,
}

struct Gpu {
    d3d: D3d,
    _manager: IMFDXGIDeviceManager,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
}

pub struct Decoder {
    transform: IMFTransform,
    gpu: Option<Gpu>,
    width: usize,
    height: usize,
    stride: usize,
    out_size: u32,
    provides_samples: bool,
    time: i64,
}

impl Decoder {
    pub fn new() -> Result<Self> {
        let hardware = std::env::var_os("DISCOSTU_SW_DECODE").is_none();
        match Self::create(hardware) {
            Ok(d) => Ok(d),
            Err(e) => {
                eprintln!("hardware H.264 decode unavailable ({e}); using software");
                Self::create(false)
            }
        }
    }

    fn create(hardware: bool) -> Result<Self> {
        startup();
        unsafe {
            let transform: IMFTransform = CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER)?;
            if let Ok(attrs) = transform.GetAttributes() {
                let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            if let Ok(codec) = transform.cast::<ICodecAPI>() {
                let _ = codec.SetValue(&CODECAPI_AVLowLatencyMode, &var_bool(true));
            }
            let gpu = if hardware {
                let d3d = D3d::new()?;
                let mut token = 0u32;
                let mut manager = None;
                MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
                let manager = manager.expect("device manager");
                manager.ResetDevice(&d3d.device, token)?;
                transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;
                Some(Gpu { d3d, _manager: manager, staging: None })
            } else {
                None
            };
            let inp = MFCreateMediaType()?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            transform.SetInputType(0, &inp, 0)?;
            let mut dec = Self {
                transform,
                gpu,
                width: 0,
                height: 0,
                stride: 0,
                out_size: 0,
                provides_samples: false,
                time: 0,
            };
            dec.choose_output()?;
            dec.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            dec.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(dec)
        }
    }

    fn choose_output(&mut self) -> Result<()> {
        unsafe {
            let mut i = 0;
            loop {
                let t = self.transform.GetOutputAvailableType(0, i)?;
                if t.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_NV12 {
                    self.transform.SetOutputType(0, &t, 0)?;
                    if let Ok(size) = t.GetUINT64(&MF_MT_FRAME_SIZE) {
                        self.width = (size >> 32) as usize;
                        self.height = (size & 0xffff_ffff) as usize;
                    }
                    self.stride = t
                        .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                        .map(|s| (s as i32).unsigned_abs() as usize)
                        .unwrap_or(self.width);
                    break;
                }
                i += 1;
            }
            let info = self.transform.GetOutputStreamInfo(0)?;
            self.out_size = info.cbSize;
            self.provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            Ok(())
        }
    }

    /// Feeds one access unit (Annex B) and emits any decoded frames.
    pub fn decode(&mut self, data: &[u8], mut out: impl FnMut(Nv12)) -> Result<()> {
        unsafe {
            let buffer = MFCreateMemoryBuffer(data.len() as u32)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(data.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(self.time)?;
            self.time += 166_666;

            if let Err(e) = self.transform.ProcessInput(0, &sample, 0) {
                if e.code() != MF_E_NOTACCEPTING {
                    return Err(e);
                }
                self.drain(&mut out)?;
                self.transform.ProcessInput(0, &sample, 0)?;
            }
            self.drain(&mut out)
        }
    }

    fn drain(&mut self, out: &mut impl FnMut(Nv12)) -> Result<()> {
        unsafe {
            loop {
                let provided = if self.provides_samples {
                    None
                } else {
                    let s = MFCreateSample()?;
                    s.AddBuffer(&MFCreateMemoryBuffer(self.out_size.max(1))?)?;
                    Some(s)
                };
                let mut buf = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(provided),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                }];
                let mut status = 0u32;
                let result = self.transform.ProcessOutput(0, &mut buf, &mut status);
                let sample = ManuallyDrop::take(&mut buf[0].pSample);
                drop(ManuallyDrop::take(&mut buf[0].pEvents));
                match result {
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        self.choose_output()?;
                        continue;
                    }
                    Err(e) => return Err(e),
                    Ok(()) => {}
                }
                let Some(sample) = sample else { continue };
                let buffer = sample.GetBufferByIndex(0)?;
                let frame = match buffer.cast::<IMFDXGIBuffer>() {
                    Ok(dxgi) if self.gpu.is_some() => self.read_gpu(&dxgi)?,
                    _ => self.read_cpu(&sample)?,
                };
                if let Some(f) = frame {
                    out(f);
                }
            }
        }
    }

    /// Copies a decoded GPU surface to system memory through a staging texture.
    unsafe fn read_gpu(&mut self, dxgi: &IMFDXGIBuffer) -> Result<Option<Nv12>> {
        unsafe {
            let mut raw = std::ptr::null_mut();
            dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw)?;
            let tex = ID3D11Texture2D::from_raw(raw);
            let index = dxgi.GetSubresourceIndex()?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            tex.GetDesc(&mut desc);
            let gpu = self.gpu.as_mut().expect("gpu decoder");
            if gpu.staging.as_ref().is_none_or(|(_, w, h)| (*w, *h) != (desc.Width, desc.Height)) {
                let staging = gpu.d3d.texture(
                    desc.Width,
                    desc.Height,
                    desc.Format,
                    D3D11_BIND_FLAG(0),
                    D3D11_USAGE_STAGING,
                    D3D11_CPU_ACCESS_READ,
                )?;
                gpu.staging = Some((staging, desc.Width, desc.Height));
            }
            let (staging, _, _) = gpu.staging.as_ref().expect("staging ensured");
            let ctx = &gpu.d3d.context;
            ctx.CopySubresourceRegion(staging, 0, 0, 0, 0, &tex, index, None);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let pitch = mapped.RowPitch as usize;
            let rows = desc.Height as usize;
            let len = pitch * rows * 3 / 2;
            let data = std::slice::from_raw_parts(mapped.pData as *const u8, len).to_vec();
            ctx.Unmap(staging, 0);
            Ok(Some(Nv12 { width: self.width, height: self.height, stride: pitch, rows, data }))
        }
    }

    unsafe fn read_cpu(&self, sample: &IMFSample) -> Result<Option<Nv12>> {
        unsafe {
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            let bytes = std::slice::from_raw_parts(ptr, len as usize);
            let stride = self.stride.max(self.width);
            // Coded height: whatever fills the buffer as Y + Y/2.
            let rows = if stride > 0 { (len as usize * 2 / 3) / stride } else { 0 };
            let frame = (rows >= self.height && self.width > 0).then(|| Nv12 {
                width: self.width,
                height: self.height,
                stride,
                rows,
                data: bytes.to_vec(),
            });
            let _ = buffer.Unlock();
            Ok(frame)
        }
    }
}
