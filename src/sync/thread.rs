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

use enkr_proto::crypto::Identity;

use super::engine::Engine;
use super::{Cmd, SyncConfig, SyncEvent};

/// Ceiling on how long [`EngineHandle::join`] waits for the engine to wind
/// down. The wind-down itself is quick — flush the debounce buffers, close the
/// socket (bounded separately, and tighter, inside the engine) — but
/// `Cmd::Shutdown` is only *seen* between awaits, and the engine may be part
/// way through a connection attempt when it arrives: the connect and each
/// handshake read are 5s apiece. Nobody should wait that out to quit an app,
/// and there is nothing to close politely on a connection that never came up,
/// so past this the engine is left to the process exit.
#[cfg(not(target_arch = "wasm32"))]
const JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[cfg(not(target_arch = "wasm32"))]
pub(super) struct EngineHandle {
    /// Sent (or, if the thread died some other way, disconnected) once
    /// `Engine::run` has returned. A plain `JoinHandle` can't be waited on
    /// with a deadline, which is the whole requirement here.
    done: std::sync::mpsc::Receiver<()>,
}

#[cfg(not(target_arch = "wasm32"))]
impl EngineHandle {
    /// Blocks until the engine has finished shutting down — it already saw
    /// `Cmd::Shutdown` by the time this is called (see
    /// `SyncClient::shutdown_blocking`) — or [`JOIN_TIMEOUT`] passes.
    pub(super) fn join(self) {
        if self.done.recv_timeout(JOIN_TIMEOUT).is_err() {
            log::warn!("sync engine did not stop within {JOIN_TIMEOUT:?}");
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn spawn_engine(
    config: SyncConfig,
    identity: Identity,
    cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    events: broadcast::Sender<SyncEvent>,
) -> Result<EngineHandle, String> {
    let (done_tx, done) = std::sync::mpsc::channel();
    // Detached on purpose: `join` waits on `done_tx` instead, so it can give
    // up on an engine that is wedged rather than hanging the app's exit.
    std::thread::Builder::new()
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
            // Err = nobody is waiting (no explicit shutdown); normal.
            let _ = done_tx.send(());
        })
        .map_err(|e| e.to_string())?;
    Ok(EngineHandle { done })
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
    identity: Identity,
    cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    events: broadcast::Sender<SyncEvent>,
) -> Result<EngineHandle, String> {
    wasm_bindgen_futures::spawn_local(Engine::new(config, identity, events).run(cmd_rx));
    Ok(EngineHandle)
}
