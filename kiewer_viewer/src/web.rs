//! The browser side: a `Viewer` over a canvas, exported through
//! wasm-bindgen. The page's script owns the event listeners (the wheel
//! listener has to be non-passive, pointer capture is the DOM's) and
//! hands each gesture here in device pixels; the camera, the parse and
//! the tessellation are Rust.
//!
//! WebGPU is tried first, without touching the canvas: an adapter can be
//! asked for before any context exists, so a browser that has
//! `navigator.gpu` but no adapter leaves the canvas free for WebGL2. A
//! failure on both is an error the page shows; nothing here panics on a
//! missing API.

use kicad_core::model::Point;
use wasm_bindgen::prelude::*;
use web_sys::HtmlCanvasElement;

use crate::camera::{Camera, Size};
use crate::render::{MSAA_SAMPLES, Renderer, device_descriptor};
use crate::scene::{Scene, palette};

/// Which backend a viewer ended up on, for the page and the beacon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    WebGpu,
    WebGl2,
}

impl Backend {
    fn name(self) -> &'static str {
        match self {
            Backend::WebGpu => "webgpu",
            Backend::WebGl2 => "webgl2",
        }
    }
}

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
    samples: u32,
}

/// What the browser gave: the surface on the canvas, the adapter behind
/// it and which backend that is.
struct Gpu {
    surface: wgpu::Surface<'static>,
    adapter: wgpu::Adapter,
    backend: Backend,
}

async fn request_gpu(canvas: HtmlCanvasElement) -> Result<Gpu, String> {
    let mut reasons = Vec::new();
    if wgpu::util::is_browser_webgpu_supported().await {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::BROWSER_WEBGPU,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        match instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
        {
            Ok(adapter) => {
                let surface = instance
                    .create_surface(wgpu::SurfaceTarget::Canvas(canvas))
                    .map_err(|error| format!("WebGPU: no surface on the canvas: {error}"))?;
                return Ok(Gpu {
                    surface,
                    adapter,
                    backend: Backend::WebGpu,
                });
            }
            Err(error) => reasons.push(format!("WebGPU: no adapter: {error}")),
        }
    } else {
        reasons.push("WebGPU: navigator.gpu is not available".to_owned());
    }
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::GL,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let surface = instance
        .create_surface(wgpu::SurfaceTarget::Canvas(canvas))
        .map_err(|error| {
            format!(
                "{}; WebGL2: no context on the canvas: {error}",
                reasons.join("; ")
            )
        })?;
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            ..Default::default()
        })
        .await
        .map_err(|error| format!("{}; WebGL2: no adapter: {error}", reasons.join("; ")))?;
    Ok(Gpu {
        surface,
        adapter,
        backend: Backend::WebGl2,
    })
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
    ) -> Result<Viewer, JsError> {
        console_error_panic_hook::set_once();
        let gpu = request_gpu(canvas)
            .await
            .map_err(|reason| JsError::new(&reason))?;
        let (device, queue) = gpu
            .adapter
            .request_device(&device_descriptor())
            .await
            .map_err(|error| {
                JsError::new(&format!("{}: no device: {error}", gpu.backend.name()))
            })?;
        let mut config = gpu
            .surface
            .get_default_config(&gpu.adapter, width.max(1), height.max(1))
            .ok_or_else(|| JsError::new("the adapter does not draw to this canvas"))?;
        config.present_mode = wgpu::PresentMode::Fifo;
        gpu.surface.configure(&device, &config);
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
        let renderer = Renderer::new(&device, config.format, samples);
        let camera = Camera::new(Size {
            width: f64::from(config.width),
            height: f64::from(config.height),
        });
        Ok(Viewer {
            device,
            queue,
            surface: gpu.surface,
            config,
            renderer,
            camera,
            scene: None,
            backend: gpu.backend,
            samples,
        })
    }

    /// `webgpu` or `webgl2`.
    #[must_use]
    pub fn backend(&self) -> String {
        self.backend.name().to_owned()
    }

    /// Samples per pixel the frame is drawn with: 4 with multisampling, 1
    /// without.
    #[must_use]
    pub fn samples(&self) -> u32 {
        self.samples
    }

    /// Parse a `.kicad_pcb` text, tessellate its outline and front
    /// copper, upload them and fit the view to the board. A parse error
    /// is the error's message with its line.
    pub fn load_board(&mut self, text: &str) -> Result<(), JsError> {
        let board =
            kicad_core::parse_board(text).map_err(|error| JsError::new(&error.to_string()))?;
        let Some(scene) = Scene::build(&board, self.config.format) else {
            return Err(JsError::new(
                "the board has no outline and no front copper to draw",
            ));
        };
        self.renderer
            .upload(&self.device, &scene.vertices, &scene.indices);
        self.camera.fit(scene.bounds);
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

    /// Multiply the scale by `factor` about the pointer: a double click,
    /// a Safari gesture.
    pub fn zoom_at(&mut self, x: f64, y: f64, factor: f64) {
        self.camera.zoom_at(Point { x, y }, factor);
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
        }
    }

    /// Draw a frame. A lost or outdated surface is reconfigured and the
    /// frame skipped; the next call draws.
    pub fn render(&mut self) -> Result<(), JsError> {
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
                return Err(JsError::new("the surface refused a frame"));
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
