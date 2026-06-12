fn main() {
    match ignition_vmnet::VmnetBackend::start() {
        Ok((b, _rx)) => {
            use devices::virtio::net::NetBackend;
            let m = b.mac();
            println!("vmnet up: mac {m:02x?}");
        }
        Err(e) => { eprintln!("vmnet start failed: {e}"); std::process::exit(1); }
    }
}
