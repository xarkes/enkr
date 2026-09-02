//! Platform-swappable WebSocket transport for the sync engine — the *only*
//! part of `sync/engine.rs` that differs by platform (per-platform "network
//! layer", alongside `thread.rs`'s "thread layer"). `Ws` implements
//! `Sink<Vec<u8>, Error = WsError> + Stream<Item = Result<Vec<u8>, WsError>>`
//! either way, so `engine.rs`'s `ws.split()`/`sink.send(...)`/`stream.next()`
//! calls are unchanged regardless of which one is compiled in.
//!
//! The engine only ever sends/receives whole binary frames (postcard-encoded
//! `ClientMsg`/`ServerMsg`) — never Text/Ping/Pong/Close — so the
//! abstraction is just "a stream of frame payloads", not tungstenite's full
//! `Message` enum; each platform's `Stream` impl silently drops non-binary
//! frames itself instead of `engine.rs` pattern-matching them out. Likewise
//! the concrete error is never inspected by the engine (only matched as
//! `Ok`/`Err`), so it's unified to a plain message string.

pub(crate) type WsError = String;

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use native::{Ws, connect};

#[cfg(target_arch = "wasm32")]
mod wasm;
#[cfg(target_arch = "wasm32")]
pub(crate) use wasm::{Ws, connect};
