//! Guest networking backend: `SocketVmnetBackend` talks to the socket_vmnet daemon
//! (no sudo; the daemon holds the privileged vmnet interface).

mod socket_vmnet;
pub use socket_vmnet::{generate_mac, SocketVmnetBackend};
