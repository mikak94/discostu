//! Hardware H.264 encoding through Media Foundation (NVENC / AMF / Quick Sync
//! MFTs), fed straight from GPU textures.
//!
//! Tuned for interactivity over bandwidth: low-latency mode, no B-frames
//! (no reordering delay), low-delay VBR, keyframes on demand.

use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_channel::{Receiver, select};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4};
use windows::core::{GUID, Interface, Result};

use super::d3d::{D3d, SendCell};
use crate::clock;

pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub key: bool,
    pub width: u32,
    pub height: u32,
    pub captured_us: u64,
    /// Capture → bitstream ready, on the sharer.
    pub encode_us: u32,
}

pub struct Encoder {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    codec: Option<ICodecAPI>,
    _manager: IMFDXGIDeviceManager,
    pub name: String,
    pub width: u32,
    pub height: u32,
    fps: u32,
}

pub fn var_u32(v: u32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_UI4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { ulVal: v },
            }),
        },
    }
}

pub fn var_bool(v: bool) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BOOL,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 {
                    boolVal: windows::Win32::Foundation::VARIANT_BOOL(if v { -1 } else { 0 }),
                },
            }),
        },
    }
}

pub fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

pub fn startup() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
    }
}

/// Enumerates MFTs for a category / input / output subtype.
pub fn enum_mfts(category: GUID, flags: MFT_ENUM_FLAG, input: GUID, output: GUID) -> Vec<IMFActivate> {
    let inp = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: input };
    let out = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: output };
    let mut ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    let mut list = Vec::new();
    unsafe {
        if MFTEnumEx(category, flags, Some(&inp), Some(&out), &mut ptr, &mut count).is_ok() && !ptr.is_null() {
            for i in 0..count as usize {
                if let Some(a) = (*ptr.add(i)).take() {
                    list.push(a);
                }
            }
            CoTaskMemFree(Some(ptr as _));
        }
    }
    list
}

fn friendly_name(a: &IMFActivate) -> String {
    unsafe {
        let mut p = windows::core::PWSTR::null();
        let mut len = 0u32;
        if a.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut len).is_ok() {
            let s = p.to_string().unwrap_or_default();
            CoTaskMemFree(Some(p.0 as _));
            return s;
        }
    }
    "H.264 encoder".into()
}

impl Encoder {
    pub fn new(d3d: &D3d, width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self> {
        startup();
        let candidates = enum_mfts(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            MFVideoFormat_NV12,
            MFVideoFormat_H264,
        );
        let mut last = windows::core::Error::from_hresult(MF_E_TOPO_CODEC_NOT_FOUND);
        for activate in candidates {
            match Self::configure(d3d, &activate, width, height, fps, bitrate) {
                Ok(enc) => return Ok(enc),
                Err(e) => {
                    eprintln!("encoder {} rejected: {e}", friendly_name(&activate));
                    last = e;
                    unsafe {
                        let _ = activate.ShutdownObject();
                    }
                }
            }
        }
        Err(last)
    }

    fn configure(
        d3d: &D3d,
        activate: &IMFActivate,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
    ) -> Result<Self> {
        unsafe {
            let name = friendly_name(activate);
            let transform: IMFTransform = activate.ActivateObject()?;
            let attrs = transform.GetAttributes()?;
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);

            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.expect("device manager");
            manager.ResetDevice(&d3d.device, token)?;
            transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;

            let codec = transform.cast::<ICodecAPI>().ok();
            if let Some(c) = &codec {
                let set = |api: &GUID, v: VARIANT| {
                    let _ = c.SetValue(api, &v);
                };
                set(&CODECAPI_AVLowLatencyMode, var_bool(true));
                set(&CODECAPI_AVEncCommonRateControlMode, var_u32(eAVEncCommonRateControlMode_LowDelayVBR.0 as u32));
                set(&CODECAPI_AVEncCommonMeanBitRate, var_u32(bitrate));
                set(&CODECAPI_AVEncCommonMaxBitRate, var_u32(bitrate.saturating_mul(2)));
                set(&CODECAPI_AVEncMPVDefaultBPictureCount, var_u32(0));
                set(&CODECAPI_AVEncMPVGOPSize, var_u32(fps * 10));
                // Favour speed: 0 = fastest preset.
                set(&CODECAPI_AVEncCommonQualityVsSpeed, var_u32(0));
            }

            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            out.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
            out.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
            out.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
            transform.SetOutputType(0, &out, 0)?;

            let inp = MFCreateMediaType()?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            inp.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            inp.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            inp.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            transform.SetInputType(0, &inp, 0)?;

            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            let events: IMFMediaEventGenerator = transform.cast()?;
            Ok(Self { transform, events, codec, _manager: manager, name, width, height, fps })
        }
    }

    fn submit(&self, tex: &ID3D11Texture2D, captured_us: u64, key: bool) -> Result<()> {
        unsafe {
            if key && let Some(c) = &self.codec {
                let _ = c.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &var_u32(1));
            }
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false)?;
            let len = buffer.cast::<IMF2DBuffer>()?.GetContiguousLength()?;
            buffer.SetCurrentLength(len)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(captured_us as i64 * 10)?;
            sample.SetSampleDuration(10_000_000 / self.fps as i64)?;
            self.transform.ProcessInput(0, &sample, 0)
        }
    }

    fn collect(&self) -> Result<Option<EncodedFrame>> {
        unsafe {
            let mut out = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(None),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let result = self.transform.ProcessOutput(0, &mut out, &mut status);
            let sample = ManuallyDrop::take(&mut out[0].pSample);
            drop(ManuallyDrop::take(&mut out[0].pEvents));
            if let Err(e) = result {
                if e.code() == MF_E_TRANSFORM_STREAM_CHANGE {
                    let t = self.transform.GetOutputAvailableType(0, 0)?;
                    self.transform.SetOutputType(0, &t, 0)?;
                    return Ok(None);
                }
                if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                    return Ok(None);
                }
                return Err(e);
            }
            let Some(sample) = sample else { return Ok(None) };
            let captured_us = (sample.GetSampleTime().unwrap_or(0) / 10).max(0) as u64;
            let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            let mut data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            let _ = buffer.Unlock();
            if key && !has_sps(&data) {
                // Some encoders keep SPS/PPS only in the media type.
                if let Ok(t) = self.transform.GetOutputCurrentType(0)
                    && let Ok(size) = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER)
                {
                    let mut header = vec![0u8; size as usize];
                    if t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut header, None).is_ok() {
                        header.extend_from_slice(&data);
                        data = header;
                    }
                }
            }
            let now = clock::now_us();
            Ok(Some(EncodedFrame {
                data,
                key,
                width: self.width,
                height: self.height,
                captured_us,
                encode_us: now.saturating_sub(captured_us) as u32,
            }))
        }
    }

    /// Drives the encoder until `stop`. Frames arrive on `frames` as
    /// (NV12 texture, capture time); only the newest pending one is encoded.
    pub fn run(
        self,
        frames: Receiver<(SendCell<ID3D11Texture2D>, u64)>,
        key_request: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        mut output: impl FnMut(EncodedFrame),
    ) {
        startup();
        // Async MFTs signal readiness through events. Wait for them on a
        // helper thread so a pending output is never stuck behind a wait
        // for the next input frame.
        let (ev_tx, ev_rx) = crossbeam_channel::unbounded::<u32>();
        let events = SendCell(self.events.clone());
        let ev_stop = stop.clone();
        let ev_thread = std::thread::Builder::new()
            .name("encoder-events".into())
            .spawn(move || {
                startup();
                let events = events;
                while !ev_stop.load(Ordering::Relaxed) {
                    match unsafe { events.0.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)) } {
                        Ok(ev) => {
                            let t = unsafe { ev.GetType() }.unwrap_or(0);
                            if ev_tx.send(t).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
            .ok();

        let mut can_take = 0u32;
        let mut pending: Option<(SendCell<ID3D11Texture2D>, u64)> = None;
        let mut first = true;
        while !stop.load(Ordering::Relaxed) {
            select! {
                recv(frames) -> f => match f {
                    Ok(f) => pending = Some(f),
                    Err(_) => break,
                },
                recv(ev_rx) -> ev => match ev {
                    Ok(t) if t == METransformNeedInput.0 as u32 => can_take += 1,
                    Ok(t) if t == METransformHaveOutput.0 as u32 => match self.collect() {
                        Ok(Some(frame)) => output(frame),
                        Ok(None) => {}
                        Err(e) => {
                            eprintln!("encoder output: {e}");
                            break;
                        }
                    },
                    Ok(_) => {}
                    Err(_) => break,
                },
                default(std::time::Duration::from_millis(100)) => {}
            }
            if can_take > 0
                && let Some((tex, ts)) = pending.take()
            {
                let key = first || key_request.swap(false, Ordering::Relaxed);
                first = false;
                match self.submit(&tex.0, ts, key) {
                    Ok(()) => can_take -= 1,
                    Err(e) if e.code() == MF_E_NOTACCEPTING => pending = Some((tex, ts)),
                    Err(e) => {
                        eprintln!("encoder input: {e}");
                        break;
                    }
                }
            }
        }
        stop.store(true, Ordering::Relaxed);
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            if let Ok(s) = self.transform.cast::<IMFShutdown>() {
                let _ = s.Shutdown();
            }
        }
        if let Some(t) = ev_thread {
            let _ = t.join();
        }
    }
}

/// Does an Annex B stream contain an SPS NAL unit?
pub fn has_sps(data: &[u8]) -> bool {
    data.windows(4).any(|w| w[0] == 0 && w[1] == 0 && w[2] == 1 && (w[3] & 0x1f) == 7)
}
