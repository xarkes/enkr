//! Native transport: a thin `Sink`/`Stream` wrapper around tokio-tungstenite,
//! narrowed to plain binary frame payloads — see the parent module's doc
//! comment for why.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::{Sink, Stream};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_with_config};

use enkr_proto::wire;

use super::WsError;

pub(crate) struct Ws(WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>);

impl Stream for Ws {
    type Item = Result<Vec<u8>, WsError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            return match Pin::new(&mut self.0).poll_next(cx) {
                Poll::Ready(Some(Ok(Message::Binary(bytes)))) => {
                    Poll::Ready(Some(Ok(bytes.to_vec())))
                }
                // Non-binary control/text frame: not part of this protocol, ignore.
                Poll::Ready(Some(Ok(_))) => continue,
                Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e.to_string()))),
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            };
        }
    }
}

impl Sink<Vec<u8>> for Ws {
    type Error = WsError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.0)
            .poll_ready(cx)
            .map_err(|e| e.to_string())
    }

    fn start_send(mut self: Pin<&mut Self>, item: Vec<u8>) -> Result<(), Self::Error> {
        Pin::new(&mut self.0)
            .start_send(Message::Binary(item.into()))
            .map_err(|e| e.to_string())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.0)
            .poll_flush(cx)
            .map_err(|e| e.to_string())
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.0)
            .poll_close(cx)
            .map_err(|e| e.to_string())
    }
}

/// Frame/message ceilings must clear the largest legal blob upload, or
/// tungstenite would refuse to send a `PutBlob` near `MAX_BLOB_BYTES` before
/// it ever reaches the wire.
pub(crate) async fn connect(url: &str) -> Result<Ws, WsError> {
    let ws_config = WebSocketConfig::default()
        .max_message_size(Some(wire::MAX_MESSAGE_BYTES))
        .max_frame_size(Some(wire::MAX_MESSAGE_BYTES));
    let connect = tokio::time::timeout(
        Duration::from_secs(5),
        connect_async_with_config(url, Some(ws_config), false),
    );
    let (ws, _) = connect
        .await
        .map_err(|_| "connect timeout".to_string())?
        .map_err(|e| e.to_string())?;
    Ok(Ws(ws))
}
