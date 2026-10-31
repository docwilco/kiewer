# vcl_fetch: a blob is content-addressed, so a found one is immutable and
# cached for a year; a missing one is cached briefly, since the server may
# be writing it. GCS answers a private object with Cache-Control: private,
# which Fastly would refuse to cache, so the header is replaced before
# Fastly reads it. Cloud Run responses are left as the server sent them.
if (req.http.X-Kiewer-Origin == "gcs") {
  unset beresp.http.Set-Cookie;
  if (beresp.status == 200) {
    set beresp.ttl = 365d;
    set beresp.http.Cache-Control = "public, max-age=31536000, immutable";
  } else {
    # 404 for a blob not yet written, 403 for a signing problem: neither is
    # worth caching for long, and a 403 cached as a 404 would hide the
    # cause.
    set beresp.ttl = 10s;
    set beresp.http.Cache-Control = "public, max-age=10";
  }
}
