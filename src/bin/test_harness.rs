//! wasm32-only test-harness binary for `enkr/tests/app_sync.rs`'s
//! `CdpDriver` scenarios (`clicking_a_note_selects_it_cdp` et al): seeds the
//! exact same `EnkrState::with_notes(
//! NoteDatabase::demo())` fixture the native `NativeDriver` scenarios
//! construct directly (mirrors `src/main.rs`'s native `--demo` flag, and how
//! `enkr/tests/app_sync.rs`'s native tests already never go through
//! `main.rs`'s real entry point either — they build `EnkrState` directly
//! too).
//!
//! The real deployed build (`src/main.rs`'s wasm `fn main`) seeds from
//! IndexedDB via `EnkrState::new_wasm()` instead, which starts empty (or
//! whatever a previous session left behind) on a real profile — not the
//! fixed fixture a scenario asserts against — so `CdpDriver` needs its own
//! small entry point here rather than pointing at `enkr/www/`'s real one.
//!
//! Also accepts `?server=<ws url>&nick=<name>` to connect to a test relay on
//! startup — see `harness_sync_params` below.
//!
//! Not part of the shipped app; never meant to run natively (there'd be
//! nothing useful to compare it against — the native scenario already
//! constructs the same `EnkrState` directly, no binary needed).

#[cfg(target_arch = "wasm32")]
fn main() {
    console_error_panic_hook::set_once();
    let ui = mae::imui::IMUI::new_dom("mae-root");
    let mut state = enkr::app::EnkrState::with_notes(enkr::note::NoteDatabase::demo());
    state.set_repaint_waker(ui.repaint_waker());
    // `?demo=1` overlays the synthetic collaborator carets `--demo` uses
    // natively (`app::demo_remote_carets`), so a scenario can check where a
    // remote caret lands without needing a second live client whose caret
    // position it does not control.
    if harness_query_flag("demo") {
        state.set_demo_presence(true);
    }
    // `?server=<ws url>&nick=<name>` connects this client to a test relay on
    // startup. The shipped web build offers only the one hardcoded server and
    // no field to type another into (see `EnkrState::add_server`), so a
    // browser client driven by a scenario has no way through its own UI to
    // reach an in-process `enkr-syncd` — this is that way in, and it exists
    // only in this test binary.
    if let Some((server, nick)) = harness_sync_params() {
        // The recovery-phrase prompt is a modal on first connect. A scenario
        // driving this client is not testing onboarding and would just have
        // to dismiss it before it could reach anything — same pre-ack
        // `tests/app_sync.rs`'s native `App` does for the same reason.
        state.notes.meta_set(enkr::app::META_RECOVERY_ACKED, "1");
        // `server_list` is what the Sync settings page iterates, and only a
        // *listed* server gets the remote-space list, its Refresh button and
        // the rest — so a client connected to something the list has never
        // heard of shows a different server's row instead, with a "Use"
        // button, as if it were not connected at all.
        state.extra_servers.push(server.clone());
        state.active_server = server;
        state.nickname_input = nick;
        state.connect_sync();
    }
    ui.run_dom(move |ui| {
        enkr::app::render(ui, &mut state);
    });
}

/// Is `name=1` in the page's query string?
#[cfg(target_arch = "wasm32")]
fn harness_query_flag(name: &str) -> bool {
    web_sys::window()
        .and_then(|w| w.location().search().ok())
        .is_some_and(|search| {
            search
                .trim_start_matches('?')
                .split('&')
                .any(|pair| pair == format!("{name}=1"))
        })
}

/// `(server, nickname)` from the page's query string, if a `server` was
/// given. Hand-parsed rather than via `URLSearchParams` (one more `web-sys`
/// feature for two lookups), and the values are percent-decoded only for
/// `%3A`/`%2F` — a `ws://host:port/ws` URL needs nothing else, and a
/// scenario passing something exotic would rather see it fail loudly here
/// than be silently mangled.
#[cfg(target_arch = "wasm32")]
fn harness_sync_params() -> Option<(String, String)> {
    let search = web_sys::window()?.location().search().ok()?;
    let mut server = None;
    let mut nick = String::new();
    for pair in search.trim_start_matches('?').split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let value = value.replace("%3A", ":").replace("%2F", "/");
        match key {
            "server" => server = Some(value),
            "nick" => nick = value,
            _ => {}
        }
    }
    Some((server?, nick))
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    panic!("test_harness is a wasm32-only test binary — see this file's module doc comment");
}
