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

/// Plain WebSockets are acceptable only for local development. A remote
/// `ws://` endpoint would expose the bearer account token and make the auth
/// challenge vulnerable to a network attacker.
pub(crate) fn validate_server_url(raw: &str) -> Result<(), WsError> {
    let url = url::Url::parse(raw).map_err(|err| format!("invalid sync server URL: {err}"))?;
    match url.scheme() {
        "wss" => Ok(()),
        "ws" => {
            let host = url
                .host_str()
                .ok_or_else(|| "sync server URL has no host".to_string())?;
            let loopback = host == "localhost"
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback());
            if loopback {
                Ok(())
            } else {
                Err("unencrypted ws:// is allowed only for loopback sync servers".into())
            }
        }
        scheme => Err(format!("unsupported sync server URL scheme: {scheme}")),
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use native::{Ws, connect};

#[cfg(target_arch = "wasm32")]
mod wasm;
#[cfg(target_arch = "wasm32")]
pub(crate) use wasm::{Ws, connect};

#[cfg(test)]
mod tests {
    use super::validate_server_url;

    #[test]
    fn plaintext_is_allowed_only_for_loopback() {
        assert!(validate_server_url("ws://127.0.0.1:9070/ws").is_ok());
        assert!(validate_server_url("ws://[::1]:9070/ws").is_ok());
        assert!(validate_server_url("ws://localhost:9070/ws").is_ok());
        assert!(validate_server_url("ws://sync.example/ws").is_err());
    }

    #[test]
    fn secure_and_invalid_schemes_are_handled_explicitly() {
        assert!(validate_server_url("wss://sync.example/ws").is_ok());
        assert!(validate_server_url("https://sync.example/ws").is_err());
        assert!(validate_server_url("not a url").is_err());
    }
}
