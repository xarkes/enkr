//! `enkr-e2e --bin showcase`: a fully automated, screen-recordable
//! collaboration demo. Nothing here is faked — both clients are real
//! [`EnkrState`]s driven exclusively through their widgets, exactly like the
//! `app_sync.rs` integration tests, against a real in-process `enkr-syncd`.
//!
//! What plays out on screen:
//! - client1 opens in a visible window on a brand-new local database
//! - client1 imports a small markdown repo as a new Space (via the folder picker)
//! - client1 connects and pushes the Space to the server
//! - client1 shares the Space by pasting client2's identity key
//! - client2 (a headless app running in this same process, invisible) fetches
//!   the Space and starts typing into a note — its edits and presence caret
//!   appear live in client1's window
//! - client1 writes back; both keep editing the shared note
//!
//! The "mouse" in the recording is a cursor sprite drawn on a top-most overlay:
//! synthetic events never move the real OS pointer, so the script glides its
//! own cursor between targets and clicks with a ripple. Park the real cursor
//! outside the window while recording.
//!
//! This module is demo-only and deliberately trades per-frame efficiency
//! (snapshots, string matching) for script robustness; none of it is compiled
//! into normal builds.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mae::imui::{IMUI, Point, RepaintWaker, UIBoxFlags};
use mae::os::{OSEvent, OSEventFlag, OSKey, OSKeyCode};
use mae::render::RectCoords;
use mae::testkit::{UiHarness, UiSnapshot};
use mae::ui::Color;

use enkr::app::{
    DARK_THEME_ICON, EnkrState, RENDER_MARKDOWN_ICON, SEARCH_ICON, SETTINGS_ICON, render,
};
use enkr::note::NoteDatabase;
use enkr::sync::IdentityStore;

/// Folder name of the sample repo, and therefore the imported Space's name.
const REPO_NAME: &str = "horizon-docs";
/// Markers used by the wait conditions to detect the other user's lines.
const BOB_LINE_1: &str = "bob: Hey! The whole space just appeared on my side.";
const ALICE_LINE: &str =
    "alice: Imported from disk, synced end-to-end encrypted, and shared - all in one take.";
const BOB_LINE_2: &str = "bob: I can see your caret moving in real time. Neat!";

/// Budget for cross-client waits (invite sent, edits synced back): these span
/// the *other* client's entire on-screen journey, so they must cover the whole
/// recording. A failed script logs a state dump and goes inert.
const STEP_TIMEOUT: f32 = 180.0;
/// Timeout for direct UI interactions (clicks, keys): the target widget should
/// exist within a frame or two, so anything beyond a few seconds is a genuine
/// failure and should surface immediately.
const INTERACT_TIMEOUT: f32 = 5.0;
/// Sidebar hit-test area (matches the layout assumptions of `app_sync.rs`).
const SIDEBAR_RIGHT_EDGE: f32 = 280.0;
const TOP_BAR_BOTTOM: f32 = 56.0;

pub fn run() {
    // Self-contained working directory: the folder picker opens the
    // cwd-relative `enkr_import`, so chdir into a scratch dir that holds the
    // sample repo. Note stores are in-memory and identities ephemeral, so the
    // user's real data is never touched.
    let dir = std::env::temp_dir().join(format!("enkr_showcase_{}", uuid::Uuid::new_v4()));
    create_sample_repo(&dir.join("enkr_import").join(REPO_NAME));
    std::env::set_current_dir(&dir).expect("chdir into showcase scratch dir");

    let server = ShowcaseServer::start(&dir);
    let url = server.url();

    // client1: the visible window.
    let mut ui = IMUI::new(1100, 720, "Enkr");
    // TODO: Maybe add the icon again
    // ui.set_app_icon(include_bytes!("../assets/icon.png"));
    let mut state1 = EnkrState::with_notes(NoteDatabase::new_in_memory());
    state1.sync_identity = Some(IdentityStore::InMemory);
    state1.hide_default_server = true;
    state1.set_repaint_waker(ui.repaint_waker());

    // Cross-client coordination, shared with the headless peer's thread.
    let shared = Arc::new(Mutex::new(Shared::default()));
    // Set once the window closes, so the peer thread can tear down its app.
    let quit = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // client2 is a full app on a headless harness, but it runs on its *own*
    // thread instead of being pumped from the window's loop. That keeps
    // client1's frame the same lean render-only frame the real app runs, so
    // its animations (e.g. the light↔dark theme fade) stay perfectly smooth
    // instead of stuttering behind the peer's per-frame work. The peer wakes
    // the window whenever its sync produces a visible change.
    let peer = std::thread::spawn({
        let url = url.clone();
        let shared = Arc::clone(&shared);
        let quit = Arc::clone(&quit);
        let waker = ui.repaint_waker();
        move || run_peer(&url, shared, quit, waker)
    });

    let mut script1 = Script::client1(&url);
    let mut snap1 = UiSnapshot::default();
    let mut last = Instant::now();

    ui.eventloop(|ui| {
        let now = Instant::now();
        let dt = (now - last).as_secs_f32().min(0.1);
        last = now;

        let events = script1.tick(dt, &snap1, &state1, &mut shared.lock().unwrap());
        for ev in events {
            ui.inject_event(ev);
        }
        render(ui, &mut state1);
        // Only pay for a layout snapshot on the frames a step actually needs
        // one to locate its target; pauses, typing and waits render nothing
        // but the app itself, exactly like a normal frame.
        if script1.needs_snapshot() {
            snap1 = ui.snapshot_laid_out();
        }
        script1.draw_cursor(ui);

        // Keep frames coming while the script acts; afterwards the window
        // idles normally and the peer's sync events wake it.
        if !script1.finished() {
            ui.request_repaint();
        }
    });

    // Window closed: tear down client1, then let the peer finish and join.
    quit.store(true, Ordering::Release);
    state1.shutdown();
    let _ = peer.join();
    drop(server);
    // Leave the scratch dir out of temp-cleanup's way; it holds nothing but
    // the sample repo and the server db.
    let _ = std::env::set_current_dir(std::env::temp_dir());
    let _ = std::fs::remove_dir_all(&dir);
}

/// The headless peer (client2), run on its own thread. It drives a full app
/// through the same widgets as client1, connects, pulls the shared space and
/// co-edits the note. Because it lives off the window's loop, the visible
/// client renders unencumbered; the peer just wakes it (via `waker`) whenever
/// its sync produces something client1 should see. It paces itself at roughly
/// display rate — enough to animate its own caret and pump sync smoothly —
/// then, once its script is done, idles until `quit` while its background sync
/// engine keeps flushing its final edits to the server.
fn run_peer(url: &str, shared: Arc<Mutex<Shared>>, quit: Arc<AtomicBool>, waker: RepaintWaker) {
    let mut harness = UiHarness::new(1100.0, 720.0);
    let mut state = EnkrState::with_notes(NoteDatabase::new_in_memory());
    state.sync_identity = Some(IdentityStore::InMemory);
    state.hide_default_server = true;
    // The peer's sync events wake the *window's* loop so background progress renders.
    state.set_repaint_waker(waker);
    harness.frame(|ui| render(ui, &mut state));

    let mut script = Script::client2(url);
    let mut last = Instant::now();
    while !quit.load(Ordering::Acquire) {
        let now = Instant::now();
        let dt = (now - last).as_secs_f32().min(0.1);
        last = now;

        if !script.finished() {
            let events = script.tick(dt, harness.snapshot(), &state, &mut shared.lock().unwrap());
            for ev in events {
                harness.push_event(ev);
            }
            harness.frame(|ui| render(ui, &mut state));
            std::thread::sleep(Duration::from_millis(8));
        } else {
            // Nothing left to script: keep the app (and its sync engine) alive
            // so client1's remaining waits resolve, but stop busy-rendering.
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    state.shutdown();
}

/// The tiny "repo" client1 imports: a few markdown files with a subfolder so
/// the sidebar shows a folder tree after import.
fn create_sample_repo(root: &Path) {
    let docs = root.join("docs");
    std::fs::create_dir_all(&docs).expect("create sample repo dirs");
    let write = |path: PathBuf, text: &str| std::fs::write(path, text).expect("write sample file");
    write(
        root.join("README.md"),
        "# Horizon\n\nA tiny knowledge base used to demo Enkr.\n\n\
         - Written in plain markdown\n- Imported straight from disk\n- Synced end-to-end encrypted\n",
    );
    // No trailing newline: the checkbox line stays the *last* line, so the
    // scripted caret can reach it with a plain end-of-document navigation.
    write(
        root.join("TODO.md"),
        "# TODO\n\n- [x] Import this repo into Enkr\n- [x] Sync it to a server\n- [ ] Share it with the team",
    );
    write(
        docs.join("architecture.md"),
        "# Architecture\n\nNotes live in local-first replicas; the server only ever sees ciphertext.\n",
    );
    write(
        docs.join("roadmap.md"),
        "# Roadmap\n\n1. Realtime presence\n2. Mobile builds\n3. Public beta\n",
    );
}

/// In-process `enkr-syncd`, lifted from the `app_sync.rs` test harness. The
/// multi-thread runtime keeps serving in the background after `start` returns.
struct ShowcaseServer {
    rt: tokio::runtime::Runtime,
    handle: Option<enkr_syncd::ServerHandle>,
    addr: std::net::SocketAddr,
}

impl ShowcaseServer {
    fn start(dir: &Path) -> Self {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let db_path = dir.join("showcase-server.sqlite3");
        let handle = rt.block_on(async {
            let store = enkr_syncd::storage::SqliteStore::open(&db_path)
                .await
                .expect("server store");
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            enkr_syncd::serve(
                Arc::new(store),
                listener,
                enkr_syncd::ServerConfig::default(),
            )
            .await
        });
        Self {
            addr: handle.addr,
            handle: Some(handle),
            rt,
        }
    }

    fn url(&self) -> String {
        format!("ws://{}/ws", self.addr)
    }
}

impl Drop for ShowcaseServer {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.rt.block_on(handle.shutdown());
        }
    }
}

/// Cross-client coordination. Both scripts run in the same process, so
/// "barriers" are just flags one script publishes and the other waits on.
#[derive(Default)]
struct Shared {
    /// client2's identity key, published once its sync engine is up; client1
    /// "pastes" it into the share dialog.
    client2_key: Option<String>,
    /// client1 finished the invite flow.
    invited: bool,
    /// client2 opened the shared note and started typing.
    client2_typing: bool,
    /// client2 switched to the TODO note (client1 follows it there).
    client2_on_todo: bool,
}

/// How a step locates the widget it acts on, resolved against the previous
/// frame's snapshot (same conventions as the `app_sync.rs` helpers).
enum Selector {
    /// Node whose label or text equals the string (`UiSnapshot::try_node`).
    Label(String),
    /// Sidebar row of the note whose body contains the marker: the note title
    /// is resolved from the app state (so it also finds synced replicas whose
    /// title differs from the original file name), then matched against text
    /// in the sidebar area (left column, below the top bar), last match wins —
    /// mirroring the `click_sidebar_note` test helper.
    SidebarNoteContaining(&'static str),
    /// The n-th single-line input below the top bar (dialog fields).
    LineEdit(usize),
    /// The markdown editor (the only multiline textarea).
    Editor,
}

impl Selector {
    fn resolve(&self, snap: &UiSnapshot, state: &EnkrState) -> Option<Point> {
        let usable = |node: &&mae::testkit::UiNodeSnapshot| {
            node.visible && node.bounds.x1 > node.bounds.x0 && node.bounds.y1 > node.bounds.y0
        };
        let sidebar_text = |text: &str| {
            snap.nodes
                .iter()
                .filter(usable)
                .filter(|node| {
                    node.text.as_deref() == Some(text)
                        && node.bounds.x1 < SIDEBAR_RIGHT_EDGE
                        && node.bounds.y0 > TOP_BAR_BOTTOM
                })
                .last()
                .map(|node| node.center())
        };
        match self {
            Selector::Label(id) => snap
                .nodes
                .iter()
                .filter(usable)
                .find(|node| node.matches(id))
                .map(|node| node.center()),
            Selector::SidebarNoteContaining(marker) => {
                let title = state.notes.summaries().iter().find_map(|summary| {
                    let note = state.notes.note(&summary.id)?;
                    note.text().contains(marker).then(|| note.title())
                })?;
                sidebar_text(&title)
            }
            Selector::LineEdit(index) => snap
                .nodes
                .iter()
                .filter(usable)
                .filter(|node| {
                    node.flags.contains(UIBoxFlags::LINE_EDIT) && node.bounds.y0 >= TOP_BAR_BOTTOM
                })
                .nth(*index)
                .map(|node| node.center()),
            Selector::Editor => snap
                .nodes
                .iter()
                .filter(usable)
                .find(|node| node.flags.contains(UIBoxFlags::MULTILINE))
                .map(|node| node.center()),
        }
    }
}

/// What to type: fixed text, or a value another client publishes into
/// [`Shared`] (typing waits until it is available).
enum TextSpec {
    Lit(String),
    FromShared(fn(&Shared) -> Option<String>),
}

enum Step {
    /// Idle for the given seconds (pacing for the recording).
    Pause(f32),
    /// Glide the cursor to the widget and left-click it.
    Click(Selector),
    /// Glide the cursor to the widget and right-click it.
    RightClick(Selector),
    /// Type into the focused widget. `cps = f32::INFINITY` emits everything in
    /// one frame — visually a paste. `'\n'` is sent as a Return key press.
    Type { text: TextSpec, cps: f32 },
    /// Press a key (with the input pipeline's usual side effects, e.g. Escape
    /// closes dialogs), `count` times in one frame, optionally modified
    /// (e.g. Shift for selections).
    Keys {
        code: OSKeyCode,
        count: u32,
        flags: Option<OSEventFlag>,
    },
    /// Block until the app/shared state satisfies the condition.
    WaitApp {
        what: &'static str,
        cond: fn(&EnkrState, &Shared) -> bool,
    },
    /// Publish a value/flag from this client's state into [`Shared`].
    Publish(fn(&EnkrState, &mut Shared)),
    /// Click `click`, wait `every` seconds, and repeat until `until` resolves
    /// (used for "Refresh until the shared space shows up").
    ClickUntil {
        click: Selector,
        until: Selector,
        every: f32,
    },
}

/// Per-step progress of the state machine.
enum Phase {
    /// Step not started (or between ClickUntil rounds).
    Start,
    Glide {
        from: Point,
        to: Point,
        dur: f32,
        t: f32,
        right: bool,
    },
    /// Hovering over the target for a beat before pressing.
    Dwell { to: Point, t: f32, right: bool },
    /// Button held down over the target.
    Hold { to: Point, t: f32, right: bool },
    /// Post-click settle so the UI reaction is visible before the next step.
    Settle { t: f32 },
    Typing {
        chars: Vec<char>,
        next: usize,
        interval: f32,
        t: f32,
    },
    /// ClickUntil: waiting before re-checking / re-clicking.
    Recheck { t: f32 },
}

/// A click ripple drawn by the cursor overlay: `(x, y, age_seconds)`.
type Ripple = (f32, f32, f32);
const RIPPLE_LIFE: f32 = 0.45;
const MAX_RIPPLES: usize = 4;

struct Script {
    name: &'static str,
    steps: Vec<Step>,
    /// Total number of steps; `steps` is temporarily empty during dispatch
    /// (moved out for borrow reasons), so completion checks use this.
    total: usize,
    idx: usize,
    phase: Phase,
    /// Time spent on the current step; drives Pause and the failure timeout.
    step_t: f32,
    cursor: Point,
    ripples: Vec<Ripple>,
    /// Windowed client: animate cursor glides. Headless client: act instantly.
    animate: bool,
    failed: bool,
    out: Vec<OSEvent>,
}

impl Script {
    /// The visible client: imports the repo, connects, pushes and shares the
    /// space, then co-edits the README with client2.
    fn client1(url: &str) -> Self {
        use Selector::*;
        use Step::*;
        let steps = vec![
            Pause(1.5),
            // -- Import the sample repo as a new Space --------------------
            // Import lives in the space switcher's action rows now, not behind
            // an unlabelled glyph in the sidebar footer.
            Click(Label("###enkr_space_switcher".into())),
            Pause(0.4),
            Click(Label("###enkr_switcher_import".into())),
            Pause(0.6),
            Click(Label(REPO_NAME.into())), // folder row: enter the repo
            Pause(0.5),
            Click(Label("Import as a new space".into())),
            Pause(0.5),
            Click(Label("Import this folder".into())),
            WaitApp {
                what: "repo imported as a new space",
                cond: |state, _| {
                    state
                        .notes
                        .spaces()
                        .iter()
                        .any(|space| space.name == REPO_NAME)
                },
            },
            Pause(1.6), // let the imported tree sink in
            // -- Connect through the settings dialog ----------------------
            Click(Label(SETTINGS_ICON.into())),
            Pause(0.4),
            Click(Label("Sync & Devices".into())),
            Pause(0.5),
            Click(LineEdit(0)),
            Type {
                text: TextSpec::Lit(url.into()),
                cps: 40.0,
            },
            Click(Label("Add\u{2026}".into())),
            Pause(0.5),
            Click(Label("Use".into())),
            Pause(0.4),
            Click(LineEdit(1)),
            Type {
                text: TextSpec::Lit("alice".into()),
                cps: 12.0,
            },
            Click(Label("Connect".into())),
            WaitApp {
                what: "client1 connected",
                cond: |state, _| state.sync.as_ref().is_some_and(|sync| sync.connected()),
            },
            Pause(0.6),
            Keys {
                code: OSKeyCode::KeyEscape,
                count: 1,
                flags: None,
            },
            Pause(0.6),
            // -- Push the Space to the server ------------------------------
            RightClick(Label("###enkr_space_switcher".into())),
            Pause(0.6),
            Click(Label("Sync this space\u{2026} >".into())),
            Pause(0.5),
            Click(Label(format!("{url}  (active)"))),
            WaitApp {
                what: "space pushed to server",
                cond: space_pushed,
            },
            Pause(0.8),
            // -- Share it with client2 (paste its identity key) -------------
            WaitApp {
                what: "client2 published its identity key",
                cond: |_, shared| shared.client2_key.is_some(),
            },
            RightClick(Label("###enkr_space_switcher".into())),
            Pause(0.6),
            Click(Label("Share\u{2026}".into())),
            Pause(0.6),
            Click(LineEdit(0)),
            Type {
                text: TextSpec::FromShared(|shared| shared.client2_key.clone()),
                cps: f32::INFINITY, // a paste
            },
            Pause(0.6),
            Click(Label("Write".into())),
            Pause(0.4),
            Click(Label("Invite".into())),
            Pause(0.8),
            Publish(|_, shared| shared.invited = true),
            // -- Watch client2 arrive, then co-edit ------------------------
            WaitApp {
                what: "client2 started typing",
                cond: |_, shared| shared.client2_typing,
            },
            Click(SidebarNoteContaining("# Horizon")),
            WaitApp {
                what: "client2's first line synced back",
                cond: |state, _| note_contains(state, BOB_LINE_1),
            },
            Pause(0.8),
            Click(Label(DARK_THEME_ICON.into())), // switch to dark theme
            Pause(1.0),
            Click(Editor),
            Keys {
                code: OSKeyCode::KeyDownArrow,
                count: 60, // clamp the caret to the last line...
                flags: None,
            },
            Keys {
                code: OSKeyCode::KeyEnd,
                count: 1, // ...then to the end of it
                flags: None,
            },
            Type {
                text: TextSpec::Lit(format!("\n\n{ALICE_LINE}")),
                cps: 17.0,
            },
            WaitApp {
                what: "client2's reply synced back",
                cond: |state, _| note_contains(state, BOB_LINE_2),
            },
            Pause(0.8),
            Click(Label(RENDER_MARKDOWN_ICON.into())), // switch to rendered view
            // -- Follow client2 over to the TODO note -----------------------
            WaitApp {
                what: "client2 moved to the TODO note",
                cond: |_, shared| shared.client2_on_todo,
            },
            Pause(1.2),
            Click(SidebarNoteContaining("Share it with the team")),
            WaitApp {
                what: "client2 ticked the share checkbox",
                cond: |state, _| note_contains(state, "[x] Share it with the team"),
            },
            // -- Find bob's line again through global search -----------------
            Pause(1.5),
            Click(Label(SEARCH_ICON.into())), // "Search all notes"
            Pause(0.6),
            Type {
                // The palette autofocuses its input on open.
                text: TextSpec::Lit("the whol".into()),
                cps: 9.0,
            },
            Pause(1.2), // results populate
            // Open the best hit. The result rows carry a `###`-only id (no
            // display text), so they can't be located by label; the palette
            // opens the top result on Return, which is what a user would press.
            Keys {
                code: OSKeyCode::KeyEnter,
                count: 1,
                flags: None,
            },
            Pause(3.0),
        ];
        Self::new("client1", steps, true)
    }

    /// The invisible client: connects, publishes its identity key, pulls the
    /// shared space once invited, and types into the README.
    fn client2(url: &str) -> Self {
        use Selector::*;
        use Step::*;
        let steps = vec![
            Pause(2.0),
            // -- Connect ---------------------------------------------------
            Click(Label(SETTINGS_ICON.into())),
            Pause(0.3),
            Click(Label("Sync & Devices".into())),
            Pause(0.3),
            Click(LineEdit(0)),
            Type {
                text: TextSpec::Lit(url.into()),
                cps: f32::INFINITY,
            },
            Click(Label("Add\u{2026}".into())),
            Click(Label("Use".into())),
            Click(LineEdit(1)),
            Type {
                text: TextSpec::Lit("bob".into()),
                cps: f32::INFINITY,
            },
            Click(Label("Connect".into())),
            WaitApp {
                what: "client2 connected",
                cond: |state, _| state.sync.as_ref().is_some_and(|sync| sync.connected()),
            },
            Keys {
                code: OSKeyCode::KeyEscape,
                count: 1,
                flags: None,
            },
            Publish(|state, shared| {
                shared.client2_key = state
                    .sync
                    .as_ref()
                    .map(|sync| sync.identity_key().to_string());
            }),
            // -- Pull the shared space once invited -------------------------
            WaitApp {
                what: "client1 sent the invite",
                cond: |_, shared| shared.invited,
            },
            Pause(0.8),
            Click(Label("###enkr_status_pill".into())),
            ClickUntil {
                click: Label("Refresh".into()),
                until: Label("Sync".into()),
                every: 1.0,
            },
            Click(Label("Sync".into())),
            WaitApp {
                what: "client2 mirrors the space",
                cond: space_mirrored,
            },
            Keys {
                code: OSKeyCode::KeyEscape,
                count: 1,
                flags: None,
            },
            Pause(0.5),
            // Switch to the synced space through the switcher dropdown.
            Click(Label("###enkr_space_switcher".into())),
            Pause(0.4),
            Click(Label(REPO_NAME.into())),
            Pause(0.5),
            Click(SidebarNoteContaining("# Horizon")),
            Pause(1.0),
            // -- Write into the shared note --------------------------------
            Click(Editor),
            // Sweep a selection over the first half of the README (visible to
            // client1 as remote presence) before settling at the end to type.
            Keys {
                code: OSKeyCode::KeyUpArrow,
                count: 60,
                flags: None,
            },
            Keys {
                code: OSKeyCode::KeyHome,
                count: 1,
                flags: None,
            },
            Keys {
                code: OSKeyCode::KeyDownArrow,
                count: 4,
                flags: Some(OSEventFlag::Shift),
            },
            Pause(1.8), // hold the selection on screen
            Keys {
                code: OSKeyCode::KeyDownArrow,
                count: 60,
                flags: None,
            },
            Keys {
                code: OSKeyCode::KeyEnd,
                count: 1,
                flags: None,
            },
            Publish(|_, shared| shared.client2_typing = true),
            Type {
                text: TextSpec::Lit(format!("\n\n{BOB_LINE_1}")),
                cps: 14.0,
            },
            WaitApp {
                what: "alice's line synced over",
                cond: |state, _| note_contains(state, ALICE_LINE),
            },
            Pause(1.2),
            Keys {
                code: OSKeyCode::KeyDownArrow,
                count: 60,
                flags: None,
            },
            Keys {
                code: OSKeyCode::KeyEnd,
                count: 1,
                flags: None,
            },
            Type {
                text: TextSpec::Lit(format!("\n{BOB_LINE_2}")),
                cps: 14.0,
            },
            // -- Move to the TODO note and tick the last checkbox -----------
            Pause(1.5),
            Click(SidebarNoteContaining("Share it with the team")),
            Publish(|_, shared| shared.client2_on_todo = true),
            Pause(2.0), // give client1 time to follow
            Click(Editor),
            Keys {
                code: OSKeyCode::KeyDownArrow,
                count: 60, // the checkbox line is the note's last line
                flags: None,
            },
            Keys {
                code: OSKeyCode::KeyHome,
                count: 1,
                flags: None,
            },
            Keys {
                code: OSKeyCode::KeyRightArrow,
                count: 4, // caret lands after the space inside "- [ ]"
                flags: None,
            },
            Pause(0.6),
            Keys {
                code: OSKeyCode::KeyBackspace,
                count: 1, // delete the space...
                flags: None,
            },
            Type {
                text: TextSpec::Lit("x".into()), // ...and tick the box
                cps: 8.0,
            },
            Pause(2.0),
        ];
        Self::new("client2", steps, false)
    }

    fn new(name: &'static str, steps: Vec<Step>, animate: bool) -> Self {
        Self {
            name,
            total: steps.len(),
            steps,
            idx: 0,
            phase: Phase::Start,
            step_t: 0.0,
            cursor: Point::new(550.0, 650.0),
            ripples: Vec::new(),
            animate,
            failed: false,
            out: Vec::new(),
        }
    }

    fn finished(&self) -> bool {
        self.failed || self.idx >= self.total
    }

    /// Whether the *next* step to run needs this frame's layout snapshot to
    /// locate its target. Only the click-family steps resolve widgets against
    /// the snapshot; pauses, typing, key presses, waits and publishes don't,
    /// so those frames skip the snapshot and render like a plain app frame.
    fn needs_snapshot(&self) -> bool {
        matches!(
            self.steps.get(self.idx),
            Some(Step::Click(_) | Step::RightClick(_) | Step::ClickUntil { .. })
        )
    }

    /// Advance the script by one frame; returns the events to inject before
    /// this frame's widgets are built. `snap` is the *previous* frame.
    fn tick(
        &mut self,
        dt: f32,
        snap: &UiSnapshot,
        state: &EnkrState,
        shared: &mut Shared,
    ) -> Vec<OSEvent> {
        self.out.clear();
        for ripple in &mut self.ripples {
            ripple.2 += dt;
        }
        self.ripples.retain(|ripple| ripple.2 < RIPPLE_LIFE);
        if self.finished() {
            return Vec::new();
        }
        self.step_t += dt;
        // Cross-client waits are legitimately slow (the other script may take
        // minutes); a click that can't find its widget is a real failure and
        // should surface quickly.
        let timeout = match &self.steps[self.idx] {
            Step::WaitApp { .. } | Step::Type { .. } | Step::ClickUntil { .. } => STEP_TIMEOUT,
            _ => INTERACT_TIMEOUT,
        };
        if self.step_t > timeout {
            self.fail(state);
            return Vec::new();
        }

        // Move the steps out for the dispatch so the handlers can borrow
        // `self` mutably while reading the current step's payload.
        let steps = std::mem::take(&mut self.steps);
        match &steps[self.idx] {
            Step::Pause(secs) => {
                if self.step_t >= *secs {
                    self.complete_step();
                }
            }
            Step::Click(sel) => self.tick_click(dt, snap, state, sel, false, false),
            Step::RightClick(sel) => self.tick_click(dt, snap, state, sel, true, false),
            Step::ClickUntil {
                click,
                until,
                every,
            } => match &self.phase {
                Phase::Start => {
                    if until.resolve(snap, state).is_some() {
                        self.complete_step();
                    } else {
                        self.tick_click(dt, snap, state, click, false, true);
                    }
                }
                Phase::Recheck { t } => {
                    if t + dt >= *every {
                        self.phase = Phase::Start;
                    } else {
                        self.phase = Phase::Recheck { t: t + dt };
                    }
                }
                _ => self.tick_click(dt, snap, state, click, false, true),
            },
            Step::Type { text, cps } => self.tick_type(dt, text, *cps, shared),
            Step::Keys { code, count, flags } => {
                for _ in 0..*count {
                    self.out.push(OSEvent::press_with_flags(
                        OSKey::Keyboard(*code),
                        None,
                        *flags,
                    ));
                }
                self.complete_step();
            }
            Step::WaitApp { cond, .. } => {
                if cond(state, shared) {
                    self.complete_step();
                }
            }
            Step::Publish(run) => {
                run(state, shared);
                self.complete_step();
            }
        }
        self.steps = steps;
        std::mem::take(&mut self.out)
    }

    /// One frame of a click cycle: glide → press → hold → release → settle.
    /// `recheck` routes the end of the cycle back to ClickUntil's pacing.
    fn tick_click(
        &mut self,
        dt: f32,
        snap: &UiSnapshot,
        state: &EnkrState,
        sel: &Selector,
        right: bool,
        recheck: bool,
    ) {
        let button = if right {
            OSKey::RightMouseButton
        } else {
            OSKey::LeftMouseButton
        };
        match &self.phase {
            Phase::Start | Phase::Recheck { .. } => {
                // Wait (step_t keeps the timeout honest) until the target
                // exists — e.g. a dialog that is still opening.
                let Some(to) = sel.resolve(snap, state) else {
                    return;
                };
                if !self.animate {
                    self.out.push(OSEvent::mouse_move(to));
                    self.out.push(OSEvent::press(button, Some(to)));
                    self.out.push(OSEvent::release(button, Some(to)));
                    self.cursor = to;
                    if recheck {
                        self.phase = Phase::Recheck { t: 0.0 };
                    } else {
                        self.complete_step();
                    }
                    return;
                }
                let to = self.jittered_target(to);
                let dist = ((to.x() - self.cursor.x()).powi(2)
                    + (to.y() - self.cursor.y()).powi(2))
                .sqrt();
                self.phase = Phase::Glide {
                    from: self.cursor,
                    to,
                    dur: (0.35 + dist / 1100.0).clamp(0.35, 1.2),
                    t: 0.0,
                    right,
                };
            }
            Phase::Glide {
                from,
                to,
                dur,
                t,
                right,
            } => {
                let (from, dur, right) = (*from, *dur, *right);
                let t = t + dt;
                // Track the target while gliding in case the layout animates.
                let to = sel
                    .resolve(snap, state)
                    .map(|center| self.jittered_target(center))
                    .unwrap_or(*to);
                if t >= dur {
                    self.cursor = to;
                    self.out.push(OSEvent::mouse_move(to));
                    self.phase = Phase::Dwell { to, t: 0.0, right };
                } else {
                    // Minimum-jerk pacing along a slightly bowed path: real
                    // hands neither move in straight lines nor at piecewise
                    // constant speeds.
                    let k = smootherstep(t / dur);
                    self.cursor = curved_path(from, to, self.idx, k);
                    self.out.push(OSEvent::mouse_move(self.cursor));
                    self.phase = Phase::Glide {
                        from,
                        to,
                        dur,
                        t,
                        right,
                    };
                }
            }
            Phase::Dwell { to, t, right } => {
                // A beat of hesitation over the target before pressing.
                let (to, right) = (*to, *right);
                let t = t + dt;
                if t >= 0.14 + 0.18 * hash01(self.idx.wrapping_mul(11) + 5) {
                    self.out.push(OSEvent::press(button, Some(to)));
                    self.push_ripple(to);
                    self.phase = Phase::Hold { to, t: 0.0, right };
                } else {
                    self.phase = Phase::Dwell { to, t, right };
                }
            }
            Phase::Hold { to, t, right } => {
                let (to, right) = (*to, *right);
                let t = t + dt;
                if t >= 0.08 + 0.05 * hash01(self.idx.wrapping_mul(13) + 7) {
                    let button = if right {
                        OSKey::RightMouseButton
                    } else {
                        OSKey::LeftMouseButton
                    };
                    self.out.push(OSEvent::release(button, Some(to)));
                    self.phase = Phase::Settle { t: 0.0 };
                } else {
                    self.phase = Phase::Hold { to, t, right };
                }
            }
            Phase::Settle { t } => {
                let t = t + dt;
                if t >= 0.45 {
                    if recheck {
                        self.phase = Phase::Recheck { t: 0.0 };
                    } else {
                        self.complete_step();
                    }
                } else {
                    self.phase = Phase::Settle { t };
                }
            }
            Phase::Typing { .. } => unreachable!("click step never enters Typing"),
        }
    }

    fn tick_type(&mut self, dt: f32, text: &TextSpec, cps: f32, shared: &Shared) {
        if let Phase::Start = self.phase {
            // A shared value may not be published yet; step_t still times out.
            let text = match text {
                TextSpec::Lit(text) => text.clone(),
                TextSpec::FromShared(fetch) => match fetch(shared) {
                    Some(text) => text,
                    None => return,
                },
            };
            self.phase = Phase::Typing {
                chars: text.chars().collect(),
                next: 0,
                interval: 1.0 / cps.max(0.1),
                t: 0.0,
            };
        }
        let Phase::Typing {
            chars,
            next,
            interval,
            t,
        } = &mut self.phase
        else {
            unreachable!("type step is always in Typing phase")
        };
        *t += dt;
        while *next < chars.len() {
            let mut delay = if interval.is_finite() {
                *interval * key_jitter(*next)
            } else {
                0.0
            };
            // Breathe at word boundaries: bursts within a word, a beat after.
            if *next > 0 && matches!(chars[*next - 1], ' ' | '\n') {
                delay *= 1.9;
            }
            if *t < delay {
                break;
            }
            *t -= delay;
            let ch = chars[*next];
            *next += 1;
            if ch == '\n' {
                self.out
                    .push(OSEvent::press(OSKey::Keyboard(OSKeyCode::KeyEnter), None));
            } else {
                self.out.push(OSEvent::text(ch));
            }
        }
        if let Phase::Typing { chars, next, .. } = &self.phase
            && *next >= chars.len()
        {
            self.complete_step();
        }
    }

    fn complete_step(&mut self) {
        self.idx += 1;
        self.step_t = 0.0;
        self.phase = Phase::Start;
        if self.idx >= self.total {
            println!("showcase: {} script complete", self.name);
        }
    }

    fn fail(&mut self, state: &EnkrState) {
        let what = match &self.steps[self.idx] {
            Step::WaitApp { what, .. } => what,
            _ => "step",
        };
        eprintln!(
            "showcase: {} gave up on step {} ({what}) after {:.0}s",
            self.name, self.idx, self.step_t
        );
        // Dump the app state so a stuck wait condition can be diagnosed from
        // the log of an unattended run.
        eprintln!(
            "showcase: {} sync connected={:?} last_error={:?}",
            self.name,
            state.sync.as_ref().map(|sync| sync.connected()),
            state.sync.as_ref().and_then(|sync| sync.last_error()),
        );
        for space in state.notes.spaces() {
            eprintln!(
                "showcase: {}   space id={} name={:?} remote={:?}",
                self.name, space.id, space.name, space.remote
            );
            for id in state.notes.note_ids_in_space(space.id) {
                if let Some(note) = state.notes.note(&id) {
                    eprintln!(
                        "showcase: {}     note id={id:?} remote_doc={:?} len={}",
                        self.name,
                        note.remote_doc(),
                        note.text().len()
                    );
                }
            }
        }
        self.failed = true;
    }

    /// Aim slightly off the widget's exact center, like a hand would. The
    /// offset is deterministic per step so re-resolution during a glide keeps
    /// aiming at the same spot.
    fn jittered_target(&self, center: Point) -> Point {
        Point::new(
            center.x() + 6.0 * hash01(self.idx.wrapping_mul(5) + 2) - 3.0,
            center.y() + 4.0 * hash01(self.idx.wrapping_mul(9) + 4) - 2.0,
        )
    }

    fn push_ripple(&mut self, at: Point) {
        if self.ripples.len() >= MAX_RIPPLES {
            self.ripples.remove(0);
        }
        self.ripples.push((at.x(), at.y(), 0.0));
    }

    /// Draw the scripted cursor and click ripples on the top-most overlay.
    /// The overlay is not clickable, so it never intercepts the script's own
    /// synthetic clicks.
    fn draw_cursor(&self, ui: &mut IMUI) {
        if !self.animate {
            return;
        }
        let (x, y) = (self.cursor.x(), self.cursor.y());
        let mut ripples = [(0.0f32, 0.0f32, f32::MAX); MAX_RIPPLES];
        for (slot, ripple) in ripples.iter_mut().zip(&self.ripples) {
            *slot = *ripple;
        }
        ui.overlay_canvas("###showcase_cursor", move |drawer, _rect, _clip| {
            let accent = Color::new("#4f9cf9");
            for (rx, ry, age) in ripples {
                if age >= RIPPLE_LIFE {
                    continue;
                }
                let k = age / RIPPLE_LIFE;
                let radius = 10.0 + 26.0 * k;
                let color = Color {
                    a: 0.35 * (1.0 - k),
                    ..accent
                };
                draw_circle(drawer, rx, ry, radius, color);
            }
            draw_arrow_cursor(drawer, x, y);
        });
    }
}

fn draw_circle(drawer: &mut mae::draw::Drawer, x: f32, y: f32, radius: f32, color: Color) {
    drawer.draw_rect(
        &RectCoords::from_size(x - radius, y - radius, radius * 2.0, radius * 2.0),
        color,
        radius,
    );
}

/// The macOS default pointer as pixel art: black arrow with a white outline,
/// hotspot at the tip (top-left). `B` = black fill, `W` = white outline.
const ARROW_CURSOR: [&str; 19] = [
    "W           ",
    "WW          ",
    "WBW         ",
    "WBBW        ",
    "WBBBW       ",
    "WBBBBW      ",
    "WBBBBBW     ",
    "WBBBBBBW    ",
    "WBBBBBBBW   ",
    "WBBBBBBBBW  ",
    "WBBBBBBBBBW ",
    "WBBBBBBBBBBW",
    "WBBBBBBWWWWW",
    "WBBBWBBW    ",
    "WBBW WBBW   ",
    "WBW  WBBW   ",
    "WW    WBBW  ",
    "W     WBBW  ",
    "       WW   ",
];

/// Rasterise [`ARROW_CURSOR`] at 1 logical px per cell, run-length-merging
/// same-colored spans so each row costs a handful of quads.
fn draw_arrow_cursor(drawer: &mut mae::draw::Drawer, x: f32, y: f32) {
    let black = Color::new("#000000");
    let white = Color::new("#ffffff");
    for (row, line) in ARROW_CURSOR.iter().enumerate() {
        let cells = line.as_bytes();
        let mut col = 0;
        while col < cells.len() {
            let cell = cells[col];
            let start = col;
            while col < cells.len() && cells[col] == cell {
                col += 1;
            }
            if cell == b' ' {
                continue;
            }
            let color = if cell == b'B' { black } else { white };
            drawer.draw_rect(
                &RectCoords::from_size(x + start as f32, y + row as f32, (col - start) as f32, 1.0),
                color,
                0.0,
            );
        }
    }
}

/// Minimum-jerk-style S-curve: gentler acceleration and deceleration than a
/// plain smoothstep, close to how a hand actually moves a mouse.
fn smootherstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

/// Deterministic pseudo-random value in [0, 1) from a seed.
fn hash01(seed: usize) -> f32 {
    (seed.wrapping_mul(2654435761) % 1000) as f32 / 1000.0
}

/// Point along a quadratic bezier from `from` to `to`, bowed sideways a little
/// (direction alternates per step) so glides curve like real mouse movement.
fn curved_path(from: Point, to: Point, seed: usize, k: f32) -> Point {
    let (dx, dy) = (to.x() - from.x(), to.y() - from.y());
    let dist = (dx * dx + dy * dy).sqrt().max(1.0);
    let bend = dist
        * (0.06 + 0.08 * hash01(seed.wrapping_mul(7) + 3))
        * if hash01(seed.wrapping_mul(3) + 1) < 0.5 {
            -1.0
        } else {
            1.0
        };
    let ctrl = Point::new(
        (from.x() + to.x()) / 2.0 - dy / dist * bend,
        (from.y() + to.y()) / 2.0 + dx / dist * bend,
    );
    let inv = 1.0 - k;
    Point::new(
        inv * inv * from.x() + 2.0 * inv * k * ctrl.x() + k * k * to.x(),
        inv * inv * from.y() + 2.0 * inv * k * ctrl.y() + k * k * to.y(),
    )
}

/// Deterministic per-keystroke pacing wobble in [0.5, 1.6) so typing reads as
/// human rather than metronomic.
fn key_jitter(index: usize) -> f32 {
    0.5 + (index.wrapping_mul(2654435761) % 110) as f32 / 100.0
}

/// The imported space exists, is mapped to a remote space, and at least one of
/// its notes is bound to a remote doc — i.e. the push actually went through.
fn space_pushed(state: &EnkrState, _: &Shared) -> bool {
    let Some(space) = state
        .notes
        .spaces()
        .iter()
        .find(|space| space.name == REPO_NAME)
    else {
        return false;
    };
    space.remote.is_some()
        && state.notes.note_ids_in_space(space.id).iter().any(|id| {
            state
                .notes
                .note(id)
                .is_some_and(|n| n.remote_doc().is_some())
        })
}

/// client2 holds a local replica of the shared space with the README content.
fn space_mirrored(state: &EnkrState, _: &Shared) -> bool {
    state
        .notes
        .spaces()
        .iter()
        .any(|space| space.name == REPO_NAME)
        && state.notes.summaries().iter().any(|summary| {
            state.notes.note(&summary.id).is_some_and(|note| {
                note.remote_doc().is_some() && note.text().contains("# Horizon")
            })
        })
}

/// Any note whose body contains `marker` (used to observe the peer's edits).
fn note_contains(state: &EnkrState, marker: &str) -> bool {
    state.notes.summaries().iter().any(|summary| {
        state
            .notes
            .note(&summary.id)
            .is_some_and(|note| note.text().contains(marker))
    })
}
