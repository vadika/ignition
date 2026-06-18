mod model;
mod config;
mod vm;
mod api;

use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use crate::vm::{Settings, VmState};

fn arg(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().collect();
    let api_sock = match arg(&args, "--api-sock") {
        Some(s) => PathBuf::from(s),
        None => { eprintln!("--api-sock <path> is required"); std::process::exit(2); }
    };
    let store = PathBuf::from(arg(&args, "--store").unwrap_or_else(|| "./fc-store".into()));
    let boot_bin = PathBuf::from(arg(&args, "--boot").unwrap_or_else(|| "target/debug/boot".into()));
    let kernel_default = PathBuf::from(arg(&args, "--kernel").unwrap_or_else(|| "kimage/out/Image".into()));
    let control_sock = store.join("control.sock");
    std::fs::create_dir_all(&store).ok();

    let shared = Arc::new(Mutex::new(VmState::new(Settings {
        boot_bin, store, control_sock, kernel_default,
    })));

    let _ = std::fs::remove_file(&api_sock);
    let listener = tokio::net::UnixListener::bind(&api_sock)
        .unwrap_or_else(|e| { eprintln!("bind {api_sock:?}: {e}"); std::process::exit(1); });
    log::info!("ignition-fc-api listening on {api_sock:?}");

    {
        let shared = shared.clone();
        let api_sock = api_sock.clone();
        tokio::spawn(async move {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
            let mut intr = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).unwrap();
            tokio::select! { _ = term.recv() => {}, _ = intr.recv() => {} }
            let mut vm = shared.lock().await;
            if let Some(mut c) = vm.child.take() { let _ = c.kill(); }
            let _ = std::fs::remove_file(&vm.settings.control_sock);
            let _ = std::fs::remove_file(&api_sock);
            std::process::exit(0);
        });
    }

    loop {
        let (stream, _) = match listener.accept().await { Ok(p) => p, Err(_) => continue };
        let shared = shared.clone();
        tokio::task::spawn(async move {
            let io = TokioIo::new(stream);
            let svc = service_fn(move |req| api::route(shared.clone(), req));
            let _ = http1::Builder::new().serve_connection(io, svc).await;
        });
    }
}
