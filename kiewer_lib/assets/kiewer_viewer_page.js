// What the board page (templates/viewer.html) and the schematic page
// (templates/schematic.html) share: the gestures on the canvas, the
// resize observer, the zoom read-out, the panic hook and the renderer
// beacon. Each page imports it and calls `start` with its canvas and its
// load; the rest of each page listens for the events its load sends.
// Served beside the viewer's script as one more hashed asset
// (kiewer_lib/build.rs, kiewer_lib/src/web/viewer.rs); build.rs rewrites
// the import below to the viewer script's hashed name before hashing
// this file, so a new viewer is a new URL here too.
//
// The page holds the elements this script writes by their ids: `status`
// (the backend line), `error` (the failure box), `zoom` (the read-out)
// and `fit` (the Fit button).
import init, { Viewer } from "./kiewer_viewer.js";

// A wasm panic reaches the page as a bare "unreachable" trap; its message
// goes through console_error_panic_hook to console.error first. This runs
// when the module is evaluated, before any page calls `start`, so a panic
// inside Viewer.create or the page's load is kept too; the beacon at the
// end of `start` reads kiewerPanic and listens for kiewer-panic.
window.kiewerPanic = "";
const originalError = console.error.bind(console);
console.error = function () {
  const text = Array.prototype.map.call(arguments, String).join(" ");
  if (text.startsWith("panicked at")) {
    window.kiewerPanic = text.split("\n\nStack:")[0];
    window.dispatchEvent(new Event("kiewer-panic"));
  }
  originalError.apply(console, arguments);
};

// Shows `message` in the page's error box.
export function fail(message) {
  const errorBox = document.getElementById("error");
  errorBox.textContent = String(message);
  errorBox.hidden = false;
}

// The page owns nothing of the gestures: this hands each one to the wasm
// in device pixels, and the camera (f64) and the drawing are Rust. The
// bindings are KiCad's (docs/v1-scope.md, Rendering, Zoom).
//
// `start(canvas, load)` creates the Viewer on `canvas`, calls
// `load(viewer, schedule)` once it exists, and sends the beacon after;
// `schedule` asks for a frame. A load that throws shows its message in
// the error box. Every frame sends kiewer-frame on the window.
export async function start(canvas, load) {
  const status = document.getElementById("status");
  const errorBox = document.getElementById("error");
  const zoomBox = document.getElementById("zoom");
  let viewer = null;
  let frame = 0;
  let gesturing = false;

  function dpr() { return window.devicePixelRatio || 1; }
  function point(e) {
    const r = canvas.getBoundingClientRect();
    return [(e.clientX - r.left) * dpr(), (e.clientY - r.top) * dpr()];
  }
  function schedule() {
    if (frame || !viewer) return;
    frame = requestAnimationFrame(function (now) {
      frame = 0;
      try { viewer.render(now); } catch (e) { fail(e); }
      showZoom();
      window.dispatchEvent(new Event("kiewer-frame"));
      // A fade asks for its own next frame; otherwise frames are on
      // demand. A sheet never fades.
      if (viewer.fading()) schedule();
    });
  }
  // Lower right: the zoom relative to the fitted board or sheet and the
  // scale in device pixels per millimetre, as the camera holds them.
  function showZoom() {
    if (!viewer) return;
    const zoom = viewer.zoom();
    if (!(zoom > 0)) { zoomBox.textContent = ""; return; }
    zoomBox.textContent = zoom.toPrecision(3).replace(/\.?0+$/, "") + "x fit, "
      + viewer.scale().toPrecision(3) + " px/mm";
  }
  function resize() {
    const r = canvas.getBoundingClientRect();
    canvas.width = Math.max(1, Math.round(r.width * dpr()));
    canvas.height = Math.max(1, Math.round(r.height * dpr()));
    if (viewer) { viewer.resize(canvas.width, canvas.height); schedule(); }
  }

  // Wheel: zoom at the pointer; with ctrl a horizontal pan and with shift
  // a vertical one, as KiCad binds them. A trackpad pinch also arrives as
  // a wheel with ctrlKey, told apart from a mouse wheel with ctrl held by
  // its fractional, small pixel deltas (a mouse notch is an integer of 40
  // or more, or arrives in line or page mode). Non-passive, so
  // preventDefault keeps the page from scrolling or zooming.
  const PINCH_GAIN = 5;
  canvas.addEventListener("wheel", function (e) {
    e.preventDefault();
    if (!viewer) return;
    const [x, y] = point(e);
    const unit = e.deltaMode === 1 ? 16 : e.deltaMode === 2 ? canvas.clientHeight : 1;
    const dx = e.deltaX * unit, dy = e.deltaY * unit;
    const notch = e.deltaMode !== 0 || (Number.isInteger(e.deltaY) && Math.abs(e.deltaY) >= 40);
    if (e.ctrlKey && !notch) {
      if (!gesturing) viewer.wheel(x, y, dy * PINCH_GAIN);
    } else if (e.ctrlKey) {
      viewer.pan(x, y, x - dy, y);
    } else if (e.shiftKey) {
      viewer.pan(x, y, x, y - (dy || dx));
    } else {
      if (dx) viewer.pan(x, y, x - dx, y);
      if (dy) viewer.wheel(x, y, dy);
    }
    schedule();
  }, { passive: false });

  // Pointers: a mouse pans with the middle or right button (the left is
  // the pages' own, for the nets); a touch or pen pans with one pointer
  // and pinches with two.
  const pointers = new Map();
  canvas.addEventListener("pointerdown", function (e) {
    if (e.pointerType === "mouse" && e.button !== 1 && e.button !== 2) return;
    canvas.setPointerCapture(e.pointerId);
    pointers.set(e.pointerId, point(e));
    e.preventDefault();
  });
  canvas.addEventListener("pointermove", function (e) {
    if (!viewer || !pointers.has(e.pointerId)) return;
    const now = point(e);
    if (pointers.size === 2) {
      const [idA, idB] = Array.from(pointers.keys());
      const a0 = pointers.get(idA), b0 = pointers.get(idB);
      const a1 = e.pointerId === idA ? now : a0;
      const b1 = e.pointerId === idB ? now : b0;
      viewer.pinch(a0[0], a0[1], b0[0], b0[1], a1[0], a1[1], b1[0], b1[1]);
    } else if (pointers.size === 1) {
      const was = pointers.get(e.pointerId);
      viewer.pan(was[0], was[1], now[0], now[1]);
    }
    pointers.set(e.pointerId, now);
    schedule();
  });
  function release(e) { pointers.delete(e.pointerId); }
  canvas.addEventListener("pointerup", release);
  canvas.addEventListener("pointercancel", release);
  canvas.addEventListener("contextmenu", function (e) { e.preventDefault(); });
  canvas.addEventListener("dblclick", function (e) {
    if (!viewer) return;
    const [x, y] = point(e);
    viewer.double_click(x, y);
    schedule();
  });

  // Safari reports a trackpad pinch as gesture events with a running
  // scale. While one is under way, a wheel with ctrlKey is the same pinch
  // seen twice and is dropped (the wheel handler checks `gesturing`).
  let gestureScale = 1;
  canvas.addEventListener("gesturestart", function (e) { e.preventDefault(); gestureScale = 1; gesturing = true; });
  canvas.addEventListener("gestureend", function (e) { e.preventDefault(); gesturing = false; });
  canvas.addEventListener("gesturechange", function (e) {
    e.preventDefault();
    if (!viewer || !e.scale) return;
    const [x, y] = point(e);
    viewer.zoom_at(x, y, e.scale / gestureScale);
    gestureScale = e.scale;
    schedule();
  });

  function fit() { if (viewer) { viewer.fit(); schedule(); } }
  document.getElementById("fit").addEventListener("click", fit);
  window.addEventListener("keydown", function (e) {
    if (!viewer || e.target instanceof HTMLInputElement) return;
    if (e.key === "+" || e.key === "=") viewer.step(1);
    else if (e.key === "-" || e.key === "_") viewer.step(-1);
    else if (e.key === "Home") fit();
    else return;
    e.preventDefault();
    schedule();
  });
  new ResizeObserver(resize).observe(canvas);

  try {
    await init();
    resize();
    viewer = await Viewer.create(canvas, canvas.width, canvas.height);
    const samples = viewer.samples();
    status.textContent = viewer.backend() + (samples > 1 ? ", " + samples + "x msaa" : "");
    // On WebGL2, hovering the status says why WebGPU was not used.
    status.title = viewer.fallback_reasons();
    await load(viewer, schedule);
    schedule();
  } catch (e) {
    fail(e && e.message ? e.message : e);
  }

  // Telemetry (docs/v1-scope.md, Telemetry; the server side is
  // kiewer_lib/src/web/beacon.rs): one beacon per page load, after the
  // adapter init above, with the backend, whether navigator.gpu existed,
  // the outcome of that init and the adapter vendor; a wasm panic sends
  // one more with its message. sendBeacon queues the POST and returns at
  // once, so nothing here waits on the network or runs before the first
  // frame is scheduled. The outcome is the adapter's: a board or sheet
  // that fails to fetch or parse after a working adapter is still "ok",
  // and a module import that fails sends nothing, since this never runs.
  // No identifiers, no cookie: the body is the five fields below.
  const BEACON_PATH = "/api/beacon";
  const MAX_REASON = 1024;
  const MAX_VENDOR = 64;
  // The names are read once, here, and never again from the wasm: after a
  // panic inside a &mut self call the object stays borrowed and any call
  // into it throws, so the panic path below touches only these.
  function fromViewer(read, fallback) {
    try { return viewer ? read() : fallback; } catch (e) { return fallback; }
  }
  // The status line was set from viewer.backend() right after create, so
  // it names the backend even when the viewer cannot be asked any more.
  const backendName = fromViewer(function () { return viewer.backend(); },
    /^(webgpu|webgl2)\b/.test(status.textContent) ? status.textContent.split(",")[0] : "none");
  let vendorName = fromViewer(function () { return viewer.adapter_vendor(); }, "");
  let panicSent = false;
  function bounded(text, max) {
    text = String(text || "").slice(0, max);
    // A cut through a surrogate pair leaves a lone high surrogate, which
    // is not JSON the server accepts.
    return /[\uD800-\uDBFF]$/.test(text) ? text.slice(0, -1) : text;
  }
  function sendBeacon(outcome, reason) {
    if (!navigator.sendBeacon) return;
    const body = JSON.stringify({
      backend: backendName,
      webgpu_present: "gpu" in navigator,
      outcome: outcome,
      reason: bounded(reason, MAX_REASON),
      vendor: bounded(vendorName.toLowerCase(), MAX_VENDOR),
    });
    try { navigator.sendBeacon(BEACON_PATH, body); } catch (e) { /* telemetry never fails the page */ }
  }
  function sendPanic() {
    if (panicSent) return;
    panicSent = true;
    sendBeacon("panic", window.kiewerPanic);
  }
  // wgpu names the vendor on WebGL2 from the browser's vendor string and
  // not yet on WebGPU (kiewer_viewer/src/web.rs, vendor_name), where the
  // browser's own adapter info says it when the browser exposes one.
  // Bounded to a second, so a slow adapter request cannot hold the beacon.
  async function webgpuVendor() {
    if (vendorName || backendName !== "webgpu" || !navigator.gpu) return vendorName;
    try {
      const adapter = await Promise.race([
        navigator.gpu.requestAdapter(),
        new Promise(function (resolve) { setTimeout(resolve, 1000, null); }),
      ]);
      return adapter && adapter.info ? adapter.info.vendor : "";
    } catch (e) { return ""; }
  }
  window.addEventListener("kiewer-panic", sendPanic);
  if (window.kiewerPanic) {
    sendPanic();
  } else if (viewer) {
    vendorName = await webgpuVendor();
    sendBeacon("ok", "");
  } else {
    sendBeacon("failed", errorBox.textContent);
  }
}
