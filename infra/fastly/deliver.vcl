# vcl_deliver: a blob response carries nothing that names the bucket or
# GCS. Fastly strips Surrogate-Key and Surrogate-Control on its own.
if (req.http.X-Kiewer-Origin == "gcs") {
  unset resp.http.x-goog-generation;
  unset resp.http.x-goog-metageneration;
  unset resp.http.x-goog-hash;
  unset resp.http.x-goog-storage-class;
  unset resp.http.x-goog-stored-content-encoding;
  unset resp.http.x-goog-stored-content-length;
  unset resp.http.x-guploader-uploadid;
  unset resp.http.Alt-Svc;
  unset resp.http.Expires;
}
