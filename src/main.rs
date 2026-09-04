use enkr::app::{EnkrState, render};
#[cfg(all(debug_assertions, not(target_arch = "wasm32")))]
use enkr::note::NoteDatabase;
use mae::imui::IMUI;
use mae::imui::MarkdownMode;

/// The application icon, embedded directly into the binary. Native only —
/// `IMUI::set_app_icon` is a documented no-op on wasm32 (favicon control is
/// out of scope there for now; see `os/wasm.rs`), so there's nothing to wire
/// up on that target.
// TODO: Choose an icon and set it back
// #[cfg(not(target_arch = "wasm32"))]
// const APP_ICON: &[u8] = include_bytes!("../assets/icon.png");

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    println!(
        "Starting Enkr {} ({})",
        env!("CARGO_PKG_VERSION"),
        env!("ENKR_GIT_HASH")
    );

    #[cfg(feature = "png_capture")]
    let capture_path = parse_capture_arg();
    let mut ui = IMUI::new(960, 640, "Enkr");
    let mut state = make_state();
    #[cfg(feature = "png_capture")]
    let mut capture_done = false;
    // Background sync events must wake the (otherwise idle) event loop.
    state.set_repaint_waker(ui.repaint_waker());
    // Embed the application icon in the binary and apply it (Dock / app switcher).
    // TODO: Reinstall the icon
    // ui.set_app_icon(APP_ICON);

    // Source by default: the markers are part of what you are editing, and
    // hiding them means every edit to emphasis or a heading is made blind.
    // The toolbar toggle switches to the rendered view for reading.
    ui.set_markdown_mode(MarkdownMode::Source);

    ui.eventloop(|ui| {
        render(ui, &mut state);
        #[cfg(feature = "png_capture")]
        if !capture_done {
            if let Some(path) = &capture_path {
                capture_done = true;
                ui.request_capture(path.clone());
                ui.request_quit();
            }
        }
    });
    state.shutdown();
}

/// wasm32 entry point. `EnkrState::new_wasm` is async (IndexedDB has no
/// synchronous read path — see its doc comment), so this kicks off the
/// whole startup sequence as one `spawn_local`'d task rather than blocking
/// `fn main` itself, which can't be async on a wasm32 binary target at all.
///
/// Matches native: the source view is the default, so the markers stay visible
/// while editing. The DOM backend hosts a `RICH_TEXT_HOST` `contenteditable`
/// div for the rendered mode (see `paint_dom.rs`) instead of native's own
/// from-scratch glyph rendering.
#[cfg(target_arch = "wasm32")]
fn main() {
    console_error_panic_hook::set_once();
    wasm_bindgen_futures::spawn_local(async {
        let mut ui = IMUI::new_dom("mae-root");
        let mut state = EnkrState::new_wasm().await;
        state.set_repaint_waker(ui.repaint_waker());
        ui.set_markdown_mode(MarkdownMode::Source);
        ui.run_dom(move |ui| {
            render(ui, &mut state);
        });
    });
}

/// Build the initial app state. In debug builds `--demo` seeds an in-memory
/// sample database and overlays fake collaborator carets for design previews;
/// release builds never compile this path and always use the real note store.
#[cfg(not(target_arch = "wasm32"))]
fn make_state() -> EnkrState {
    #[cfg(debug_assertions)]
    if std::env::args().any(|a| a == "--demo") {
        let mut state = EnkrState::with_notes(NoteDatabase::demo());
        state.set_demo_presence(true);
        return state;
    }
    EnkrState::new()
}

#[cfg(feature = "png_capture")]
fn parse_capture_arg() -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    let pos = args.iter().position(|a| a == "--capture")?;
    args.get(pos + 1).cloned()
}
