//! Platform-swappable "run the sync engine off the main thread" layer —
//! alongside `transport.rs`'s "network layer". `spawn_engine` starts
//! `Engine::run` executing and returns immediately; the caller
//! (`SyncClient::spawn`) doesn't wait on it for anything — the engine
//! reports everything else (readiness, events) back through the
//! `cmd_tx`/`events` channels it always has, same as before this module
//! existed.
//!
//! Native: a real OS thread running a dedicated single-threaded tokio
//! runtime (unchanged from before this module existed).
//!
//! wasm32: no threads exist at all. `Engine::run`'s future is instead driven
//! cooperatively via `wasm_bindgen_futures::spawn_local` on the *main*
//! thread — correctly non-blocking (it yields to the browser's event loop
//! between polls), unlike naively trying `Runtime::block_on`, which tokio's
//! own wasm support explicitly panics on rather than pretending to block
//! (wasm has no OS-level "park this thread until woken" primitive to fake
//! blocking with). This still runs on the main thread, not a real Worker:
//! the sync protocol's own per-message work (postcard encode/decode, a
//! handful of small crypto ops) is lightweight enough that this is fine in
//! practice. Moving it into a real Worker (a separate wasm instance talking
//! over `postMessage`) is a natural later upgrade, isolated entirely to this
//! module — `engine.rs` wouldn't need to change at all.

use tokio::sync::{broadcast, mpsc};

use enkr_proto::crypto::DeviceIdentity;

use super::engine::Engine;
use super::{Cmd, SyncConfig, SyncEvent};

#[cfg(not(target_arch = "wasm32"))]
pub(super) struct EngineHandle {
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(not(target_arch = "wasm32"))]
impl EngineHandle {
    /// Blocks until the engine thread exits (it already saw `Cmd::Shutdown`
    /// by the time this is called — see `SyncClient::shutdown`).
    pub(super) fn join(mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn spawn_engine(
    config: SyncConfig,
    identity: DeviceIdentity,
    cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    events: broadcast::Sender<SyncEvent>,
) -> Result<EngineHandle, String> {
    let thread = std::thread::Builder::new()
        .name("enkr-sync".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    log::error!("sync runtime: {err}");
                    return;
                }
            };
            runtime.block_on(Engine::new(config, identity, events).run(cmd_rx));
        })
        .map_err(|e| e.to_string())?;
    Ok(EngineHandle {
        thread: Some(thread),
    })
}

/// Nothing to join — `spawn_local`'d work has no handle of its own; the
/// engine stops itself once its `run()` future observes `Cmd::Shutdown`
/// (already sent by the time `SyncClient::shutdown`/`Drop` calls this) on a
/// later microtask tick, not synchronously.
#[cfg(target_arch = "wasm32")]
pub(super) struct EngineHandle;

#[cfg(target_arch = "wasm32")]
impl EngineHandle {
    pub(super) fn join(self) {}
}

#[cfg(target_arch = "wasm32")]
pub(super) fn spawn_engine(
    config: SyncConfig,
    identity: DeviceIdentity,
    cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    events: broadcast::Sender<SyncEvent>,
) -> Result<EngineHandle, String> {
    wasm_bindgen_futures::spawn_local(Engine::new(config, identity, events).run(cmd_rx));
    Ok(EngineHandle)
}
