# Thumbnails rendered on the server with wgpu on lavapipe

Discord unfurls `og:image`, so every revision needs a PNG. The uploader's browser already renders the board and could post a capture, but then any image a client chose would appear on a kiewer page. Thumbnails are rendered by the server instead, with the same wgpu pipeline as the browser on Mesa's software Vulkan (lavapipe) inside the server image, so what Discord shows is what the viewer shows and nothing user-chosen is ever an image. The image grows by roughly 100 MB.

Rendering happens inside the upload request within a time budget; past the budget the revision still saves, a Cloud Tasks retry produces the thumbnail, and the page shows a placeholder until then. View 5 (front copper, paste, mask, silkscreen) fit to the board, at 1200×630 for `og:image` and 400×400 for lists; a design with only a schematic uses its first sheet.

## Considered options

- Client-captured PNG at upload: no server GPU story, but an open image slot.
- A CPU rasteriser (tiny-skia) over the same triangles: smaller image, a second renderer that drifts from the browser.
