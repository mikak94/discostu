//! Windows Graphics Capture: monitors or single windows, GPU-resident frames.
//!
//! Frames arrive on a WGC worker thread as D3D11 textures on our device the
//! moment the compositor presents them; the callback converts and hands
//! them to the encoder without touching system memory. The cursor is
//! composited by WGC itself.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::{IInspectable, Interface, Result};

use super::d3d::{D3d, SendCell};

#[derive(Debug, Clone, Copy)]
pub enum Target {
    Monitor(isize),
    Window(isize),
}

pub struct Wgc {
    session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
    pub closed: Arc<AtomicBool>,
}

impl Drop for Wgc {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

pub fn item_for(target: Target) -> Result<GraphicsCaptureItem> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    unsafe {
        match target {
            Target::Monitor(h) => interop.CreateForMonitor(HMONITOR(h as _)),
            Target::Window(h) => interop.CreateForWindow(HWND(h as _)),
        }
    }
}

impl Wgc {
    /// `on_frame(texture, content_width, content_height)` runs on a capture
    /// thread for every presented frame.
    pub fn start(
        d3d: &D3d,
        target: Target,
        on_frame: impl Fn(&ID3D11Texture2D, u32, u32) + Send + 'static,
    ) -> Result<(Self, (u32, u32))> {
        let item = item_for(target)?;
        let dxgi: IDXGIDevice = d3d.device.cast()?;
        let rt: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
        let size = item.Size()?;
        let format = DirectXPixelFormat::B8G8R8A8UIntNormalized;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&rt, format, 2, size)?;
        let session = pool.CreateCaptureSession(&item)?;
        let _ = session.SetIsCursorCaptureEnabled(true);
        // Windows 11: no yellow capture border (ignored where unsupported).
        let _ = session.SetIsBorderRequired(false);

        let closed = Arc::new(AtomicBool::new(false));
        let c = closed.clone();
        item.Closed(&TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
            c.store(true, Ordering::Relaxed);
            Ok(())
        }))?;

        let pool_size = std::sync::Mutex::new(size);
        let rt = SendCell(rt);
        pool.FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(
            move |sender, _| {
                let rt = &rt; // capture the whole SendCell, not its field
                let Some(pool) = sender.as_ref() else { return Ok(()) };
                // Drain to the newest frame.
                let mut frame = pool.TryGetNextFrame()?;
                while let Ok(newer) = pool.TryGetNextFrame() {
                    frame = newer;
                }
                let content = frame.ContentSize()?;
                let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
                let tex: ID3D11Texture2D = unsafe { access.GetInterface()? };
                let mut ps = pool_size.lock().expect("pool size lock");
                let (cw, ch) = (
                    content.Width.clamp(1, ps.Width) as u32,
                    content.Height.clamp(1, ps.Height) as u32,
                );
                on_frame(&tex, cw, ch);
                drop(frame);
                // Window resized: grow/shrink the pool to match.
                if content.Width != ps.Width || content.Height != ps.Height {
                    let new = SizeInt32 { Width: content.Width.max(1), Height: content.Height.max(1) };
                    pool.Recreate(&rt.0, format, 2, new)?;
                    *ps = new;
                }
                Ok(())
            },
        ))?;
        session.StartCapture()?;
        Ok((Self { session, pool, closed }, (size.Width as u32, size.Height as u32)))
    }
}
