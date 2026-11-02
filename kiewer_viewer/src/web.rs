//! The browser side: a `Viewer` over a canvas, exported through
//! wasm-bindgen. The page's script owns the event listeners (the wheel
//! listener has to be non-passive, pointer capture is the DOM's) and
//! hands each gesture here in device pixels; the camera, the parse and
//! the tessellation are Rust.
//!
//! WebGPU is tried first, and its adapter and device are asked for before
//! anything touches the canvas: a canvas keeps the first context it is
//! given, so a WebGPU failure at any step before the surface leaves it
//! free for a WebGL2 context. A failure on both is an [`Error`] the page
//! shows; nothing here panics on a missing API.

use kicad_core::model::Point;
use wasm_bindgen::prelude::*;
use web_sys::HtmlCanvasElement;

use crate::camera::{Camera, DOUBLE_CLICK, Size};
use crate::error::{Error, Result};
use crate::render::{Backend, MSAA_SAMPLES, Renderer, device_descriptor};
use crate::scene::{Scene, palette};

/// The viewer over one canvas.
#[wasm_bindgen]
pub struct Viewer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    renderer: Renderer,
    camera: Camera,
    scene: Option<Scene>,
    backend: Backend,
    fallback: Vec<String>,
    /// The adapter's vendor as [`vendor_name`] read it, for the beacon.
    vendor: String,
    /// The scale the board was last fitted at, 0 before a board: the
    /// zoom the page shows is relative to it.
    fit_scale: f64,
}

/// What the browser gave: the surface on the canvas, the adapter and the
/// device behind it, which backend that is, and why it is not WebGPU
/// when it is not.
struct Gpu {
    surface: wgpu::Surface<'static>,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    backend: Backend,
    fallback: Vec<String>,
}

fn instance(backends: wgpu::Backends) -> wgpu::Instance {
    wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    })
}

/// WebGPU, or `None` with the reason added to `reasons`. The canvas is
/// touched last, once the adapter and the device are in hand.
async fn webgpu(canvas: &HtmlCanvasElement, reasons: &mut Vec<String>) -> Option<Gpu> {
    if !wgpu::util::is_browser_webgpu_supported().await {
        // wgpu's check is both: the property present and an adapter
        // behind it, so Chrome with the API but no usable GPU lands here.
        reasons.push(
            "WebGPU: navigator.gpu is missing (an insecure origin, an old browser) or gave no adapter (chrome://gpu says why)"
                .to_owned(),
        );
        return None;
    }
    let instance = instance(wgpu::Backends::BROWSER_WEBGPU);
    let adapter = match instance
        .request_adapter(&wgpu::RequestAdapterOptions::default())
        .await
    {
        Ok(adapter) => adapter,
        Err(error) => {
            reasons.push(format!("WebGPU: no adapter: {error}"));
            return None;
        }
    };
    let (device, queue) = match adapter.request_device(&device_descriptor()).await {
        Ok(pair) => pair,
        Err(error) => {
            reasons.push(format!("WebGPU: no device: {error}"));
            return None;
        }
    };
    let surface = match instance.create_surface(wgpu::SurfaceTarget::Canvas(canvas.clone())) {
        Ok(surface) => surface,
        Err(error) => {
            reasons.push(format!("WebGPU: no surface on the canvas: {error}"));
            return None;
        }
    };
    Some(Gpu {
        surface,
        adapter,
        device,
        queue,
        backend: Backend::WebGpu,
        fallback: Vec::new(),
    })
}

/// WebGL2, the last resort: the context has to exist before its adapter
/// can be asked for, so the surface comes first here.
async fn webgl2(canvas: HtmlCanvasElement, mut reasons: Vec<String>) -> Result<Gpu> {
    let backend = Backend::WebGl2;
    let instance = instance(wgpu::Backends::GL);
    let surface = instance
        .create_surface(wgpu::SurfaceTarget::Canvas(canvas))
        .map_err(|source| Error::Surface { backend, source })?;
    let adapter = match instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            ..Default::default()
        })
        .await
    {
        Ok(adapter) => adapter,
        Err(error) => {
            reasons.push(format!("WebGL2: no adapter: {error}"));
            return Err(Error::NoAdapter { reasons });
        }
    };
    let (device, queue) = adapter
        .request_device(&device_descriptor())
        .await
        .map_err(|source| Error::Device { backend, source })?;
    Ok(Gpu {
        surface,
        adapter,
        device,
        queue,
        backend,
        fallback: reasons,
    })
}

/// The vendor `info` names, lowercased, for the renderer beacon
/// (docs/v1-scope.md, Telemetry): the PCI vendor id mapped to its name,
/// the id in hex for one outside the list, empty when nothing is known.
/// On WebGL2 the id comes from the browser's vendor string, which
/// Firefox masks to Mozilla (empty here) and Chrome exposes; on WebGPU
/// wgpu 30 maps only the description and leaves the id 0
/// (gfx-rs/wgpu#8819), so the page asks `navigator.gpu` itself when this
/// is empty.
fn vendor_name(info: &wgpu::AdapterInfo) -> String {
    // The ids wgpu-hal's own table knows (auxil/db.rs).
    match info.vendor {
        0 => String::new(),
        0x1002 => "amd".to_owned(),
        0x1010 => "imgtec".to_owned(),
        0x10DE => "nvidia".to_owned(),
        0x13B5 => "arm".to_owned(),
        0x5143 => "qualcomm".to_owned(),
        0x8086 => "intel".to_owned(),
        0x14E4 => "broadcom".to_owned(),
        0x106B => "apple".to_owned(),
        0x10005 => "mesa".to_owned(),
        other => format!("{other:#x}"),
    }
}

async fn request_gpu(canvas: HtmlCanvasElement) -> Result<Gpu> {
    let mut reasons = Vec::new();
    if let Some(gpu) = webgpu(&canvas, &mut reasons).await {
        return Ok(gpu);
    }
    webgl2(canvas, reasons).await
}

#[wasm_bindgen]
impl Viewer {
    /// A viewer drawing into `canvas`, whose backing size is `width` by
    /// `height` device pixels. Fails with a message naming why neither
    /// WebGPU nor WebGL2 could be had.
    pub async fn create(
        canvas: HtmlCanvasElement,
        width: u32,
        height: u32,
    ) -> core::result::Result<Viewer, JsError> {
        console_error_panic_hook::set_once();
        let gpu = request_gpu(canvas).await?;
        // The page shows the backend; why WebGPU was passed over goes to
        // the console, where a check on a browser that should have it
        // finds it.
        if !gpu.fallback.is_empty() {
            web_sys::console::warn_1(
                &format!(
                    "kiewer: WebGL2 after WebGPU failed: {}",
                    gpu.fallback.join("; ")
                )
                .into(),
            );
        }
        let mut config = gpu
            .surface
            .get_default_config(&gpu.adapter, width.max(1), height.max(1))
            .ok_or(Error::NoSurfaceConfig {
                backend: gpu.backend,
            })?;
        config.present_mode = wgpu::PresentMode::Fifo;
        gpu.surface.configure(&gpu.device, &config);
        let samples = if gpu
            .adapter
            .get_texture_format_features(config.format)
            .flags
            .sample_count_supported(MSAA_SAMPLES)
        {
            MSAA_SAMPLES
        } else {
            1
        };
        let vendor = vendor_name(&gpu.adapter.get_info());
        let renderer = Renderer::new(&gpu.device, config.format, samples);
        let camera = Camera::new(Size {
            width: f64::from(config.width),
            height: f64::from(config.height),
        });
        Ok(Viewer {
            device: gpu.device,
            queue: gpu.queue,
            surface: gpu.surface,
            config,
            renderer,
            camera,
            scene: None,
            backend: gpu.backend,
            fallback: gpu.fallback,
            vendor,
            fit_scale: 0.0,
        })
    }

    /// The scale, in device pixels per millimetre.
    #[must_use]
    pub fn scale(&self) -> f64 {
        self.camera.scale()
    }

    /// The zoom relative to the fitted board: 1 right after a fit, 1000
    /// when a millimetre spans a thousand times the pixels it did then.
    /// 0 before a board is loaded.
    #[must_use]
    pub fn zoom(&self) -> f64 {
        if self.fit_scale > 0.0 {
            self.camera.scale() / self.fit_scale
        } else {
            0.0
        }
    }

    /// `webgpu` or `webgl2`.
    #[must_use]
    pub fn backend(&self) -> String {
        self.backend.to_string()
    }

    /// Why the backend is not WebGPU, one reason per step that failed,
    /// joined with `; `; empty on WebGPU.
    #[must_use]
    pub fn fallback_reasons(&self) -> String {
        self.fallback.join("; ")
    }

    /// The adapter's vendor, lowercased (`nvidia`, `intel`, `apple`, ...),
    /// or empty when the browser exposes none; see [`vendor_name`].
    #[must_use]
    pub fn adapter_vendor(&self) -> String {
        self.vendor.clone()
    }

    /// Samples per pixel the frame is drawn with: 4 with multisampling, 1
    /// without.
    #[must_use]
    pub fn samples(&self) -> u32 {
        self.renderer.sample_count()
    }

    /// Parse a `.kicad_pcb` text, tessellate its outline and front
    /// copper, upload them and fit the view to the board. A parse error
    /// is the error's message with its line.
    pub fn load_board(&mut self, text: &str) -> core::result::Result<(), JsError> {
        let board = kicad_core::parse_board(text).map_err(Error::from)?;
        let scene = Scene::build(&board, self.config.format).ok_or(Error::NothingToDraw)?;
        self.renderer
            .upload(&self.device, &scene.vertices, &scene.indices);
        self.camera.fit(scene.bounds);
        self.fit_scale = self.camera.scale();
        self.scene = Some(scene);
        Ok(())
    }

    /// The canvas's backing size changed.
    pub fn resize(&mut self, width: u32, height: u32) {
        let (width, height) = (width.max(1), height.max(1));
        if (width, height) == (self.config.width, self.config.height) {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        self.camera.set_viewport(Size {
            width: f64::from(width),
            height: f64::from(height),
        });
    }

    /// A wheel of `delta` pixels at the pointer: zooms about it.
    pub fn wheel(&mut self, x: f64, y: f64, delta: f64) {
        self.camera.wheel(Point { x, y }, delta);
    }

    /// Multiply the scale by `factor` about the pointer: a Safari pinch
    /// gesture's running scale.
    pub fn zoom_at(&mut self, x: f64, y: f64, factor: f64) {
        self.camera.zoom_at(Point { x, y }, factor);
    }

    /// A double click: one [`DOUBLE_CLICK`] zoom about the pointer.
    pub fn double_click(&mut self, x: f64, y: f64) {
        self.camera.zoom_at(Point { x, y }, DOUBLE_CLICK);
    }

    /// `steps` presses of plus (positive) or minus, at the view's centre.
    pub fn step(&mut self, steps: i32) {
        self.camera.step(steps);
    }

    /// What was under (`from_x`, `from_y`) is now under (`to_x`, `to_y`).
    pub fn pan(&mut self, from_x: f64, from_y: f64, to_x: f64, to_y: f64) {
        self.camera.pan(
            Point {
                x: from_x,
                y: from_y,
            },
            Point { x: to_x, y: to_y },
        );
    }

    /// Two pointers moved from (`ax0`, `ay0`), (`bx0`, `by0`) to
    /// (`ax1`, `ay1`), (`bx1`, `by1`).
    #[expect(
        clippy::too_many_arguments,
        reason = "two pointers before and after are eight numbers at the JavaScript boundary, where a struct would be an object to build on every move"
    )]
    pub fn pinch(
        &mut self,
        ax0: f64,
        ay0: f64,
        bx0: f64,
        by0: f64,
        ax1: f64,
        ay1: f64,
        bx1: f64,
        by1: f64,
    ) {
        self.camera.pinch(
            Point { x: ax0, y: ay0 },
            Point { x: bx0, y: by0 },
            Point { x: ax1, y: ay1 },
            Point { x: bx1, y: by1 },
        );
    }

    /// Fit the board in the view again.
    pub fn fit(&mut self) {
        if let Some(scene) = &self.scene {
            self.camera.fit(scene.bounds);
            self.fit_scale = self.camera.scale();
        }
    }

    /// Draw a frame. A lost or outdated surface is reconfigured and the
    /// frame skipped; the next call draws.
    pub fn render(&mut self) -> core::result::Result<(), JsError> {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err(Error::FrameRefused {
                    backend: self.backend,
                }
                .into());
            }
        };
        let target = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let origin = self
            .scene
            .as_ref()
            .map_or(Point::default(), |scene| scene.origin);
        self.renderer.render(
            &self.device,
            &self.queue,
            &target,
            (self.config.width, self.config.height),
            self.camera.view(origin),
            palette::PAGE.clear_for_format(self.config.format),
        );
        self.queue.present(frame);
        Ok(())
    }
}
