//! wasm32 transport: a `web_sys::WebSocket`-backed `Sink`/`Stream`, bridging
//! its callback-based JS API (onopen/onmessage/onerror/onclose) into the same
//! shape the native transport presents — see the parent module's doc comment.

use std::cell::RefCell;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use futures_util::{Sink, Stream};
use tokio::sync::{mpsc, oneshot};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{BinaryType, CloseEvent, ErrorEvent, MessageEvent, WebSocket};

use super::WsError;

pub(crate) struct Ws {
    socket: WebSocket,
    incoming: mpsc::UnboundedReceiver<Result<Vec<u8>, WsError>>,
    // Kept alive for the socket's lifetime; dropping these would detach the
    // listeners (same pattern `os/wasm.rs` uses for its DOM listeners).
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_error: Closure<dyn FnMut(ErrorEvent)>,
    _on_close: Closure<dyn FnMut(CloseEvent)>,
}

impl Stream for Ws {
    type Item = Result<Vec<u8>, WsError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.incoming.poll_recv(cx)
    }
}

impl Sink<Vec<u8>> for Ws {
    type Error = WsError;

    // The browser buffers `WebSocket.send` internally (backed by
    // `bufferedAmount`, not surfaced as backpressure here) — nothing to wait
    // on before a send.
    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: Vec<u8>) -> Result<(), Self::Error> {
        self.socket
            .send_with_u8_array(&item)
            .map_err(|e| format!("{e:?}"))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let _ = self.socket.close();
        Poll::Ready(Ok(()))
    }
}

/// Opens a `WebSocket` and waits for it to actually connect (or fail) before
/// returning — mirrors the native transport's `connect()` not returning until
/// the TCP+TLS handshake completes. There's no frame-size config knob to set
/// here (unlike native's `WebSocketConfig`): the browser enforces its own
/// limits, and `wire::MAX_MESSAGE_BYTES` is already enforced at the protocol
/// level regardless of transport.
pub(crate) async fn connect(url: &str) -> Result<Ws, WsError> {
    let socket = WebSocket::new(url).map_err(|e| format!("{e:?}"))?;
    socket.set_binary_type(BinaryType::Arraybuffer);

    let (tx, rx) = mpsc::unbounded_channel::<Result<Vec<u8>, WsError>>();

    let on_message = {
        let tx = tx.clone();
        Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
            if let Ok(buf) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
                let bytes = js_sys::Uint8Array::new(&buf).to_vec();
                let _ = tx.send(Ok(bytes));
            }
            // A non-ArrayBuffer payload (e.g. a stray text frame) is dropped,
            // matching the native transport silently skipping non-binary frames.
        })
    };
    socket
        .add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref())
        .map_err(|e| format!("{e:?}"))?;

    let on_error = {
        let tx = tx.clone();
        Closure::<dyn FnMut(ErrorEvent)>::new(move |_e: ErrorEvent| {
            let _ = tx.send(Err("websocket error".to_string()));
        })
    };
    socket
        .add_event_listener_with_callback("error", on_error.as_ref().unchecked_ref())
        .map_err(|e| format!("{e:?}"))?;

    let on_close = {
        let tx = tx.clone();
        Closure::<dyn FnMut(CloseEvent)>::new(move |_e: CloseEvent| {
            let _ = tx.send(Err("websocket closed".to_string()));
        })
    };
    socket
        .add_event_listener_with_callback("close", on_close.as_ref().unchecked_ref())
        .map_err(|e| format!("{e:?}"))?;

    // Resolved by whichever of open/error/close fires first while connecting
    // — the `Option::take` guard means only the first one wins.
    let (open_tx, open_rx) = oneshot::channel::<Result<(), WsError>>();
    let open_tx = Rc::new(RefCell::new(Some(open_tx)));

    let on_open = {
        let open_tx = open_tx.clone();
        Closure::<dyn FnMut(web_sys::Event)>::new(move |_e: web_sys::Event| {
            if let Some(tx) = open_tx.borrow_mut().take() {
                let _ = tx.send(Ok(()));
            }
        })
    };
    socket
        .add_event_listener_with_callback("open", on_open.as_ref().unchecked_ref())
        .map_err(|e| format!("{e:?}"))?;

    let on_open_error = {
        let open_tx = open_tx.clone();
        Closure::<dyn FnMut(web_sys::Event)>::new(move |_e: web_sys::Event| {
            if let Some(tx) = open_tx.borrow_mut().take() {
                let _ = tx.send(Err("connect failed".to_string()));
            }
        })
    };
    socket
        .add_event_listener_with_callback("error", on_open_error.as_ref().unchecked_ref())
        .map_err(|e| format!("{e:?}"))?;
    socket
        .add_event_listener_with_callback("close", on_open_error.as_ref().unchecked_ref())
        .map_err(|e| format!("{e:?}"))?;

    let opened = open_rx.await.map_err(|_| "connect cancelled".to_string())?;

    // Only needed to resolve the connect future above; the ongoing
    // `on_message`/`on_error`/`on_close` listeners (already attached) stay
    // alive for the connection's lifetime via `Ws`'s fields instead.
    drop(on_open);
    drop(on_open_error);

    opened?;
    Ok(Ws {
        socket,
        incoming: rx,
        _on_message: on_message,
        _on_error: on_error,
        _on_close: on_close,
    })
}
