//! BGRA → NV12 on the GPU with the D3D11 video processor (fixed-function
//! hardware on every GPU), scaling/letterboxing into the encoder's size.

use std::mem::ManuallyDrop;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_CPU_ACCESS_FLAG, D3D11_TEX2D_VPIV,
    D3D11_TEX2D_VPOV, D3D11_USAGE_DEFAULT, D3D11_VIDEO_COLOR, D3D11_VIDEO_COLOR_0, D3D11_VIDEO_COLOR_YCbCrA,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Texture2D, ID3D11VideoContext,
    ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT_B8G8R8A8_UNORM,
    DXGI_FORMAT_NV12, DXGI_RATIONAL,
};
use windows::core::{Interface, Result};

use super::d3d::{D3d, texture_size};

/// NV12 textures handed to the encoder. The encoder reads them
/// asynchronously, so we rotate through several.
const POOL: usize = 6;

pub struct Converter {
    d3d: D3d,
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    vp: ID3D11VideoProcessor,
    input: Option<(ID3D11Texture2D, ID3D11VideoProcessorInputView, (u32, u32))>,
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
    pub width: u32,
    pub height: u32,
}

impl Converter {
    pub fn new(d3d: &D3d, in_w: u32, in_h: u32, out_w: u32, out_h: u32) -> Result<Self> {
        let vdev: ID3D11VideoDevice = d3d.device.cast()?;
        let vctx: ID3D11VideoContext = d3d.context.cast()?;
        let rate = DXGI_RATIONAL { Numerator: 60, Denominator: 1 };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: in_w,
            InputHeight: in_h,
            OutputFrameRate: rate,
            OutputWidth: out_w,
            OutputHeight: out_h,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        let enumerator = unsafe { vdev.CreateVideoProcessorEnumerator(&desc)? };
        let vp = unsafe { vdev.CreateVideoProcessor(&enumerator, 0)? };
        unsafe {
            vctx.VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            vctx.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);
            if let Ok(v1) = vctx.cast::<ID3D11VideoContext1>() {
                v1.VideoProcessorSetStreamColorSpace1(&vp, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
                v1.VideoProcessorSetOutputColorSpace1(&vp, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            }
            // Letterbox bars: black in limited-range YCbCr.
            let black = D3D11_VIDEO_COLOR {
                Anonymous: D3D11_VIDEO_COLOR_0 {
                    YCbCr: D3D11_VIDEO_COLOR_YCbCrA { Y: 16.0 / 255.0, Cb: 0.5, Cr: 0.5, A: 1.0 },
                },
            };
            vctx.VideoProcessorSetOutputBackgroundColor(&vp, true, &black);
        }

        let mut outputs = Vec::with_capacity(POOL);
        for _ in 0..POOL {
            let tex = d3d.texture(
                out_w,
                out_h,
                DXGI_FORMAT_NV12,
                D3D11_BIND_RENDER_TARGET,
                D3D11_USAGE_DEFAULT,
                D3D11_CPU_ACCESS_FLAG(0),
            )?;
            let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
            };
            let mut view = None;
            unsafe { vdev.CreateVideoProcessorOutputView(&tex, &enumerator, &view_desc, Some(&mut view))? };
            outputs.push((tex, view.expect("output view")));
        }
        Ok(Self {
            d3d: d3d.clone(),
            vdev,
            vctx,
            enumerator,
            vp,
            input: None,
            outputs,
            next: 0,
            width: out_w,
            height: out_h,
        })
    }

    fn ensure_input(&mut self, w: u32, h: u32) -> Result<()> {
        if self.input.as_ref().is_some_and(|(_, _, s)| *s == (w, h)) {
            return Ok(());
        }
        let tex = self.d3d.texture(
            w,
            h,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE,
            D3D11_USAGE_DEFAULT,
            D3D11_CPU_ACCESS_FLAG(0),
        )?;
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
            },
        };
        let mut view = None;
        unsafe { self.vdev.CreateVideoProcessorInputView(&tex, &self.enumerator, &desc, Some(&mut view))? };
        self.input = Some((tex, view.expect("input view"), (w, h)));
        Ok(())
    }

    /// Converts the `cw`×`ch` top-left content of `src` into the next NV12
    /// pool texture, aspect-fit and centred.
    pub fn convert(&mut self, src: &ID3D11Texture2D, cw: u32, ch: u32) -> Result<ID3D11Texture2D> {
        let (sw, sh) = texture_size(src);
        let (cw, ch) = (cw.min(sw), ch.min(sh));
        self.ensure_input(sw, sh)?;
        let (in_tex, in_view, _) = self.input.as_ref().expect("input ensured");
        let region = D3D11_BOX { left: 0, top: 0, front: 0, right: cw, bottom: ch, back: 1 };
        unsafe { self.d3d.context.CopySubresourceRegion(in_tex, 0, 0, 0, 0, src, 0, Some(&region)) };

        // Aspect-fit the content into the output, on even pixel boundaries.
        let scale = (self.width as f32 / cw as f32).min(self.height as f32 / ch as f32);
        let dw = ((cw as f32 * scale) as i32 & !1).max(2);
        let dh = ((ch as f32 * scale) as i32 & !1).max(2);
        let dx = ((self.width as i32 - dw) / 2) & !1;
        let dy = ((self.height as i32 - dh) / 2) & !1;
        let src_rect = RECT { left: 0, top: 0, right: cw as i32, bottom: ch as i32 };
        let dst_rect = RECT { left: dx, top: dy, right: dx + dw, bottom: dy + dh };

        let (out_tex, out_view) = &self.outputs[self.next];
        self.next = (self.next + 1) % self.outputs.len();
        unsafe {
            self.vctx.VideoProcessorSetStreamSourceRect(&self.vp, 0, true, Some(&src_rect));
            self.vctx.VideoProcessorSetStreamDestRect(&self.vp, 0, true, Some(&dst_rect));
            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(in_view.clone())),
                ..Default::default()
            };
            let streams = [stream];
            let result = self.vctx.VideoProcessorBlt(&self.vp, out_view, 0, &streams);
            let [mut s] = streams;
            ManuallyDrop::drop(&mut s.pInputSurface);
            result?;
            // Kick the GPU now instead of when the encoder next syncs.
            self.d3d.context.Flush();
        }
        Ok(out_tex.clone())
    }
}
