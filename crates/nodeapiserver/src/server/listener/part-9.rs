
/// Buffers a request's entire body into memory — fine for the object
/// sizes this build's own resources actually reach (real kube-apiserver
/// itself has no streaming write path either; every write is a single
/// decoded object). No size cap yet — a named, real gap, not a
/// forgotten one: real upstream enforces `--max-request-body-bytes`.
async fn read_body_bytes(req: Request<Incoming>) -> Result<Vec<u8>, hyper::Error> {
    include!("body-13-1.rs");
    include!("body-13-2.rs");
}
