//! Hyper request router: maps (method, path) to handlers, encodes FC status codes.
use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Method, Request, Response, StatusCode};
use tokio::sync::Mutex;
use crate::model::*;
use crate::vm::{sanitize_name, State, VmState};

pub type Shared = Arc<Mutex<VmState>>;

fn json<T: serde::Serialize>(status: StatusCode, body: &T) -> Response<Full<Bytes>> {
    let b = serde_json::to_vec(body).unwrap_or_default();
    Response::builder().status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(b))).unwrap()
}
fn empty(status: StatusCode) -> Response<Full<Bytes>> {
    Response::builder().status(status).body(Full::new(Bytes::new())).unwrap()
}
fn fault(msg: impl Into<String>) -> Response<Full<Bytes>> {
    json(StatusCode::BAD_REQUEST, &Fault { fault_message: msg.into() })
}
fn parse<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, Response<Full<Bytes>>> {
    serde_json::from_slice(body).map_err(|_| fault("failed to parse body"))
}

/// Send one control line to the boot child's control socket, await the reply.
async fn control(sock: &std::path::Path, action: &str, name: Option<&str>) -> Result<(), String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut stream = tokio::net::UnixStream::connect(sock).await
        .map_err(|e| format!("control connect: {e}"))?;
    let line = match name {
        Some(n) => format!("{{\"action\":\"{action}\",\"name\":\"{n}\"}}\n"),
        None => format!("{{\"action\":\"{action}\"}}\n"),
    };
    stream.write_all(line.as_bytes()).await.map_err(|e| format!("control write: {e}"))?;
    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    reader.read_line(&mut resp).await.map_err(|e| format!("control read: {e}"))?;
    if resp.contains("\"ok\":true") { Ok(()) } else { Err(format!("control error: {}", resp.trim())) }
}

/// Poll-connect the control socket until it answers or the deadline passes.
async fn wait_ready(sock: &std::path::Path) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if tokio::net::UnixStream::connect(sock).await.is_ok() { return Ok(()); }
        if Instant::now() >= deadline { return Err("vm did not become ready".into()); }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Spawn `boot` with the given extra args, store the child, wait until ready.
async fn spawn_boot(vm: &mut VmState, extra: Vec<String>) -> Result<(), String> {
    let _ = std::fs::remove_file(&vm.settings.control_sock);
    let mut cmd = std::process::Command::new(&vm.settings.boot_bin);
    cmd.arg("--control-sock").arg(&vm.settings.control_sock)
        .arg("--store").arg(&vm.settings.store);
    for a in extra { cmd.arg(a); }
    cmd.stdin(std::process::Stdio::null());
    let child = cmd.spawn().map_err(|e| format!("spawn boot: {e}"))?;
    vm.child = Some(child);
    if let Err(e) = wait_ready(&vm.settings.control_sock).await {
        // boot never became ready: kill the child so a retried InstanceStart
        // does not orphan it (Child::drop does not kill).
        if let Some(mut c) = vm.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        return Err(e);
    }
    Ok(())
}

pub async fn route(shared: Shared, req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let body = match req.into_body().collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => return Ok(fault("failed to read body")),
    };
    Ok(handle(shared, method, path, &body).await)
}

async fn handle(shared: Shared, method: Method, path: String, body: &[u8]) -> Response<Full<Bytes>> {
    let mut vm = shared.lock().await;

    macro_rules! cfg_put { ($parse:ty, $set:expr) => {{
        if let Err(m) = vm.ensure_not_started() { return fault(m); }
        match parse::<$parse>(body) { Ok(v) => { $set(&mut vm, v); empty(StatusCode::NO_CONTENT) }, Err(r) => r }
    }}; }

    match (method, path.as_str()) {
        (Method::GET, "/") => json(StatusCode::OK, &InstanceInfo {
            id: "ignition".into(), state: vm.state.as_str().into(),
            vmm_version: env!("CARGO_PKG_VERSION").into(), app_name: "ignition-fc-api".into() }),

        (Method::PUT, "/machine-config") =>
            cfg_put!(MachineConfig, |vm: &mut VmState, v| vm.config.set_machine(v)),
        (Method::GET, "/machine-config") => json(StatusCode::OK, &serde_json::json!({
            "vcpu_count": vm.config.vcpu_count, "mem_size_mib": vm.config.mem_size_mib,
            "track_dirty_pages": vm.config.track_dirty_pages })),
        (Method::PUT, "/boot-source") =>
            cfg_put!(BootSource, |vm: &mut VmState, v| vm.config.set_boot_source(v)),
        (Method::PUT, p) if p.starts_with("/drives/") => {
            if let Err(m) = vm.ensure_not_started() { return fault(m); }
            match parse::<Drive>(body) {
                Ok(d) => match vm.config.set_drive(d) { Ok(()) => empty(StatusCode::NO_CONTENT), Err(m) => fault(m) },
                Err(r) => r,
            }
        }
        (Method::PUT, p) if p.starts_with("/network-interfaces/") =>
            cfg_put!(NetworkInterface, |vm: &mut VmState, v| vm.config.set_net(v)),

        (Method::PUT, "/actions") => {
            let a: Action = match parse(body) { Ok(a) => a, Err(r) => return r };
            match a.action_type.as_str() {
                "InstanceStart" => {
                    if vm.state != State::NotStarted { return fault("vm already started"); }
                    let flags = match vm.config.to_boot_flags() { Ok(f) => f, Err(m) => return fault(m) };
                    match spawn_boot(&mut vm, flags).await {
                        Ok(()) => { vm.state = State::Running; empty(StatusCode::NO_CONTENT) }
                        Err(m) => fault(m),
                    }
                }
                other => fault(format!("unsupported action_type: {other}")),
            }
        }

        (Method::PATCH, "/vm") => {
            let u: VmUpdate = match parse(body) { Ok(u) => u, Err(r) => return r };
            // Advisory pause: just record the requested state. The guest keeps
            // running; snapshot/create does its own atomic stop-the-world capture.
            match vm.vm_update(&u.state) {
                Ok(new_state) => { vm.state = new_state; empty(StatusCode::NO_CONTENT) }
                Err(m) => fault(m),
            }
        }

        (Method::PUT, "/snapshot/create") => {
            if let Err(m) = vm.ensure_paused_for_snapshot() { return fault(m); }
            let s: SnapshotCreate = match parse(body) { Ok(s) => s, Err(r) => return r };
            let name = sanitize_name(&s.snapshot_path);
            let sock = vm.settings.control_sock.clone();
            // Pause is advisory, so the guest is running; the snapshot control
            // command runs its own atomic stop-the-world rendezvous (synchronous:
            // boot replies only once the snapshot is written). VM stays Paused.
            match control(&sock, "snapshot", Some(&name)).await {
                Ok(()) => { vm.paths.insert(s.snapshot_path, name); empty(StatusCode::NO_CONTENT) }
                Err(m) => fault(m),
            }
        }

        (Method::PUT, "/snapshot/load") => {
            if vm.state != State::NotStarted { return fault("cannot load after boot"); }
            let l: SnapshotLoad = match parse(body) { Ok(l) => l, Err(r) => return r };
            let name = vm.paths.get(&l.snapshot_path).cloned()
                .unwrap_or_else(|| sanitize_name(&l.snapshot_path));
            // ponytail: a load-only client must still PUT boot-source + a root drive
            // first (firecracker-go-sdk does in its snapshot-resume flow). Fall back to
            // the --kernel default for the kernel; the rootfs positional comes from the
            // configured root drive (boot --restore still needs the positionals).
            let kernel = vm.config.kernel_image_path.clone()
                .unwrap_or_else(|| vm.settings.kernel_default.to_string_lossy().into_owned());
            let rootfs = vm.config.root_drive_path.clone();
            let mut flags = vec!["--restore".to_string(), name, kernel];
            if let Some(r) = rootfs { flags.push(r); }
            match spawn_boot(&mut vm, flags).await {
                Ok(()) => {
                    if !l.resume_vm {
                        let sock = vm.settings.control_sock.clone();
                        if let Err(m) = control(&sock, "pause", None).await { return fault(m); }
                        vm.state = State::Paused;
                    } else {
                        vm.state = State::Running;
                    }
                    empty(StatusCode::NO_CONTENT)
                }
                Err(m) => fault(m),
            }
        }

        (Method::GET, _) | (Method::PUT, _) | (Method::PATCH, _) => empty(StatusCode::NOT_FOUND),
        _ => empty(StatusCode::METHOD_NOT_ALLOWED),
    }
}
