//! Platform-swappable timing primitives for the sync engine — alongside
//! `transport.rs`'s "network layer" and `thread.rs`'s "thread layer".
//!
//! `tokio::time` (`interval`/`timeout`/`sleep`) calls `std::time::Instant::
//! now()` **internally**, inside the tokio crate itself (see
//! `tokio-*/src/time/clock.rs`) — `web_time` (used elsewhere in this port)
//! only covers direct calls in `enkr`'s own code, not tokio's, so it doesn't
//! help here. That internal call unconditionally panics on wasm32-unknown-
//! unknown ("time not implemented on this platform"), so `tokio::time`'s
//! interval/timeout are unusable there regardless of which `tokio` features
//! are enabled. `gloo-timers`' `setTimeout`-backed sleep future is the
//! wasm32 replacement for the two call sites `sync/engine.rs` has.

use std::future::Future;
use std::time::Duration;

/// A repeating tick, `Duration` apart. Native wraps the real
/// `tokio::time::Interval` (unchanged scheduling behavior); wasm32 just
/// sleeps the full period every tick (equivalent to `tokio::time`'s
/// `MissedTickBehavior::Delay`, which is what `Engine::run` already
/// configures natively — never trying to "catch up" missed ticks).
pub(crate) struct Interval {
    #[cfg(not(target_arch = "wasm32"))]
    inner: tokio::time::Interval,
    #[cfg(target_arch = "wasm32")]
    period: Duration,
}

impl Interval {
    pub(crate) fn new(period: Duration) -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let mut inner = tokio::time::interval(period);
            inner.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            Self { inner }
        }
        #[cfg(target_arch = "wasm32")]
        {
            Self { period }
        }
    }

    pub(crate) async fn tick(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.inner.tick().await;
        }
        #[cfg(target_arch = "wasm32")]
        {
            gloo_timers::future::sleep(self.period).await;
        }
    }
}

/// Races `fut` against a `duration` sleep, returning `Err(())` if the sleep
/// wins. Native wraps the real `tokio::time::timeout`; wasm32 races the two
/// futures manually via `tokio::select!` (the macro itself doesn't depend on
/// tokio's own timer — only the sleep future driving one of its arms does,
/// and `gloo_timers` supplies that here).
pub(crate) async fn timeout<F: Future>(duration: Duration, fut: F) -> Result<F::Output, ()> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        tokio::time::timeout(duration, fut).await.map_err(|_| ())
    }
    #[cfg(target_arch = "wasm32")]
    {
        tokio::select! {
            result = fut => Ok(result),
            _ = gloo_timers::future::sleep(duration) => Err(()),
        }
    }
}
