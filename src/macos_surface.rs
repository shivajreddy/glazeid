/// macOS-specific rendering surface using CoreGraphics.
///
/// softbuffer's CG backend uses `CGImageAlphaInfo::NoneSkipFirst`, which means
/// alpha is discarded and transparency is impossible regardless of layer/window
/// opacity settings.
///
/// This module bypasses softbuffer entirely on macOS: it allocates a pixel
/// buffer with premultiplied BGRA (matching CGImage PremultipliedFirst +
/// ByteOrder32Little), lets tiny_skia render into it, then wraps it in a
/// `CGImage` and pushes it directly onto the window's backing `CALayer`.
/// This gives full per-pixel transparency.
use objc2::msg_send;
use objc2::rc::Retained;
use objc2_app_kit::{NSColor, NSView, NSWindow};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGImage, CGImageAlphaInfo,
    CGImageByteOrderInfo, CGImageComponentInfo, CGImagePixelFormatInfo,
};
use objc2_quartz_core::{CALayer, CATransaction};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

pub struct MacosSurface {
    /// The CALayer we write CGImages into.
    layer: Retained<CALayer>,
    /// Pixel buffer (premultiplied BGRA as u32, matching CGImage format).
    buf: Vec<u32>,
    width: u32,
    height: u32,
}

impl MacosSurface {
    /// Create a surface for `window`, setting up the layer for transparency.
    pub fn new(window: &Window) -> Option<Self> {
        let handle = window.window_handle().ok()?;
        let RawWindowHandle::AppKit(h) = handle.as_raw() else {
            return None;
        };

        unsafe {
            let view: &NSView = h.ns_view.cast().as_ref();
            view.setWantsLayer(true);

            let layer = view.layer()?;

            // Mark non-opaque and remove any solid background fill.
            layer.setOpaque(false);
            layer.setBackgroundColor(None);

            // Mark the NSWindow non-opaque with a clear background.
            let ns_window_ptr: *mut objc2::runtime::AnyObject = msg_send![view, window];
            if !ns_window_ptr.is_null() {
                let ns_window: &NSWindow = &*(ns_window_ptr as *const NSWindow);
                ns_window.setOpaque(false);
                ns_window.setBackgroundColor(Some(&NSColor::clearColor()));
                // Disable the window drop shadow — it appears as a faint
                // border around the transparent window edges.
                ns_window.setHasShadow(false);
            }

            Some(Self {
                layer,
                buf: Vec::new(),
                width: 0,
                height: 0,
            })
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.buf.resize((width * height) as usize, 0);
    }

    /// Access the pixel buffer for rendering.
    pub fn pixels_mut(&mut self) -> &mut [u32] {
        &mut self.buf
    }

    /// Present the current pixel buffer to the screen.
    pub fn present(&self) {
        if self.width == 0 || self.height == 0 {
            return;
        }

        let Some(image) = self.make_cg_image() else {
            return;
        };

        CATransaction::begin();
        CATransaction::setDisableActions(true);
        unsafe { self.layer.setContents(Some(image.as_ref())) };
        CATransaction::commit();
    }

    /// Build a `CGImage` from the pixel buffer with premultiplied alpha.
    fn make_cg_image(&self) -> Option<CFRetained<CGImage>> {
        let w = self.width as usize;
        let h = self.height as usize;
        let len_bytes = w * h * 4;

        // Clone the pixel data into a heap allocation the data-provider callback
        // will free when the CGImage is released.
        let owned: Box<[u32]> = self.buf.clone().into_boxed_slice();
        let data_ptr: *const std::ffi::c_void = owned.as_ptr() as *const std::ffi::c_void;

        // Leak the box — the release callback below will reclaim it.
        let raw = Box::into_raw(owned) as *mut u32;

        unsafe extern "C-unwind" fn release(
            info: *mut std::ffi::c_void,
            _data: std::ptr::NonNull<std::ffi::c_void>,
            size: usize,
        ) {
            // Reconstruct the box from the raw pointer stored in info.
            let ptr = info as *mut u32;
            let len = size / 4;
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)));
        }

        let data_provider = unsafe {
            objc2_core_graphics::CGDataProvider::with_data(
                raw as *mut std::ffi::c_void, // info — passed to release callback
                data_ptr,                     // data pointer
                len_bytes,
                Some(release),
            )?
        };

        let color_space = CGColorSpace::new_device_rgb()?;

        // PremultipliedFirst + ByteOrder32Little:
        // bytes in memory = [B, G, R, A] → u32 on little-endian = 0xAARRGGBB.
        // This matches what renderer.rs writes after the R↔B swap + alpha keep.
        let bitmap_info = CGBitmapInfo(
            CGImageAlphaInfo::PremultipliedFirst.0
                | CGImageComponentInfo::Integer.0
                | CGImageByteOrderInfo::Order32Little.0
                | CGImagePixelFormatInfo::Packed.0,
        );

        unsafe {
            CGImage::new(
                w,
                h,
                8,
                32,
                w * 4,
                Some(&color_space),
                bitmap_info,
                Some(&data_provider),
                std::ptr::null(),
                false,
                CGColorRenderingIntent::RenderingIntentDefault,
            )
        }
    }
}
