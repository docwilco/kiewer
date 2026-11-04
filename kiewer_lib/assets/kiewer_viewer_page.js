// What the board page (templates/viewer.html) and the schematic page
// (templates/schematic.html) share: the gestures on the canvas, the
// resize observer, the zoom read-out, the panic hook and the renderer
// beacon. Each page imports it and calls `start` with its canvas and its
// load; the rest of each page wakes on the event its load sends
// (kiewer-board, kiewer-sheet) and on kiewer-frame, which this sends.
// Served beside the viewer's script as one more hashed asset
// (kiewer_lib/build.rs, kiewer_lib/src/web/viewer.rs); build.rs rewrites
// the import in importViewer to the viewer script's hashed name before
// hashing this file, so a new viewer is a new URL here too.
//
// The page holds the elements this script writes by their ids: `status`
// (the backend line), `error` (the failure box), `zoom` (the read-out)
// and `fit` (the Fit button).

// Telemetry (docs/v1-scope.md, Telemetry; the server side is
// kiewer_lib/src/web/beacon.rs): one beacon per page load, after the
// adapter init in `start`, with the backend, whether navigator.gpu
// existed, the outcome of that init and the adapter vendor; a wasm panic
// sends one more with its message, and a capped canvas one `capped`
// (kiewer_lib/src/web/beacon.rs, Outcome). sendBeacon queues the POST
// and returns at once, so nothing here waits on the network or runs
// before the first frame is scheduled. The outcome is the adapter's: a
// board or sheet that fails to fetch or parse after a working adapter is
// still "ok". A viewer script that fails to import is "failed" with
// backend "none" and the reason `import: <message>` (importViewer). No
// identifiers, no cookie: the body is the five fields below.
const BEACON_PATH = "/api/beacon";
const MAX_REASON = 1024;
const MAX_VENDOR = 64;
// The backend and the vendor as the Viewer named them when it was
// created, "none" and empty before. They are read once and never again
// from the wasm: after a panic inside a &mut self call the object stays
// borrowed and any call into it throws, so the panic path touches only
// these.
let backendName = "none";
let vendorName = "";
// `text` cut to `max` UTF-16 units, as the server's bounds allow.
function bounded(text, max) {
  text = String(text || "").slice(0, max);
  // A cut through a surrogate pair leaves a lone high surrogate, which
  // is not JSON the server accepts.
  return /[\uD800-\uDBFF]$/.test(text) ? text.slice(0, -1) : text;
}
// Sends one beacon with `outcome` and `reason`; the other fields are the
// module's.
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

// The panic hook. A wasm panic reaches the page as a bare "unreachable"
// trap, and a trap inside an async call such as Viewer.create leaves its
// promise unsettled, so no catch and nothing after an await runs. The
// message goes through console_error_panic_hook to console.error first,
// so the hook acts there and then: it shows the panic in the error box
// and sends the panic beacon, with backend "none" when no Viewer exists
// yet. It is set when the module is evaluated, before any page calls
// `start` and before the wasm is fetched, so a panic at any stage is
// shown and sent. The first panic is the one shown and sent; kiewerPanic
// keeps its message and kiewer-panic tells the page.
window.kiewerPanic = "";
const originalError = console.error.bind(console);
console.error = function () {
  const text = Array.prototype.map.call(arguments, String).join(" ");
  if (text.startsWith("panicked at") && !window.kiewerPanic) {
    window.kiewerPanic = text.split("\n\nStack:")[0];
    showError(window.kiewerPanic);
    sendBeacon("panic", window.kiewerPanic);
    window.dispatchEvent(new Event("kiewer-panic"));
  }
  originalError.apply(console, arguments);
};

// Shows `message` in the page's error box, whatever it held.
function showError(message) {
  const errorBox = document.getElementById("error");
  errorBox.textContent = String(message);
  errorBox.hidden = false;
}

// Shows `message` in the page's error box. After a panic the box keeps
// the panic: the trap that follows it reaches a catch as "unreachable".
export function fail(message) {
  if (!window.kiewerPanic) showError(message);
}

// The viewer's module, imported in `start`'s try rather than at the top
// of this file: a static import that fails (a 404, a network error)
// stops this whole module before any of it runs, and the page stays
// blank with no beacon. A failure is thrown on as `import: <message>`,
// so the error box and the failed beacon say what broke. The panic hook
// above is set before this runs, as before.
async function importViewer() {
  try {
    return await import("./kiewer_viewer.js");
  } catch (e) {
    throw new Error("import: " + (e && e.message ? e.message : e));
  }
}

// Device pixels for the pointer event `e` over `canvas`: its offset from
// the canvas's corner scaled by the canvas's backing width over its
// displayed width. That is devicePixelRatio until the viewer caps the
// canvas to the device's texture limit (Viewer.create, resize), and the
// capped canvas's own scale after. The pages' net blocks use it too.
export function devicePoint(canvas, e) {
  const r = canvas.getBoundingClientRect();
  const scale = r.width > 0 ? canvas.width / r.width : 1;
  return [(e.clientX - r.left) * scale, (e.clientY - r.top) * scale];
}

// The page owns nothing of the gestures: this hands each one to the wasm
// in device pixels, and the camera (f64) and the drawing are Rust. The
// bindings are KiCad's (docs/v1-scope.md, Rendering, Zoom).
//
// `start(canvas, load)` creates the Viewer on `canvas`, calls
// `load(viewer, schedule)` once it exists, and sends the beacon after;
// `schedule` asks for a frame. A load that throws shows its message in
// the error box. Every frame sends kiewer-frame on the window. The page
// asks for the canvas's displayed size in device pixels; the Viewer sets
// the canvas to it, capped to the device's limit.
export async function start(canvas, load) {
  const status = document.getElementById("status");
  const errorBox = document.getElementById("error");
  const zoomBox = document.getElementById("zoom");
  let viewer = null;
  let frame = 0;
  let gesturing = false;
  // Whether the ok beacon went, whether the canvas has been capped at
  // any point since create, and whether `capped` went: at most one per
  // page load, after `ok`.
  let okSent = false;
  let everCapped = false;
  let cappedSent = false;

  function point(e) { return devicePoint(canvas, e); }
  // The backing size the page asks for: the displayed size in device
  // pixels.
  function wanted() {
    const r = canvas.getBoundingClientRect();
    const dpr = window.devicePixelRatio || 1;
    return [Math.max(1, Math.round(r.width * dpr)), Math.max(1, Math.round(r.height * dpr))];
  }
  // Called after each create and resize: notes whether the viewer held
  // the canvas to the device's limit, and sends `capped` the first time,
  // once `ok` has gone. A cap that came and went before then still
  // counts.
  function noteCap() {
    try { if (viewer.capped()) everCapped = true; } catch (e) { return; }
    if (!okSent || !everCapped || cappedSent || window.kiewerPanic) return;
    cappedSent = true;
    sendBeacon("capped", "");
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
    if (!viewer) return;
    const [width, height] = wanted();
    viewer.resize(width, height);
    noteCap();
    schedule();
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
    const { default: init, Viewer } = await importViewer();
    await init();
    const [width, height] = wanted();
    viewer = await Viewer.create(canvas, width, height);
    backendName = viewer.backend();
    vendorName = viewer.adapter_vendor();
    noteCap();
    // The layout may have moved while the adapter was asked for.
    viewer.resize(...wanted());
    noteCap();
    const samples = viewer.samples();
    status.textContent = backendName + (samples > 1 ? ", " + samples + "x msaa" : "");
    // On WebGL2, hovering the status says why WebGPU was not used.
    status.title = viewer.fallback_reasons();
    await load(viewer, schedule);
    schedule();
  } catch (e) {
    fail(e && e.message ? e.message : e);
  }

  // A panic sent its own beacon when it happened.
  if (window.kiewerPanic) return;
  if (viewer) {
    vendorName = await webgpuVendor();
    // A panic while the vendor was asked for sent its own beacon.
    if (window.kiewerPanic) return;
    sendBeacon("ok", "");
    okSent = true;
    noteCap();
  } else {
    sendBeacon("failed", errorBox.textContent);
  }
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
