//! A loopback TCP proxy that sits between client and relay, so tests can add a
//! realistic round trip and inject network faults.
//!
//! Localhost has effectively zero RTT and never fails, which flatters anything
//! round-trip bound and hides how the client copes when a link degrades rather
//! than closes cleanly. Real clients talk to a remote relay at roughly
//! 30–80 ms RTT, over links that stall, black-hole and drop.
//!
//! Delay is applied per chunk but **not** serialised: a reader task stamps each
//! chunk with its release time and hands it to a writer task, so ordering is
//! preserved while transfers still pipeline. Sleeping inside the read loop
//! instead would model a stop-and-wait link and make throughput a function of
//! chunk count, which is not what a real network does.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc};
use tokio::time::Instant;

/// Shared switches every live connection watches.
#[derive(Default)]
struct Faults {
    /// Bytes are still accepted but never delivered — a half-open link. The
    /// interesting case: nothing errors, so a peer with no liveness check has
    /// no way to notice.
    stalled: AtomicBool,
    /// New connections are refused, modelling a partition or a down relay.
    refusing: AtomicBool,
}

pub struct NetProxy {
    addr: SocketAddr,
    faults: Arc<Faults>,
    /// Abort handles for live connections, so `cut` can drop them all.
    conns: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    task: tokio::task::JoinHandle<()>,
}

impl NetProxy {
    /// Listen on an ephemeral port, forwarding to `target` with `one_way`
    /// delay in each direction (so RTT is `2 * one_way`).
    pub async fn start(target: SocketAddr, one_way: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
        let addr = listener.local_addr().expect("proxy addr");
        let faults = Arc::new(Faults::default());
        let conns: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> = Arc::default();

        let accept_faults = faults.clone();
        let accept_conns = conns.clone();
        let task = tokio::spawn(async move {
            while let Ok((inbound, _)) = listener.accept().await {
                if accept_faults.refusing.load(Ordering::Relaxed) {
                    drop(inbound); // connection refused mid-partition
                    continue;
                }
                let faults = accept_faults.clone();
                let handle = tokio::spawn(async move {
                    let Ok(outbound) = TcpStream::connect(target).await else {
                        return;
                    };
                    let _ = inbound.set_nodelay(true);
                    let _ = outbound.set_nodelay(true);
                    let (cr, cw) = inbound.into_split();
                    let (sr, sw) = outbound.into_split();
                    // Joined, not spawned: aborting a task does **not** abort
                    // tasks it spawned, so spawning the pumps here would leave
                    // them forwarding after `cut` — the connection would look
                    // severed to the test while traffic kept flowing.
                    tokio::select! {
                        _ = pump(cr, sw, one_way, faults.clone()) => {}
                        _ = pump(sr, cw, one_way, faults) => {}
                    }
                });
                accept_conns.lock().await.push(handle);
            }
        });
        Self {
            addr,
            faults,
            conns,
            task,
        }
    }

    pub fn url(&self) -> String {
        format!("ws://{}/ws", self.addr)
    }

    /// Stop delivering bytes in both directions without closing anything.
    pub fn stall(&self) {
        self.faults.stalled.store(true, Ordering::Relaxed);
    }

    pub fn resume(&self) {
        self.faults.stalled.store(false, Ordering::Relaxed);
    }

    /// Drop every live connection, as a relay restart or a middlebox would.
    pub async fn cut(&self) {
        for handle in self.conns.lock().await.drain(..) {
            handle.abort();
        }
    }

    /// Refuse new connections (partition). `heal` restores them.
    pub fn partition(&self) {
        self.faults.refusing.store(true, Ordering::Relaxed);
    }

    pub fn heal(&self) {
        self.faults.refusing.store(false, Ordering::Relaxed);
    }
}

impl Drop for NetProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn pump(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    delay: Duration,
    faults: Arc<Faults>,
) {
    let (tx, mut rx) = mpsc::channel::<(Instant, Vec<u8>)>(1024);
    let writer_faults = faults.clone();
    let writer = tokio::spawn(async move {
        while let Some((due, bytes)) = rx.recv().await {
            tokio::time::sleep_until(due).await;
            // Hold everything back while stalled; the socket stays open, so
            // neither end sees an error.
            while writer_faults.stalled.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            if to.write_all(&bytes).await.is_err() {
                return;
            }
        }
        let _ = to.shutdown().await;
    });
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match from.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if tx
                    .send((Instant::now() + delay, buf[..n].to_vec()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    }
    drop(tx);
    let _ = writer.await;
}
