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

use std::collections::{BTreeMap, BTreeSet};

use kicad_core::font::outline::{self, FaceMetrics, Ink};
use kicad_core::font::placed_texts;
use kicad_core::geometry::Bounds;
use kicad_core::hit::{Hit, find_footprint, footprint_bounds, net_pads};
use kicad_core::labels::LabelClass;
use kicad_core::model::{Board, LayerId, Point};
use kicad_core::schematic::hit::SheetHit;
use wasm_bindgen::prelude::*;
use web_sys::HtmlCanvasElement;

use crate::atlas::{self, Plan, Quads};
use crate::camera::{Camera, DOUBLE_CLICK, Size};
use crate::error::{Error, Result};
use crate::highlight::{self, Highlight, SLOP_PX};
use crate::pick::Gesture;
use crate::render::atlas_pipeline::Coverage;
use crate::render::label_pipeline::LabelFrame;
use crate::render::{
    Backend, MSAA_SAMPLES, Renderer, Rgb, Target, cap_canvas, device_descriptor_for,
};
use crate::scene::{Scene, clear_color};
use crate::schematic::{self, SheetScene};
use crate::sheet_highlight::SheetHighlight;
use crate::view::{Layer, Side, View, ViewState, label_key, palette, parse_label_key};

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
    /// The sheet shown instead of a board, when a schematic was loaded
    /// last: its own draws and paper colour, no views.
    sheet: Option<SheetScene>,
    /// The view in force and the toggles on it; the page's layer panel
    /// and the URL fragment mirror it.
    state: ViewState,
    backend: Backend,
    fallback: Vec<String>,
    /// Whether the canvas's size in force is smaller than the page asked
    /// for, [`cap_canvas`] having held it to the device's limit.
    capped: bool,
    /// The adapter's vendor as [`vendor_name`] read it, for the beacon.
    vendor: String,
    /// The scale the board was last fitted at, 0 before a board: the
    /// zoom the page shows is relative to it.
    fit_scale: f64,
    /// The board as parsed, kept for the text the atlas draws.
    board: Option<Board>,
    /// The faces the page draws from the browser's fonts, by the name
    /// the design uses, with what the page measured and planned.
    system_faces: BTreeMap<String, SystemFace>,
    /// The net under the pointer and the pinned ones, with the hit test
    /// over the board (`crate::highlight`).
    highlight: Highlight,
    /// The same over the sheet, with the sheet as parsed, when a
    /// schematic was loaded last (`crate::sheet_highlight`).
    sheet_highlight: Option<SheetHighlight>,
}

/// A face the page rasterises (docs/v1-scope.md, Fonts), as far as the
/// page has got with it.
struct SystemFace {
    /// The face's ascent and descent as the page measured them.
    metrics: FaceMetrics,
    /// The characters its texts draw, in the order the page measures
    /// them.
    chars: Vec<char>,
    /// Each character's advance in em, once the page has measured them;
    /// empty before.
    advances: Vec<f64>,
    /// Each character's ink in em, measured with its advance.
    inks: Vec<Ink>,
    /// Where its atlas stands.
    stage: AtlasStage,
}

/// Where a face's atlas stands.
enum AtlasStage {
    /// Nothing planned: the next `atlas_wanted` names the face.
    Wanted,
    /// Planned, for the page to draw into; the plan carries the pixels
    /// per em the view asked for.
    Planned(Plan),
    /// Uploaded, from a plan made for `wanted` pixels per em: what the
    /// next `atlas_wanted` compares the view's wish against.
    Uploaded { wanted: u32 },
    /// Its cells fit no atlas even at the smallest size; never asked
    /// again, so the page does not try every frame.
    Unfit,
}

impl AtlasStage {
    /// The pixels per em the atlas in hand was asked for, `None` when
    /// there is none yet; `Some(u32::MAX)` for an unfit face, which no
    /// wish exceeds.
    fn planned_for(&self) -> Option<u32> {
        match self {
            AtlasStage::Wanted => None,
            AtlasStage::Planned(plan) => Some(plan.wanted),
            AtlasStage::Uploaded { wanted } => Some(*wanted),
            AtlasStage::Unfit => Some(u32::MAX),
        }
    }
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

/// The backing size for a page asking for `width` by `height` device
/// pixels on `device` ([`cap_canvas`] at the device's limit), and whether
/// the cap held it below the ask.
fn canvas_size(device: &wgpu::Device, width: u32, height: u32) -> ((u32, u32), bool) {
    let wanted = (width.max(1), height.max(1));
    let size = cap_canvas(wanted.0, wanted.1, device.limits().max_texture_dimension_2d);
    (size, size != wanted)
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
    let (device, queue) = match adapter
        .request_device(&device_descriptor_for(&adapter))
        .await
    {
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
        .request_device(&device_descriptor_for(&adapter))
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
    /// A viewer drawing into `canvas`, whose backing size the page asks
    /// to be `width` by `height` device pixels: the viewer sets it,
    /// capped to the device's limit ([`cap_canvas`], [`Viewer::capped`]).
    ///
    /// # Errors
    ///
    /// No GPU to draw with, the message naming why: neither WebGPU nor
    /// WebGL2 gave an adapter, WebGL2 gave no surface on the canvas or no
    /// device, or the adapter in hand does not draw to this canvas.
    ///
    /// # Panics
    ///
    /// Only in the browser tests' build, with the `panic-probe` feature,
    /// when the canvas carries `data-panic-probe`: on purpose.
    pub async fn create(
        canvas: HtmlCanvasElement,
        width: u32,
        height: u32,
    ) -> core::result::Result<Viewer, JsError> {
        console_error_panic_hook::set_once();
        #[cfg(feature = "panic-probe")]
        let probe = canvas.has_attribute("data-panic-probe");
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
        // The browser tests' panic (lib.rs, panic-probe): at the point
        // where an oversized canvas once panicked, so the page meets it
        // with the GPU in hand and no viewer yet.
        #[cfg(feature = "panic-probe")]
        assert!(!probe, "panic-probe: the canvas carries data-panic-probe");
        let ((width, height), capped) = canvas_size(&gpu.device, width, height);
        let mut config = gpu
            .surface
            .get_default_config(&gpu.adapter, width, height)
            .ok_or(Error::NoSurfaceConfig {
                backend: gpu.backend,
            })?;
        config.present_mode = wgpu::PresentMode::Fifo;
        // Configuring sets the canvas's backing size to the config's, on
        // WebGL2 (wgpu-hal) and on WebGPU (wgpu's browser backend) alike.
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
            sheet: None,
            state: ViewState::new(1),
            backend: gpu.backend,
            fallback: gpu.fallback,
            capped,
            vendor,
            fit_scale: 0.0,
            board: None,
            system_faces: BTreeMap::new(),
            highlight: Highlight::default(),
            sheet_highlight: None,
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

    /// Whether the canvas is smaller than the page last asked for, held
    /// to the device's limit: the browser stretches it, a little soft,
    /// and the page sends the `capped` beacon once.
    #[must_use]
    pub fn capped(&self) -> bool {
        self.capped
    }

    /// Samples per pixel the frame is drawn with: 4 with multisampling, 1
    /// without.
    #[must_use]
    pub fn samples(&self) -> u32 {
        self.renderer.sample_count()
    }

    /// Parse a `.kicad_pcb` text, tessellate every layer, upload them,
    /// open on the board's default view ([`Stack::default_view`]) and
    /// fit the camera to the board.
    ///
    /// # Errors
    ///
    /// The text does not parse (malformed, with its line in the message,
    /// or from a KiCad older than 9), or the board has no outline and no
    /// front copper to draw.
    ///
    /// [`Stack::default_view`]: crate::view::Stack::default_view
    pub fn load_board(&mut self, text: &str) -> core::result::Result<(), JsError> {
        let board = kicad_core::parse_board(text).map_err(Error::from)?;
        let scene = Scene::build(&board).ok_or(Error::NothingToDraw)?;
        self.renderer
            .upload(&self.device, &scene.vertices, &scene.indices);
        self.renderer.upload_labels(&self.device, &scene.labels);
        self.renderer.clear_text();
        self.system_faces.clear();
        self.state = ViewState::new(scene.stack.default_view());
        self.camera.fit(scene.bounds);
        self.fit_scale = self.camera.scale();
        self.scene = Some(scene);
        self.sheet = None;
        self.sheet_highlight = None;
        self.highlight = Highlight::new(&board);
        self.board = Some(board);
        self.mirror_for_view();
        Ok(())
    }

    /// Parse a `.kicad_sch` text, tessellate the one sheet, upload it,
    /// build its nets and hit test, and fit the camera to its paper. The
    /// sheet replaces any board: no views, no font round trip, the paper
    /// colour behind it.
    ///
    /// # Errors
    ///
    /// The text does not parse (malformed, with its line in the message,
    /// or from a KiCad older than 9), or the sheet has nothing to draw.
    pub fn load_schematic(&mut self, text: &str) -> core::result::Result<(), JsError> {
        let parsed = kicad_core::parse_schematic(text).map_err(Error::Schematic)?;
        let sheet = schematic::build(&parsed, self.config.format).ok_or(Error::EmptySheet)?;
        self.renderer
            .upload(&self.device, &sheet.vertices, &sheet.indices);
        self.renderer.clear_labels();
        self.renderer.clear_text();
        self.system_faces.clear();
        self.scene = None;
        self.board = None;
        self.highlight = Highlight::default();
        self.sheet_highlight = Some(SheetHighlight::new(parsed));
        self.camera.set_mirrored(false);
        self.camera.fit(sheet.bounds);
        self.fit_scale = self.camera.scale();
        self.sheet = Some(sheet);
        Ok(())
    }

    /// The sheet symbol under the screen point (`x`, `y`), by its index
    /// among the sheet's sheet symbols (the order the page's sheet panel
    /// has them in): what a click enters. Nothing when a wire, pin or
    /// label is there (a sheet pin on the symbol's edge is a net, not a
    /// way in), over empty space, or without a sheet.
    #[must_use]
    pub fn sheet_symbol_at(&self, x: f64, y: f64) -> Option<usize> {
        if self.sheet_hit_at(x, y).is_some() {
            return None;
        }
        let world = self.camera.screen_to_world(Point { x, y });
        self.sheet_highlight
            .as_ref()?
            .index()
            .sheet_symbol_at(world)
    }

    /// The views of the loaded board, their titles in order; the view
    /// numbers the page shows count from 1. Empty before a board.
    #[must_use]
    pub fn views(&self) -> Vec<String> {
        self.scene.as_ref().map_or_else(Vec::new, |scene| {
            scene
                .stack
                .views()
                .into_iter()
                .map(|view| view.title)
                .collect()
        })
    }

    /// The view in force, numbered from 1.
    #[must_use]
    pub fn view(&self) -> usize {
        self.state.view
    }

    /// Switch to view `number` (from 1), dropping every toggle set on the
    /// view before. A number past the last view is ignored.
    pub fn set_view(&mut self, number: usize) {
        let Some(scene) = &self.scene else { return };
        if (1..=scene.views.len()).contains(&number) {
            self.state = self.state.with_view(number);
            self.mirror_for_view();
        }
    }

    /// Every layer the panel lists, by key, in the view's draw order
    /// (bottom first): the board, the outline, the copper, mask, paste,
    /// silkscreen and courtyard of both sides, the holes.
    #[must_use]
    pub fn layers(&self) -> Vec<String> {
        match (&self.scene, self.current_view()) {
            (Some(scene), Some(view)) => scene
                .stack
                .order(view.top)
                .iter()
                .map(ToString::to_string)
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Whether the layer `key` names is shown under the view in force.
    #[must_use]
    pub fn layer_shown(&self, key: &str) -> bool {
        self.layer_state(key).is_some()
    }

    /// The opacity the layer `key` names draws at, 0 to 1. For a hidden
    /// layer, the opacity the view in force gives it, or 1 when the view
    /// does not show it either: where the slider starts when it is ticked.
    #[must_use]
    pub fn layer_opacity(&self, key: &str) -> f64 {
        let opacity = self.layer_state(key).or_else(|| {
            let layer = Layer::parse(key)?;
            self.current_view()?.layers.get(&layer).copied()
        });
        f64::from(opacity.unwrap_or(1.0))
    }

    /// The colour the layer `key` names draws in under the view in force,
    /// as CSS writes it (`#f0c030`), for the panel's swatch; empty for an
    /// unknown layer or before a board.
    #[must_use]
    pub fn layer_color(&self, key: &str) -> String {
        let (Some(scene), Some(view), Some(layer)) =
            (&self.scene, self.current_view(), Layer::parse(key))
        else {
            return String::new();
        };
        view.color(&scene.stack, &layer).to_css()
    }

    /// Show the layer `key` names at `opacity`, or hide it: a toggle on
    /// top of the view in force.
    pub fn set_layer(&mut self, key: &str, shown: bool, opacity: f64) {
        let Some(layer) = Layer::parse(key) else {
            return;
        };
        let Some(view) = self.current_view().cloned() else {
            return;
        };
        #[expect(
            clippy::cast_possible_truncation,
            reason = "an opacity is 0 to 1, which f32 holds to the hundredth the state keeps"
        )]
        let opacity = opacity as f32;
        self.state.set(&view, layer, shown.then_some(opacity));
    }

    /// The state as the URL fragment carries it, without the `#`:
    /// `view=5&F.Cu=0.5&F.SilkS=off`.
    #[must_use]
    pub fn fragment(&self) -> String {
        self.state.fragment()
    }

    /// Set the state from a URL fragment, `#` or not: the view it names
    /// when the board has it (else the default view) with the toggles it
    /// lists. Nothing before a board.
    pub fn apply_fragment(&mut self, fragment: &str) {
        let Some(scene) = &self.scene else { return };
        self.state = ViewState::from_fragment(fragment, &scene.stack);
        self.mirror_for_view();
    }

    /// Whether the view in force is mirrored, seen from the back.
    #[must_use]
    pub fn mirrored(&self) -> bool {
        self.camera.mirrored()
    }

    /// One line per thing the page should say about the board: a zone
    /// drawn hatched for want of a stored fill. Empty when all is well.
    #[must_use]
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if let Some(scene) = &self.scene
            && scene.unfilled_zones > 0
        {
            let zones = if scene.unfilled_zones == 1 {
                "1 zone has no stored fill and is drawn hatched".to_owned()
            } else {
                format!(
                    "{} zones have no stored fill and are drawn hatched",
                    scene.unfilled_zones
                )
            };
            warnings.push(format!(
                "{zones}: fill the zones in KiCad and save the board"
            ));
        }
        warnings
    }

    /// The net label classes the page toggles, by key, in their order:
    /// `pad.number`, `pad.net`, `via.net`, `track.net`, `zone.net`.
    #[must_use]
    pub fn label_classes(&self) -> Vec<String> {
        LabelClass::ALL
            .into_iter()
            .map(|class| label_key(class).to_owned())
            .collect()
    }

    /// Whether the label class `key` names is shown; false for an
    /// unknown key.
    #[must_use]
    pub fn label_shown(&self, key: &str) -> bool {
        parse_label_key(key).is_some_and(|class| self.state.labels.shown(class))
    }

    /// Show or hide the label class `key` names.
    pub fn set_label(&mut self, key: &str, shown: bool) {
        if let Some(class) = parse_label_key(key) {
            self.state.labels.set_shown(class, shown);
        }
    }

    /// The text height on screen, in device pixels, a label starts to
    /// show at.
    #[must_use]
    pub fn label_threshold(&self) -> f64 {
        f64::from(self.state.labels.threshold)
    }

    /// Set the label threshold in device pixels; clamped to the range
    /// the fragment carries ([`LABEL_THRESHOLD_RANGE`]).
    ///
    /// [`LABEL_THRESHOLD_RANGE`]: crate::view::LABEL_THRESHOLD_RANGE
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a threshold is a few pixels, which f32 holds to the tenth the state keeps"
    )]
    pub fn set_label_threshold(&mut self, px: f64) {
        self.state.labels.set_threshold(px as f32);
    }

    /// The view the state names, for the loaded board.
    fn current_view(&self) -> Option<&View> {
        self.scene.as_ref()?.view(&self.state)
    }

    /// The opacity of the layer `key` names, or `None` when hidden or
    /// unknown.
    fn layer_state(&self, key: &str) -> Option<f32> {
        let layer = Layer::parse(key)?;
        self.state.opacity(self.current_view()?, &layer)
    }

    /// Whether the named-face text, which the atlas draws over every
    /// layer, belongs in the frame: its layer, the front silkscreen, is
    /// shown. Its opacity is the follow-up issue-10 files.
    fn named_text_shown(&self) -> bool {
        self.layer_state(&Layer::Silk(Side::Front).to_string())
            .is_some()
    }

    /// What this frame draws the net labels with: the camera's scale and
    /// mirror, the state's threshold and toggles, and the copper layers
    /// the view shows in its draw order. `None` without a board.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "pixels per millimetre is of order 1e-3 to 1e7, well within f32"
    )]
    fn label_frame(&self) -> Option<LabelFrame> {
        let scene = self.scene.as_ref()?;
        scene.view(&self.state)?;
        Some(LabelFrame {
            px_per_mm: self.camera.scale() as f32,
            threshold_px: self.state.labels.threshold,
            mirrored: self.camera.mirrored(),
            classes: self.state.labels.mask(),
            layers: highlight::shown_copper(scene, &self.state),
            dim: self.highlight.pick().dim(),
        })
    }

    /// Mirror the camera as the view in force asks.
    fn mirror_for_view(&mut self) {
        let mirrored = self.current_view().is_some_and(|view| view.mirrored);
        self.camera.set_mirrored(mirrored);
    }

    /// The page asks for a backing size of `width` by `height` device
    /// pixels: the viewer sets the canvas to it, capped to the device's
    /// limit as [`Viewer::create`] does.
    pub fn resize(&mut self, width: u32, height: u32) {
        let ((width, height), capped) = canvas_size(&self.device, width, height);
        self.capped = capped;
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

    /// Fit the board, or the sheet, in the view again.
    pub fn fit(&mut self) {
        let bounds = match (&self.sheet, &self.scene) {
            (Some(sheet), _) => sheet.bounds,
            (None, Some(scene)) => scene.bounds,
            (None, None) => return,
        };
        self.camera.fit(bounds);
        self.fit_scale = self.camera.scale();
    }

    /// Draw a frame for the time `now`, in the page's milliseconds, which
    /// moves a fade in progress on; [`Viewer::fading`] says whether
    /// another frame is due after it. A lost or outdated surface is
    /// reconfigured and the frame skipped; the next call draws.
    ///
    /// # Errors
    ///
    /// The surface refuses a frame for a reason that is not a resize: a
    /// validation failure in the configuration.
    pub fn render(&mut self, now: f64) -> core::result::Result<(), JsError> {
        match &mut self.sheet_highlight {
            Some(sheet) => sheet.pick_mut().advance(now),
            None => self.highlight.pick_mut().advance(now),
        }
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
        // The lit nets' copper rides after the scene's own geometry; when
        // they changed since the last write, the buffers take the change
        // before the draws name the new ranges.
        if let (Some(board), Some(scene)) = (&self.board, &self.scene)
            && let Some(splice) = self.highlight.geometry(board, scene)
        {
            self.renderer.splice(&self.device, &self.queue, &splice);
        }
        if let (Some(sheet), Some(highlight)) = (&self.sheet, &mut self.sheet_highlight)
            && let Some(splice) = highlight.geometry(sheet)
        {
            self.renderer.splice(&self.device, &self.queue, &splice);
        }
        // A sheet, when one is loaded, has its own origin, draws and
        // paper; otherwise the board's view in force says what is drawn.
        // Either is dimmed around the lit nets when there are any.
        let (origin, draws, clear) = match (&self.sheet, &self.scene) {
            (Some(sheet), _) => (
                sheet.origin,
                self.sheet_highlight.as_ref().map_or_else(
                    || sheet.draws.clone(),
                    |h| h.draws(sheet, self.config.format),
                ),
                schematic::clear_color(self.config.format),
            ),
            (None, Some(scene)) => (
                scene.origin,
                self.highlight.draws(scene, &self.state, self.config.format),
                clear_color(self.config.format),
            ),
            (None, None) => (
                Point::default(),
                Vec::new(),
                clear_color(self.config.format),
            ),
        };
        self.renderer.set_label_frame(self.label_frame());
        self.renderer.render(
            &self.device,
            &self.queue,
            &Target {
                view: &target,
                size: (self.config.width, self.config.height),
                clear,
            },
            self.camera.view(origin),
            &draws,
            self.named_text_shown(),
        );
        self.queue.present(frame);
        Ok(())
    }
}

/// The font side of the viewer (docs/v1-scope.md, Fonts; `crate::atlas`):
/// which faces the board names, and the round trip by which the page
/// rasterises one. The page calls `font_faces` after `load_board`, probes
/// and asks the person, then either `set_face_stroke` (Newstroke draws
/// the face, the default) or `set_face_system` with the face's measured
/// ascent and descent. From then on, after every frame the page asks
/// `atlas_wanted`; for the face it names, the page measures the advances
/// and the ink of `face_chars` at some pixel size, hands them to
/// `atlas_plan`, draws each character into the rectangle the plan gives
/// at the plan's pixels per em, and hands the alpha channel to
/// `atlas_upload`. The wasm keeps
/// the layout (`kicad_core`) and the quads; the page keeps the font.
#[wasm_bindgen]
impl Viewer {
    /// The faces the board's drawn text names, distinct, in order of
    /// first appearance; empty before a board or for a board whose text
    /// is all in the stroke font.
    #[must_use]
    pub fn font_faces(&self) -> Vec<String> {
        self.silk()
            .map(|(board, layer)| atlas::faces_on(board, layer))
            .unwrap_or_default()
    }

    /// The distinct characters the texts in `face` draw, sorted, a space
    /// among them: what the page measures and rasterises, in this order.
    #[must_use]
    pub fn face_chars(&self, face: &str) -> String {
        self.silk()
            .map(|(board, layer)| atlas::chars_in(board, layer, face).into_iter().collect())
            .unwrap_or_default()
    }

    /// Draw `face` in Newstroke: the default, and the choice when the
    /// browser lacks the face and the person picks nothing else.
    pub fn set_face_stroke(&mut self, face: &str) {
        self.system_faces.remove(face);
        self.renderer.remove_text(face);
        self.rebuild_scene();
    }

    /// Draw `face` from a font the browser has, whose line box reaches
    /// `ascent` em above the baseline and `descent` em below it; NaN or
    /// nothing for either (a browser without `fontBoundingBoxAscent`)
    /// takes the usual 0.8 and 0.2. The face's text leaves the Newstroke
    /// scene; the next `atlas_wanted` names the face, and until the page
    /// uploads its atlas the text is not drawn.
    pub fn set_face_system(&mut self, face: &str, ascent: f64, descent: f64) {
        let chars = self
            .silk()
            .map(|(board, layer)| atlas::chars_in(board, layer, face))
            .unwrap_or_default();
        let metrics = FaceMetrics {
            ascent: if ascent.is_finite() && ascent > 0.0 {
                ascent
            } else {
                0.8
            },
            descent: if descent.is_finite() && descent >= 0.0 {
                descent
            } else {
                0.2
            },
        };
        self.system_faces.insert(
            face.to_owned(),
            SystemFace {
                metrics,
                chars,
                advances: Vec::new(),
                inks: Vec::new(),
                stage: AtlasStage::Wanted,
            },
        );
        self.renderer.remove_text(face);
        self.rebuild_scene();
    }

    /// The face whose atlas the page should rasterise now, for the
    /// current zoom (`crate::atlas::needs_rebuild`), or empty when every
    /// face's atlas fits the view.
    #[must_use]
    pub fn atlas_wanted(&self) -> String {
        self.system_faces
            .iter()
            .find(|(face, system)| {
                atlas::needs_rebuild(system.stage.planned_for(), self.wanted_px(face))
            })
            .map(|(face, _)| face.clone())
            .unwrap_or_default()
    }

    /// Plan `face`'s atlas from the page's measurement: `advances` is
    /// the width of each character of `face_chars` at `px` pixels per
    /// em, in that order, and `inks` four numbers per character in the
    /// same order and size, `measureText`'s `actualBoundingBoxLeft`,
    /// `Right`, `Ascent` and `Descent` (a character whose four are not
    /// all numbers counts as inked to its cell, the face's line box one
    /// advance wide). Returns the pixels per em to draw at, the
    /// atlas's width and height, the baseline's distance below a cell's
    /// top in pixels (the face's ascent, as the layout places it), then
    /// `x, y, width, height` per character, all whole texels; empty when
    /// the face is unknown, the measurement does not match the
    /// characters, or the cells fit no atlas.
    #[must_use]
    pub fn atlas_plan(&mut self, face: &str, px: u32, advances: &[f64], inks: &[f64]) -> Vec<f64> {
        let wanted = self.wanted_px(face);
        let Some(system) = self.system_faces.get_mut(face) else {
            return Vec::new();
        };
        if px == 0 || advances.len() != system.chars.len() || inks.len() != 4 * advances.len() {
            return Vec::new();
        }
        let px_per_em = f64::from(px);
        system.advances = advances.iter().map(|advance| advance / px_per_em).collect();
        system.inks = system
            .advances
            .iter()
            .zip(inks.as_chunks::<4>().0)
            .map(|(&advance, &ink)| measured_ink(ink, px_per_em, advance, system.metrics))
            .collect();
        let Some(plan) = atlas::plan(&system.advances, system.metrics.line_height(), wanted) else {
            system.stage = AtlasStage::Unfit;
            return Vec::new();
        };
        let mut out = vec![
            f64::from(plan.px),
            f64::from(plan.packing.width),
            f64::from(plan.packing.height),
            system.metrics.ascent * f64::from(plan.px),
        ];
        for rect in &plan.packing.rects {
            out.extend([rect.x, rect.y, rect.width, rect.height].map(f64::from));
        }
        system.stage = AtlasStage::Planned(plan);
        out
    }

    /// `face`'s atlas as the page drew it to the last plan: one byte of
    /// coverage per texel, row-major, the plan's width times its height.
    /// Lays the face's texts out, builds their quads and uploads both.
    pub fn atlas_upload(&mut self, face: &str, coverage: &[u8]) {
        let Some(system) = self.system_faces.get(face) else {
            return;
        };
        let (AtlasStage::Planned(plan), Some(scene), Some((board, layer))) =
            (&system.stage, &self.scene, self.silk())
        else {
            return;
        };
        let glyphs = plan.glyphs(
            &system.chars,
            &system.advances,
            system.metrics.line_height(),
        );
        let advances: BTreeMap<char, f64> = system
            .chars
            .iter()
            .copied()
            .zip(system.advances.iter().copied())
            .collect();
        let advance = |c: char| advances.get(&c).copied().unwrap_or(0.0);
        let inks: BTreeMap<char, Ink> = system
            .chars
            .iter()
            .copied()
            .zip(system.inks.iter().copied())
            .collect();
        let ink = |c: char| inks.get(&c).copied().unwrap_or_default();
        // The silkscreen's colour under the view in force, as the flat
        // slices have it; a later view change recolours at the next
        // rebuild (#46 has the per-view text).
        let silk = Layer::Silk(Side::Front);
        let color = self
            .current_view()
            .map_or(palette::F_SILKS, |view| view.color(&scene.stack, &silk))
            .for_format(self.config.format);
        let mut quads = Quads::default();
        for (content, placement, text) in placed_texts(board, layer) {
            if text.font.face.as_deref() != Some(face) || content.is_empty() {
                continue;
            }
            let laid = outline::layout(
                &content,
                &placement,
                system.metrics,
                advance,
                ink,
                text.knockout,
            );
            quads.add(&laid, &glyphs, scene.origin, color, text.knockout);
        }
        self.renderer.upload_text(
            &self.device,
            &self.queue,
            face,
            &Coverage {
                width: glyphs.width,
                height: glyphs.height,
                bytes: coverage,
            },
            &quads.vertices,
            &quads.indices,
        );
        let wanted = plan.wanted;
        if let Some(system) = self.system_faces.get_mut(face) {
            system.stage = AtlasStage::Uploaded { wanted };
        }
    }
}

/// One character's ink from the page's four numbers at `px_per_em`
/// (`measureText`'s actual bounding box left, right, ascent and
/// descent), in em; when any is not a number (a browser without them),
/// the whole of its cell, `advance` em wide in a face of `metrics`.
fn measured_ink(ink: [f64; 4], px_per_em: f64, advance: f64, metrics: FaceMetrics) -> Ink {
    let [left, right, ascent, descent] = ink;
    if ink.iter().all(|value| value.is_finite()) {
        Ink {
            left: left / px_per_em,
            right: right / px_per_em,
            ascent: ascent / px_per_em,
            descent: descent / px_per_em,
        }
    } else {
        Ink {
            left: 0.0,
            right: advance,
            ascent: metrics.ascent,
            descent: metrics.descent,
        }
    }
}

impl Viewer {
    /// The board and the layer whose text the viewer draws.
    fn silk(&self) -> Option<(&Board, LayerId)> {
        let board = self.board.as_ref()?;
        let silk = Layer::Silk(Side::Front).to_string();
        Some((board, board.layer_named(&silk)?))
    }

    /// The pixels per em `face` wants at the camera's scale.
    fn wanted_px(&self, face: &str) -> u32 {
        let height = self
            .silk()
            .map_or(0.0, |(board, layer)| atlas::max_height(board, layer, face));
        atlas::wanted_px(height, self.camera.scale())
    }

    /// Rebuild the Newstroke scene with the system faces' texts left
    /// out, keeping the camera and the view state where they are.
    fn rebuild_scene(&mut self) {
        let Some(board) = &self.board else {
            return;
        };
        let system: BTreeSet<String> = self.system_faces.keys().cloned().collect();
        let stroke = atlas::stroke_board(board, &system);
        if let Some(scene) = Scene::build(&stroke) {
            self.renderer
                .upload(&self.device, &scene.vertices, &scene.indices);
            self.renderer.upload_labels(&self.device, &scene.labels);
            self.scene = Some(scene);
            // The scene alone is on the GPU now; the next frame appends
            // the highlighted net's copper again.
            self.highlight.invalidate();
        }
    }
}

/// The net interaction and the reference search (docs/v1-scope.md, Net
/// interaction, Search box), on the board (the state is `crate::pick`
/// behind `crate::highlight`, the hit test `kicad_core::hit`) or on the
/// sheet when a schematic was loaded last (`crate::sheet_highlight`,
/// `kicad_core::schematic::hit`), so the schematic page's net script is
/// the board page's. The page hands each pointer position here in device
/// pixels: `hover` on a move, `click`, `modified_click` or `tap` on a
/// press, `hover_leave` when the pointer leaves the canvas, `clear_net`
/// on Escape and `undo_pin` on Ctrl or Cmd with Z. Each says whether a
/// frame is due. `pinned_nets` is what the page lists, one row per pin,
/// `net_pads` a row's pads, `go_to_pad` and `search_reference` move the
/// camera.
#[wasm_bindgen]
impl Viewer {
    /// The pointer is at (`x`, `y`): the item there, within [`SLOP_PX`]
    /// pixels, is hovered. Whether a frame is due; the tooltip is
    /// [`Viewer::hover_text`].
    pub fn hover(&mut self, x: f64, y: f64) -> bool {
        if self.sheet_highlight.is_some() {
            let hit = self.sheet_hit_at(x, y);
            return self
                .sheet_highlight
                .as_mut()
                .is_some_and(|sheet| sheet.pick_mut().hover(hit));
        }
        let hit = self.hit_at(x, y);
        self.highlight.pick_mut().hover(hit)
    }

    /// The pointer left the canvas: nothing hovered. Whether a frame is
    /// due.
    pub fn hover_leave(&mut self) -> bool {
        match &mut self.sheet_highlight {
            Some(sheet) => sheet.pick_mut().hover(None),
            None => self.highlight.pick_mut().hover(None),
        }
    }

    /// What the pointer is over, for the tooltip: `LED6 pad 3: GND`,
    /// `via: GND`, `track: GND`, `zone: GND` on a board; `wire: VIN`,
    /// `R1 pin 2: GND`, `label: VIN`, `junction: VIN`, `sheet pin VIN:
    /// VIN` on a sheet; empty over nothing.
    #[must_use]
    pub fn hover_text(&self) -> String {
        if let Some(sheet) = &self.sheet_highlight {
            return sheet.tooltip();
        }
        self.board
            .as_ref()
            .map(|board| self.highlight.tooltip(board))
            .unwrap_or_default()
    }

    /// A plain mouse click at (`x`, `y`): the net of the item there
    /// becomes the one pin, and empty space clears them all. Whether a
    /// frame is due.
    pub fn click(&mut self, x: f64, y: f64) -> bool {
        self.press(Gesture::Click, x, y)
    }

    /// A mouse click with Ctrl or Cmd at (`x`, `y`): the net of the item
    /// there is added or unpinned, and empty space does nothing. Whether
    /// a frame is due.
    pub fn modified_click(&mut self, x: f64, y: f64) -> bool {
        self.press(Gesture::ModifiedClick, x, y)
    }

    /// A touch or a pen at (`x`, `y`): the net of the item there is added
    /// or unpinned, and empty space clears them all. Whether a frame is
    /// due.
    pub fn tap(&mut self, x: f64, y: f64) -> bool {
        self.press(Gesture::Tap, x, y)
    }

    /// Escape: nothing pinned, nothing hovered. Whether a frame is due.
    pub fn clear_net(&mut self) -> bool {
        match &mut self.sheet_highlight {
            Some(sheet) => sheet.pick_mut().clear(),
            None => self.highlight.pick_mut().clear(),
        }
    }

    /// Step back one pin change. Whether there was one, so a frame is
    /// due.
    pub fn undo_pin(&mut self) -> bool {
        match &mut self.sheet_highlight {
            Some(sheet) => sheet.pick_mut().undo(),
            None => self.highlight.pick_mut().undo(),
        }
    }

    /// Whether a fade is in progress, so the page draws another frame.
    #[must_use]
    pub fn fading(&self) -> bool {
        match &self.sheet_highlight {
            Some(sheet) => sheet.pick().fading(),
            None => self.highlight.pick().fading(),
        }
    }

    /// `prefers-reduced-motion`: the same levels on a board, drawn
    /// instantly. A sheet is instant anyway.
    pub fn set_reduced_motion(&mut self, reduced: bool) {
        self.highlight.pick_mut().set_instant(reduced);
    }

    /// One row per pinned net in pin order, for the net block: its colour
    /// slot, a tab, its swatch's colours as CSS writes them, separated by
    /// spaces, a tab, and its name, which may be empty or repeat on a
    /// sheet; the slot tells two rows apart. A row's position is what
    /// [`Viewer::net_pads`] and [`Viewer::go_to_pad`] take.
    #[must_use]
    pub fn pinned_nets(&self) -> Vec<String> {
        if let Some(sheet) = &self.sheet_highlight {
            return sheet
                .pick()
                .pins()
                .iter()
                .map(|pinned| {
                    let swatch = SheetHighlight::swatch(pinned.slot).to_css();
                    let name = sheet.net_name(pinned.net);
                    format!("{}\t{swatch}\t{name}", pinned.slot)
                })
                .collect();
        }
        let (Some(board), Some(scene)) = (&self.board, &self.scene) else {
            return Vec::new();
        };
        self.highlight
            .pick()
            .pins()
            .iter()
            .map(|pinned| {
                let swatch: Vec<String> = self
                    .highlight
                    .swatch(scene, &self.state, pinned.slot)
                    .into_iter()
                    .map(Rgb::to_css)
                    .collect();
                let name = board.net(pinned.net).map_or("", |net| net.name.as_str());
                format!("{}\t{}\t{name}", pinned.slot, swatch.join(" "))
            })
            .collect()
    }

    /// The pads of the net in row `row` of [`Viewer::pinned_nets`] (its
    /// pins, on a sheet), one `reference<tab>number` per pad in natural
    /// order (`R2` before `R10`); the position in this list is what
    /// [`Viewer::go_to_pad`] takes. Empty past the last row.
    #[must_use]
    pub fn net_pads(&self, row: usize) -> Vec<String> {
        if let Some(sheet) = &self.sheet_highlight {
            return sheet
                .pinned_pins(row)
                .into_iter()
                .map(|pin| format!("{}\t{}", pin.reference, pin.number))
                .collect();
        }
        let (Some(board), Some(pinned)) = (&self.board, self.highlight.pick().pins().get(row))
        else {
            return Vec::new();
        };
        net_pads(board, pinned.net)
            .into_iter()
            .map(|pad| format!("{}\t{}", pad.reference, pad.number))
            .collect()
    }

    /// Pan to pad `index` of [`Viewer::net_pads`] of `row`, zooming in
    /// only when it would be too small to see. Whether a frame is due.
    pub fn go_to_pad(&mut self, row: usize, index: usize) -> bool {
        if let Some(sheet) = &self.sheet_highlight {
            return sheet.go_to_pin(&mut self.camera, row, index);
        }
        let (Some(board), Some(pinned)) = (&self.board, self.highlight.pick().pins().get(row))
        else {
            return false;
        };
        let Some(pad) = net_pads(board, pinned.net).into_iter().nth(index) else {
            return false;
        };
        highlight::go_to_pad(&mut self.camera, pad.bounds);
        true
    }

    /// Frame the footprint (the symbol, on a sheet) whose reference
    /// designator is `reference`, letter case aside; one with neither
    /// pads nor graphics is framed as a millimetre around its origin.
    /// False, with the camera untouched, when there is no such one.
    pub fn search_reference(&mut self, reference: &str) -> bool {
        if let Some(sheet) = &self.sheet_highlight {
            return sheet.search_reference(&mut self.camera, reference);
        }
        let Some(board) = &self.board else {
            return false;
        };
        let Some(footprint) =
            find_footprint(board, reference).and_then(|index| board.footprints.get(index))
        else {
            return false;
        };
        let bounds = footprint_bounds(footprint).unwrap_or(Bounds {
            min: Point {
                x: footprint.at.x - 1.0,
                y: footprint.at.y - 1.0,
            },
            max: Point {
                x: footprint.at.x + 1.0,
                y: footprint.at.y + 1.0,
            },
        });
        highlight::go_to_footprint(&mut self.camera, bounds);
        true
    }
}

impl Viewer {
    /// `gesture` on the item at the screen point (`x`, `y`). Whether a
    /// frame is due.
    fn press(&mut self, gesture: Gesture, x: f64, y: f64) -> bool {
        if self.sheet_highlight.is_some() {
            let hit = self.sheet_hit_at(x, y);
            return self
                .sheet_highlight
                .as_mut()
                .is_some_and(|sheet| sheet.pick_mut().press(gesture, hit));
        }
        let hit = self.hit_at(x, y);
        self.highlight.pick_mut().press(gesture, hit)
    }

    /// The item under the screen point (`x`, `y`) on the copper layers
    /// the view in force shows, top first, within [`SLOP_PX`] pixels.
    fn hit_at(&self, x: f64, y: f64) -> Option<Hit> {
        let (board, scene) = (self.board.as_ref()?, self.scene.as_ref()?);
        let layers: Vec<LayerId> = highlight::shown_copper(scene, &self.state)
            .into_iter()
            .rev()
            .filter_map(|layer| board.layer_named(&layer.to_string()))
            .collect();
        let world = self.camera.screen_to_world(Point { x, y });
        self.highlight
            .index()
            .hit(world, SLOP_PX / self.camera.scale(), &layers)
    }

    /// The item under the screen point (`x`, `y`) on the sheet, within
    /// [`SLOP_PX`] pixels.
    fn sheet_hit_at(&self, x: f64, y: f64) -> Option<SheetHit> {
        let world = self.camera.screen_to_world(Point { x, y });
        self.sheet_highlight
            .as_ref()?
            .hit_at(world, SLOP_PX / self.camera.scale())
    }
}
