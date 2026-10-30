# Cloud Run, Cloud SQL Postgres and Fastly with identical cacheable pages

The server is axum on Cloud Run in europe-west4, with Cloud SQL Postgres from the start, chosen over SQLite because of how SQLite handles foreign keys and because several Cloud Run instances write at once. Fastly fronts one hostname with two origins: `/blob/*` goes to a private GCS bucket with HMAC-signed origin requests, everything else to Cloud Run, so readers never hit the server for bytes and nobody can hotlink the bucket around Fastly. Staging is the same Fastly service routed by hostname to its own Cloud Run service, its own database on the shared instance, and its own bucket.

Every HTML page renders identically for everyone and is cacheable; a small script fetches `/api/me`, never cached, to fill the account menu and edit controls, and writes purge by `Surrogate-Key`. Pages that are personal (settings, upload) are `private, no-store`. This is a constraint on every handler: nothing in a cacheable page may depend on the session. Static assets carry a content hash in the filename and are pre-compressed with brotli and gzip at build time.

## Considered options

- Static bucket plus CDN, no server: no uploads, no sign-in.
- SQLite with Litestream and one instance: single writer, foreign keys off by default per connection.
- Pages marked private with the session rendered server-side: every anonymous read and every Discord unfurl would reach Cloud Run.
