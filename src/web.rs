//! Implementation of software buffering for web targets.

#![allow(clippy::uninlined_format_args)]

use std::convert::TryInto;
use std::marker::PhantomData;
use std::num::NonZeroU32;

use js_sys::Object;
use raw_window_handle::{HasRawWindowHandle, RawWindowHandle, WebWindowHandle};
use wasm_bindgen::{JsCast, JsValue};
use web_sys::ImageData;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement};
use web_sys::{OffscreenCanvas, OffscreenCanvasRenderingContext2d};

use crate::error::SwResultExt;
use crate::Context;
use crate::{Rect, SoftBufferError};

/// Display implementation for the web platform.
///
/// This just caches the document to prevent having to query it every time.
pub struct WebDisplayImpl {
    document: web_sys::Document,
}

impl WebDisplayImpl {
    pub(super) fn new() -> Result<Self, SoftBufferError> {
        let document = web_sys::window()
            .swbuf_err("`Window` is not present in this runtime")?
            .document()
            .swbuf_err("`Document` is not present in this runtime")?;

        Ok(Self { document })
    }
}

pub struct WebImpl {
    /// The handle and context to the canvas that we're drawing to.
    canvas: Canvas,

    /// The buffer that we're drawing to.
    buffer: Vec<u32>,

    /// Buffer has been presented.
    buffer_presented: bool,

    /// The current canvas width/height.
    size: Option<(NonZeroU32, NonZeroU32)>,
}

/// Holding canvas and context for [`HtmlCanvasElement`] or [`OffscreenCanvas`],
/// since they have different types.
enum Canvas {
    Canvas {
        canvas: HtmlCanvasElement,
        ctx: CanvasRenderingContext2d,
    },
    OffscreenCanvas {
        canvas: OffscreenCanvas,
        ctx: OffscreenCanvasRenderingContext2d,
    },
}

impl WebImpl {
    pub fn new(display: &WebDisplayImpl, handle: WebWindowHandle) -> Result<Self, SoftBufferError> {
        let canvas: HtmlCanvasElement = display
            .document
            .query_selector(&format!("canvas[data-raw-handle=\"{}\"]", handle.id))
            // `querySelector` only throws an error if the selector is invalid.
            .unwrap()
            .swbuf_err("No canvas found with the given id")?
            // We already made sure this was a canvas in `querySelector`.
            .unchecked_into();

        Self::from_canvas(canvas)
    }

    fn from_canvas(canvas: HtmlCanvasElement) -> Result<Self, SoftBufferError> {
        let ctx = Self::resolve_ctx(canvas.get_context("2d").ok(), "CanvasRenderingContext2d")?;

        Ok(Self {
            canvas: Canvas::Canvas { canvas, ctx },
            buffer: Vec::new(),
            buffer_presented: false,
            size: None,
        })
    }

    fn from_offscreen_canvas(canvas: OffscreenCanvas) -> Result<Self, SoftBufferError> {
        let ctx = Self::resolve_ctx(
            canvas.get_context("2d").ok(),
            "OffscreenCanvasRenderingContext2d",
        )?;

        Ok(Self {
            canvas: Canvas::OffscreenCanvas { canvas, ctx },
            buffer: Vec::new(),
            buffer_presented: false,
            size: None,
        })
    }

    /// De-duplicates the error handling between `HtmlCanvasElement` and `OffscreenCanvas`.
    fn resolve_ctx<T: JsCast>(
        result: Option<Option<Object>>,
        name: &str,
    ) -> Result<T, SoftBufferError> {
        let ctx = result
            .swbuf_err("Canvas already controlled using `OffscreenCanvas`")?
            .swbuf_err(format!(
                "A canvas context other than `{name}` was already created"
            ))?
            .dyn_into()
            .unwrap_or_else(|_| panic!("`getContext(\"2d\") didn't return a `{name}`"));

        Ok(ctx)
    }

    /// Resize the canvas to the given dimensions.
    pub(crate) fn resize(
        &mut self,
        width: NonZeroU32,
        height: NonZeroU32,
    ) -> Result<(), SoftBufferError> {
        if self.size != Some((width, height)) {
            self.buffer_presented = false;
            self.buffer.resize(total_len(width.get(), height.get()), 0);
            self.canvas.set_width(width.get());
            self.canvas.set_height(height.get());
            self.size = Some((width, height));
        }

        Ok(())
    }

    /// Get a pointer to the mutable buffer.
    pub(crate) fn buffer_mut(&mut self) -> Result<BufferImpl, SoftBufferError> {
        Ok(BufferImpl { imp: self })
    }

    fn present_with_damage(&mut self, damage: &[Rect]) -> Result<(), SoftBufferError> {
        let (width, _height) = self
            .size
            .expect("Must set size of surface before calling `present_with_damage()`");
        // Create a bitmap from the buffer.
        let bitmap: Vec<_> = self
            .buffer
            .iter()
            .copied()
            .flat_map(|pixel| [(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8, 255])
            .collect();

        #[cfg(target_feature = "atomics")]
        let result = {
            use js_sys::{Uint8Array, Uint8ClampedArray};
            use wasm_bindgen::prelude::wasm_bindgen;

            #[wasm_bindgen]
            extern "C" {
                #[wasm_bindgen(js_name = ImageData)]
                type ImageDataExt;

                #[wasm_bindgen(catch, constructor, js_class = ImageData)]
                fn new(array: Uint8ClampedArray, sw: u32) -> Result<ImageDataExt, JsValue>;
            }

            let array = Uint8Array::new_with_length(bitmap.len() as u32);
            array.copy_from(&bitmap);
            let array = Uint8ClampedArray::new(&array);
            ImageDataExt::new(array, width.get())
                .map(JsValue::from)
                .map(ImageData::unchecked_from_js)
        };
        #[cfg(not(target_feature = "atomics"))]
        let result =
            ImageData::new_with_u8_clamped_array(wasm_bindgen::Clamped(&bitmap), width.get());
        // This should only throw an error if the buffer we pass's size is incorrect.
        let image_data = result.unwrap();

        for rect in damage {
            // This can only throw an error if `data` is detached, which is impossible.
            self.canvas
                .put_image_data(
                    &image_data,
                    rect.x.into(),
                    rect.y.into(),
                    rect.width.get().into(),
                    rect.height.get().into(),
                )
                .unwrap();
        }

        self.buffer_presented = true;

        Ok(())
    }

    /// Fetch the buffer from the window.
    pub fn fetch(&mut self) -> Result<Vec<u32>, SoftBufferError> {
        let (width, height) = self
            .size
            .expect("Must set size of surface before calling `fetch()`");

        let image_data = self
            .canvas
            .get_image_data(0., 0., width.get().into(), height.get().into())
            .ok()
            // TODO: Can also error if width or height are 0.
            .swbuf_err("`Canvas` contains pixels from a different origin")?;

        Ok(image_data
            .data()
            .0
            .chunks_exact(4)
            .map(|chunk| u32::from_be_bytes([0, chunk[0], chunk[1], chunk[2]]))
            .collect())
    }
}

/// Extension methods for the Wasm target on [`Surface`](crate::Surface).
pub trait SurfaceExtWeb: Sized {
    /// Creates a new instance of this struct, using the provided [`HtmlCanvasElement`].
    ///
    /// # Errors
    /// - If the canvas was already controlled by an `OffscreenCanvas`.
    /// - If a another context then "2d" was already created for this canvas.
    fn from_canvas<W: HasRawWindowHandle>(
        context: &Context,
        window: &W,
        canvas: HtmlCanvasElement,
    ) -> Result<Self, SoftBufferError>;

    /// Creates a new instance of this struct, using the provided [`HtmlCanvasElement`].
    ///
    /// # Errors
    /// If a another context then "2d" was already created for this canvas.
    fn from_offscreen_canvas(offscreen_canvas: OffscreenCanvas) -> Result<Self, SoftBufferError>;
}

impl SurfaceExtWeb for crate::Surface {
    fn from_canvas<W: HasRawWindowHandle>(
        context: &Context,
        window: &W,
        canvas: HtmlCanvasElement,
    ) -> Result<Self, SoftBufferError> {
        // NOTE: we know a softbuffer context was properly created because we take one in parameters

        let handle = window.raw_window_handle();

        let RawWindowHandle::Web(web_handle) = handle else {
            return Err(SoftBufferError::UnsupportedWindowPlatform {
                human_readable_window_platform_name: crate::window_handle_type_name(
                    &handle,
                ),
                human_readable_display_platform_name: context.context_impl.variant_name(),
                window_handle: handle,
            });
        };

        let attribute_data_raw_handle = canvas
            .get_attribute("data-raw-handle")
            .swbuf_err("No canvas found with the given id")?;

        // Sanity check: the `data-raw-handle` attribute should match the `web_handle.id`
        if attribute_data_raw_handle != web_handle.id.to_string() {
            return Err(SoftBufferError::PlatformError(
                Some("Raw handle ID mismatch".to_owned()),
                None,
            ));
        }

        let imple = crate::SurfaceDispatch::Web(WebImpl::from_canvas(canvas)?);

        Ok(Self {
            surface_impl: Box::new(imple),
            _marker: PhantomData,
        })
    }

    fn from_offscreen_canvas(offscreen_canvas: OffscreenCanvas) -> Result<Self, SoftBufferError> {
        let imple = crate::SurfaceDispatch::Web(WebImpl::from_offscreen_canvas(offscreen_canvas)?);

        Ok(Self {
            surface_impl: Box::new(imple),
            _marker: PhantomData,
        })
    }
}

impl Canvas {
    fn set_width(&self, width: u32) {
        match self {
            Self::Canvas { canvas, .. } => canvas.set_width(width),
            Self::OffscreenCanvas { canvas, .. } => canvas.set_width(width),
        }
    }

    fn set_height(&self, height: u32) {
        match self {
            Self::Canvas { canvas, .. } => canvas.set_height(height),
            Self::OffscreenCanvas { canvas, .. } => canvas.set_height(height),
        }
    }

    fn get_image_data(&self, sx: f64, sy: f64, sw: f64, sh: f64) -> Result<ImageData, JsValue> {
        match self {
            Canvas::Canvas { ctx, .. } => ctx.get_image_data(sx, sy, sw, sh),
            Canvas::OffscreenCanvas { ctx, .. } => ctx.get_image_data(sx, sy, sw, sh),
        }
    }

    fn put_image_data(
        &self,
        imagedata: &ImageData,
        dx: f64,
        dy: f64,
        widht: f64,
        height: f64,
    ) -> Result<(), JsValue> {
        match self {
            Self::Canvas { ctx, .. } => ctx
                .put_image_data_with_dirty_x_and_dirty_y_and_dirty_width_and_dirty_height(
                    imagedata, dx, dy, dx, dy, widht, height,
                ),
            Self::OffscreenCanvas { ctx, .. } => ctx
                .put_image_data_with_dirty_x_and_dirty_y_and_dirty_width_and_dirty_height(
                    imagedata, dx, dy, dx, dy, widht, height,
                ),
        }
    }
}

pub struct BufferImpl<'a> {
    imp: &'a mut WebImpl,
}

impl<'a> BufferImpl<'a> {
    pub fn pixels(&self) -> &[u32] {
        &self.imp.buffer
    }

    pub fn pixels_mut(&mut self) -> &mut [u32] {
        &mut self.imp.buffer
    }

    pub fn age(&self) -> u8 {
        if self.imp.buffer_presented {
            1
        } else {
            0
        }
    }

    /// Push the buffer to the canvas.
    pub fn present(self) -> Result<(), SoftBufferError> {
        let (width, height) = self
            .imp
            .size
            .expect("Must set size of surface before calling `present()`");
        self.imp.present_with_damage(&[Rect {
            x: 0,
            y: 0,
            width,
            height,
        }])
    }

    pub fn present_with_damage(self, damage: &[Rect]) -> Result<(), SoftBufferError> {
        self.imp.present_with_damage(damage)
    }
}

#[inline(always)]
fn total_len(width: u32, height: u32) -> usize {
    // Convert width and height to `usize`, then multiply.
    width
        .try_into()
        .ok()
        .and_then(|w: usize| height.try_into().ok().and_then(|h| w.checked_mul(h)))
        .unwrap_or_else(|| {
            panic!(
                "Overflow when calculating total length of buffer: {}x{}",
                width, height
            );
        })
}
