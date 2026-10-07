//! Per-process system audio capture (Windows 10 2004+ process loopback).
//!
//! - Sharing a screen: capture everything *except* our own process tree, so
//!   the voice chat we play is never sent back to the people we hear.
//! - Sharing a window: capture only that window's process tree.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, AUDIOCLIENT_ACTIVATION_PARAMS,
    AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient,
    IAudioClient, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX,
};
use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
use windows::Win32::System::Com::{BLOB, COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{Interface, implement};

use crate::protocol::SAMPLE_RATE;

#[derive(Debug, Clone, Copy)]
pub enum Target {
    /// Everything except this process and its children.
    AllExcept(u32),
    /// Only this process and its children.
    Process(u32),
}

pub struct Loopback {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Completion {
    done: std::sync::mpsc::SyncSender<()>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for Completion_Impl {
    fn ActivateCompleted(
        &self,
        _op: windows::core::Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let _ = self.done.send(());
        Ok(())
    }
}

impl Loopback {
    /// Starts capturing; `sink` receives interleaved stereo f32 at 48 kHz.
    pub fn start(target: Target, mut sink: impl FnMut(&[f32]) + Send + 'static) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);
        let s = stop.clone();
        let thread = thread::Builder::new()
            .name("audio-loopback".into())
            .spawn(move || {
                let _ = thread_priority::set_current_thread_priority(thread_priority::ThreadPriority::Max);
                match unsafe { open(target) } {
                    Ok((client, capture, event)) => {
                        let _ = ready_tx.send(Ok(()));
                        unsafe { pump(&client, &capture, event, &s, &mut sink) };
                        unsafe {
                            let _ = client.Stop();
                            let _ = CloseHandle(event);
                        }
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(Self { stop, thread: Some(thread) }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("audio capture did not start".into()),
        }
    }
}

unsafe fn open(
    target: Target,
) -> Result<(IAudioClient, IAudioCaptureClient, windows::Win32::Foundation::HANDLE), String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let (pid, mode) = match target {
            Target::AllExcept(pid) => (pid, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE),
            Target::Process(pid) => (pid, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE),
        };
        let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: mode,
                },
            },
        };
        // ManuallyDrop: PROPVARIANT's Drop calls PropVariantClear, which would
        // CoTaskMemFree our stack-allocated params.
        let prop = std::mem::ManuallyDrop::new(PROPVARIANT {
            Anonymous: PROPVARIANT_0 {
                Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                    vt: VT_BLOB,
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous: PROPVARIANT_0_0_0 {
                        blob: BLOB {
                            cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                            pBlobData: &mut params as *mut _ as *mut u8,
                        },
                    },
                }),
            },
        });

        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        let handler: IActivateAudioInterfaceCompletionHandler = Completion { done: done_tx }.into();
        let op = ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&*prop),
            &handler,
        )
        .map_err(|e| format!("activate: {e}"))?;
        done_rx.recv_timeout(Duration::from_secs(3)).map_err(|_| "activation timed out")?;
        let mut hr = windows::core::HRESULT(0);
        let mut iface = None;
        op.GetActivateResult(&mut hr, &mut iface).map_err(|e| e.to_string())?;
        hr.ok().map_err(|e| format!("process loopback unavailable: {e}"))?;
        let client: IAudioClient = iface.ok_or("no audio client")?.cast().map_err(|e| e.to_string())?;

        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
            nChannels: 2,
            nSamplesPerSec: SAMPLE_RATE,
            nAvgBytesPerSec: SAMPLE_RATE * 8,
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                // SRC_DEFAULT_QUALITY: without it, converting a 44.1 kHz device to
                // our 48 kHz uses Windows' cheapest resampler, which sounds gritty.
                AUDCLNT_STREAMFLAGS_LOOPBACK
                    | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                200_000, // 20 ms buffer
                0,
                &format,
                None,
            )
            .map_err(|e| format!("initialize: {e}"))?;
        let event = CreateEventW(None, false, false, None).map_err(|e| e.to_string())?;
        client.SetEventHandle(event).map_err(|e| e.to_string())?;
        let capture: IAudioCaptureClient = client.GetService().map_err(|e| e.to_string())?;
        client.Start().map_err(|e| format!("start: {e}"))?;
        Ok((client, capture, event))
    }
}

unsafe fn pump(
    _client: &IAudioClient,
    capture: &IAudioCaptureClient,
    event: windows::Win32::Foundation::HANDLE,
    stop: &AtomicBool,
    sink: &mut impl FnMut(&[f32]),
) {
    let mut silence = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        if unsafe { WaitForSingleObject(event, 100) } != WAIT_OBJECT_0 {
            continue;
        }
        loop {
            let Ok(n) = (unsafe { capture.GetNextPacketSize() }) else { return };
            if n == 0 {
                break;
            }
            let mut data = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            if unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }.is_err() {
                return;
            }
            let len = frames as usize * 2;
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                silence.resize(len, 0.0);
                sink(&silence);
            } else {
                sink(unsafe { std::slice::from_raw_parts(data as *const f32, len) });
            }
            unsafe {
                let _ = capture.ReleaseBuffer(frames);
            }
        }
    }
}
