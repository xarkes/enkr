use enkr_proto::crypto::{BlobKey, content_hash};
use enkr_proto::wire::ImageMime;
use mae::imui::TextEditBuffer;
#[cfg(not(target_arch = "wasm32"))]
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
#[cfg(not(target_arch = "wasm32"))]
use std::fs;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::Mutex;
#[cfg(target_arch = "wasm32")]
use std::{cell::RefCell, rc::Rc};
use std::{
    collections::HashMap,
    error::Error,
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
#[cfg(not(target_arch = "wasm32"))]
use unicode_normalization::UnicodeNormalization;
// `std::time::Instant`/`SystemTime` unconditionally panic on wasm32-unknown-
// unknown; `web_time`'s are API-identical, real `std::time` on native, and
// backed by `performance.now()`/`Date.now()` on wasm32 — see Cargo.toml.
use uuid::Uuid;
use web_time::{Instant, SystemTime, UNIX_EPOCH};
use yrs::{
    Any, Assoc, Doc, GetString, IndexedSequence, Observable, OffsetKind, Options, Out, ReadTxn,
    StateVector, StickyIndex, Text, TextRef, Transact, Update, types::Delta,
    updates::decoder::Decode,
};

/// A body doc indexes positions in **UTF-16 code units** for every offset API
/// (insert/delete, caret sticky indices). This keeps `StickyIndex::get_offset`
/// internally consistent across a fragmented item structure - under the default
/// byte `OffsetKind`, sticky-index input and `get_offset` output disagree once
/// the doc has several items, so caret anchors resolve to the wrong place.
fn new_body_doc() -> Doc {
    Doc::with_options(Options {
        offset_kind: OffsetKind::Utf16,
        ..Options::default()
    })
}

/// Origin tag for transactions applying *remote* sync data; the sync observer
/// skips them so they don't echo back into the network pipeline.
pub const REMOTE_ORIGIN: &str = "enkr-sync";

const AUTOSAVE_DELAY: Duration = Duration::from_millis(750);
const WELCOME_NOTE_ID: &str = "Welcome";
const WELCOME_NOTE_TEXT: &str =
    "# Welcome\n## First note\n\nThis is your first note! Let's try to play with it :)";
pub const DEFAULT_SPACE_ID: i64 = 1;
pub const DEFAULT_SPACE_NAME: &str = "Space";
const META_ALLOW_EMPTY_SPACES: &str = "allow_empty_spaces";
const LEGACY_DEFAULT_SPACE_NAMES: &[&str] = &[
    "My Notes",
    "Enkr notebook",
    "Enkr Notebook",
    "Enkr space",
    "Enkr Space",
];

pub type NoteDbResult<T> = Result<T, Box<dyn Error>>;

/// A space is a top-level collection of notes and directories. Identified by a stable
/// integer id (its SQL primary key); notes reference it via `space_id`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Space {
    pub id: i64,
    pub name: String,
    /// Remote sync-space id once this space was pushed to / fetched from a
    /// sync server; None for purely local spaces.
    pub remote: Option<Uuid>,
    /// The sync server (normalized ws/wss URL) this space is bound to. A space
    /// is bound to **exactly one** server and never syncs against another, so
    /// switching the active server can't push a server-A space onto server B.
    /// None for purely local spaces.
    pub server: Option<String>,
}

/// A folder groups notes inside a space. Identified by a UUID so folders keep
/// their identity across devices when the space syncs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    pub id: Uuid,
    pub space_id: i64,
    /// Parent folder in the same space (`None` = top-level folder).
    pub parent: Option<Uuid>,
    pub name: String,
    /// Persisted UI state: folded folders hide their children in the sidebar.
    pub folded: bool,
    /// This folder's create/rename hasn't been acknowledged by the sync
    /// server yet. Mirrors the note-level `needs_push` durability contract:
    /// flagged folders are written into the space index doc on (re)connect
    /// and are protected from remote-absence cleanup until acknowledged.
    pub needs_push: bool,
}

/// A binary image file inside a space. Modelled like a source note (a named
/// file that lives in a space + optional folder and syncs), but its content is
/// raw image bytes held in a dedicated blob store rather than a Yrs document.
/// Referenced from markdown as `./blob/<name>`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blob {
    /// Stable id (UUID string); the sync/storage key. Distinct from `name`,
    /// which is the user-facing path used in `./blob/<name>` links.
    pub id: String,
    pub space_id: i64,
    /// Folder this blob sits in (None = space root).
    pub folder: Option<Uuid>,
    pub name: String,
    pub mime: ImageMime,
    /// SHA-256 of `bytes`; recorded so a synced peer can verify decrypted blob
    /// content against the (authenticated) index-doc entry.
    pub content_hash: [u8; 32],
    /// This blob's own random content key. It is published in the space index
    /// doc (which is itself re-sealed under the current epoch by ordinary
    /// snapshot compaction), so blob ciphertext never has to be re-encrypted
    /// when a space epoch rotates. Local-only blobs carry one too — the key is
    /// minted at creation, before anyone knows whether the space will sync.
    pub key: [u8; 32],
    pub bytes: Vec<u8>,
    /// Content/metadata not yet acknowledged by the sync server (same
    /// durability contract as a note's `needs_push`): flagged blobs re-upload
    /// + re-announce on (re)connect and are protected from remote-absence
    /// cleanup until acknowledged.
    pub needs_push: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteSummary {
    pub id: String,
    pub title: String,
    pub file_path: String,
    pub space_id: i64,
    /// Folder this note sits in (None = space root).
    pub folder: Option<Uuid>,
    /// First non-heading line of the body, used as a list preview.
    pub preview: String,
    /// ISO timestamp of the last edit, used for the list date label.
    pub updated: String,
}

pub struct NoteDatabase {
    notes: Vec<Note>,
    /// Ordered list of spaces. Tracked explicitly so empty spaces persist.
    spaces: Vec<Space>,
    /// Bumped by every local space metadata mutation that must be reflected
    /// into remote space index docs.
    spaces_rev: u64,
    /// Ordered list of folders across all spaces (small; filtered per space).
    folders: Vec<Folder>,
    /// Bumped by every folder/assignment mutation; the sync bridge skips its
    /// per-frame index diff pass when this hasn't moved.
    folders_rev: u64,
    /// Image blobs across all spaces (filtered per space on demand).
    blobs: Vec<Blob>,
    /// Bumped by every blob create/delete/metadata mutation; the sync bridge
    /// skips its per-frame index diff pass when this hasn't moved.
    blobs_rev: u64,
    next_note_number: usize,
    next_space_id: i64,
    /// Persistence thread handle; None for in-memory databases.
    store: Option<NoteStoreHandle>,
    /// Small app-level key/value settings (server URL, nickname, …),
    /// write-through to the store when one exists.
    meta: HashMap<String, String>,
}

/// Replace `dst`'s contents with `src`, keeping `dst`'s allocation.
fn overwrite(dst: &mut String, src: &str) {
    dst.clear();
    dst.push_str(src);
}

pub struct Note {
    id: String,
    file_path: String,
    space_id: i64,
    frontmatter_title: Option<String>,
    created: String,
    updated: String,
    /// Folder this note sits in (None = space root).
    folder: Option<Uuid>,
    /// The folder assignment changed locally and hasn't been written into
    /// the space index doc + acknowledged yet. While set, remote assignment
    /// state never overrides the local one (same contract as `needs_push`).
    folder_needs_push: bool,
    /// Remote sync doc id once this note is mapped to a synced doc.
    remote_doc: Option<Uuid>,
    /// This note may hold content the sync server hasn't acknowledged yet.
    /// Persisted atomically with the content (same row) — the single source
    /// of sync durability: on boot/reconnect, flagged notes ship their full
    /// state (idempotent), so in-flight loss anywhere downstream is safe.
    needs_push: bool,
    doc: Doc,
    body: TextRef,
    /// Forwards local update bytes to the sync engine (skips remote origins).
    sync_observer: Option<yrs::Subscription>,
    /// Bumped by the sync observer on every *local* edit (skips remote
    /// applies). Lets the UI tell an edit-induced caret move (which rides the
    /// pushed update) apart from a navigation move (which needs a presence
    /// ping). `Arc<Atomic>` so the observer closure can own a handle.
    local_edit_clock: Arc<AtomicU64>,
    dirty: bool,
    last_edit_at: Option<Instant>,
    /// Cached [`Self::title`]. Derived from `frontmatter_title`/`file_path`.
    title: String,
    /// Cached [`Self::preview`]. Deriving it materializes the whole Yrs body
    /// (`text()`), so it is computed on mutation rather than on every read —
    /// the sidebar asks every note for one on every frame.
    preview: String,
}

impl NoteDatabase {
    pub fn new_in_memory() -> Self {
        Self {
            notes: vec![Note::new(WELCOME_NOTE_ID, WELCOME_NOTE_TEXT)],
            spaces: vec![default_space()],
            spaces_rev: 0,
            folders: Vec::new(),
            folders_rev: 0,
            blobs: Vec::new(),
            blobs_rev: 0,
            next_note_number: 1,
            next_space_id: DEFAULT_SPACE_ID + 1,
            store: None,
            // Pre-marked as onboarded. This constructor is never a real first
            // install — both real paths go through `open`/`open_wasm`, and this
            // is either a test fixture or the fallback used when opening the
            // real store failed. Showing someone the welcome screen because
            // their database would not open would be actively misleading.
            meta: onboarded_meta(),
        }
    }

    /// In-memory database seeded with representative content, used for design
    /// previews/screenshots so the layout renders against realistic data.
    pub fn demo() -> Self {
        let spaces = vec![
            Space {
                id: 1,
                name: DEFAULT_SPACE_NAME.to_string(),
                remote: None,
                server: None,
            },
            Space {
                id: 2,
                name: "Work".to_string(),
                remote: None,
                server: None,
            },
            Space {
                id: 3,
                name: "Ideas".to_string(),
                remote: None,
                server: None,
            },
            Space {
                id: 4,
                name: "Archive".to_string(),
                remote: None,
                server: None,
            },
        ];

        // (id, space_id, updated, body). The first seven sit in the default space to drive the
        // notes list; the rest populate the other spaces so their counts read naturally.
        let seed: &[(&str, i64, &str, &str)] = &[
            (
                "Welcome",
                1,
                "2026-06-09T11:32:00+00:00",
                "# Welcome\n\n## First note\n\nThis is your first note! Let's try to play with it :)\n\n## First note\n\nThis is your first note! Let's try to play with it :)\n\n## First note\n\nThis is your first note! Let's try to play with it :)\n\n## First note\n\nThis is your first note! Let's try to play with it :)\n\n## First note\n\nThis is your first note! Let's try to play with it :)\n\n## First note\n\nThis is your first note! Let's try to play with it :)\n\n## First note\n\nThis is your first note! Let's try to play with it :)\n\n## First note\n\nThis is your first note! Let's try to play with it :)",
            ),
            (
                "Product roadmap",
                1,
                "2026-06-08T16:10:00+00:00",
                "# Product roadmap\n\nQ3 milestones and deliverables for the team.",
            ),
            (
                "Design inspiration",
                1,
                "2026-05-17T09:00:00+00:00",
                "# Design inspiration\n\nIdeas from Dribbble and Behance to explore.",
            ),
            (
                "Meeting notes - 5/17",
                1,
                "2026-05-17T14:30:00+00:00",
                "# Meeting notes - 5/17\n\nDiscussed user research findings and next steps.",
            ),
            (
                "Personal goals",
                1,
                "2026-05-14T20:00:00+00:00",
                "# Personal goals\n\nHealth, learning, and travel plans for the year.",
            ),
            (
                "Reading list",
                1,
                "2026-05-09T08:15:00+00:00",
                "# Reading list\n\nAtomic Habits, Deep Work, Atlas of the Heart.",
            ),
            (
                "Workflows",
                1,
                "2026-05-02T11:45:00+00:00",
                "# Workflows\n\nAutomation ideas and tools to streamline the day.",
            ),
            (
                "Sprint planning",
                2,
                "2026-06-07T10:00:00+00:00",
                "# Sprint planning\n\nBacklog grooming for the next sprint.",
            ),
            (
                "1:1 notes",
                2,
                "2026-06-05T15:00:00+00:00",
                "# 1:1 notes\n\nTalking points and follow-ups.",
            ),
            (
                "OKRs",
                2,
                "2026-05-30T09:00:00+00:00",
                "# OKRs\n\nObjectives and key results for the quarter.",
            ),
            (
                "App concept",
                3,
                "2026-06-01T18:00:00+00:00",
                "# App concept\n\nA calmer way to take notes.",
            ),
            (
                "Side projects",
                3,
                "2026-05-20T12:00:00+00:00",
                "# Side projects\n\nThings to build on weekends.",
            ),
            (
                "Old receipts",
                4,
                "2026-03-12T08:00:00+00:00",
                "# Old receipts\n\nArchived for reference.",
            ),
        ];

        let mut notes = Vec::new();
        for (id, space_id, updated, text) in seed {
            let mut note = Note::new(*id, *text);
            note.space_id = *space_id;
            note.created = updated.to_string();
            note.updated = updated.to_string();
            notes.push(note);
        }

        Self {
            next_note_number: next_note_number(&notes),
            next_space_id: next_space_id(&spaces),
            notes,
            spaces,
            spaces_rev: 0,
            folders: Vec::new(),
            folders_rev: 0,
            blobs: Vec::new(),
            blobs_rev: 0,
            store: None,
            // See `new_in_memory`: a seeded fixture is not a first run.
            meta: onboarded_meta(),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn open(path: impl AsRef<Path>) -> NoteDbResult<Self> {
        let store = SqliteNoteStore::open(path)?;
        let notes = store.load_notes()?;
        let spaces = store.load_spaces()?;
        let folders = store.load_folders()?;
        let blobs = store.load_blobs()?;
        let meta = store.load_meta()?;
        Self::finish_open(
            NoteStoreHandle::spawn(store),
            notes,
            spaces,
            folders,
            blobs,
            meta,
        )
    }

    /// wasm32: the IndexedDB-backed counterpart to `open` — necessarily
    /// async, since there's no synchronous way to read from IndexedDB (and,
    /// unlike native, no blocking-boot-on-a-background-thread trick to hide
    /// that behind — see `sync/mod.rs`'s `SyncClient::spawn` for the same
    /// reasoning applied to sync). The caller (the wasm32 entry point) drives
    /// this once at startup before the first frame builds.
    #[cfg(target_arch = "wasm32")]
    pub async fn open_wasm(db_name: &str) -> NoteDbResult<Self> {
        let store = IndexedDbNoteStore::open(db_name).await?;
        let (notes, spaces, folders, blobs, meta) = store.load_all().await?;
        Self::finish_open(
            NoteStoreHandle::spawn(store),
            notes,
            spaces,
            folders,
            blobs,
            meta,
        )
    }

    /// Post-load normalization shared by `open` (native) and `open_wasm`
    /// (wasm32) — the same regardless of which platform's store the raw
    /// notes/spaces/folders/blobs/meta were fetched from: welcome-note
    /// seeding, ensuring at least one space exists, and dropping/repointing
    /// notes/folders/blobs left orphaned by a vanished space or folder.
    fn finish_open(
        handle: NoteStoreHandle,
        mut notes: Vec<Note>,
        mut spaces: Vec<Space>,
        mut folders: Vec<Folder>,
        mut blobs: Vec<Blob>,
        meta: HashMap<String, String>,
    ) -> NoteDbResult<Self> {
        let allow_empty_spaces = meta.get(META_ALLOW_EMPTY_SPACES).is_some_and(|v| v == "1");
        if notes.is_empty() && !allow_empty_spaces {
            let mut note = Note::new(WELCOME_NOTE_ID, WELCOME_NOTE_TEXT);
            note.mark_dirty();
            notes.push(note);
        }

        // Make sure there is at least one space to land notes in, unless the
        // user explicitly deleted every space. Legacy notes default to the
        // first available space.
        if spaces.is_empty() && (!allow_empty_spaces || !notes.is_empty()) {
            spaces.push(default_space());
        }
        normalize_default_space_name(&mut spaces);
        if let Some(fallback_id) = spaces.first().map(|space| space.id) {
            for note in &mut notes {
                if !spaces.iter().any(|space| space.id == note.space_id) {
                    note.space_id = fallback_id;
                }
            }
        }

        // Blobs of vanished spaces go; blobs pointing at vanished folders fall
        // back to the space root (handled with notes below).
        blobs.retain(|blob| spaces.iter().any(|space| space.id == blob.space_id));

        // Folders of vanished spaces go; notes pointing at vanished folders
        // fall back to the space root.
        folders.retain(|folder| spaces.iter().any(|space| space.id == folder.space_id));
        for note in &mut notes {
            if let Some(folder) = note.folder
                && !folders
                    .iter()
                    .any(|f| f.id == folder && f.space_id == note.space_id)
            {
                note.folder = None;
            }
        }
        let folder_ids: Vec<Uuid> = folders.iter().map(|folder| folder.id).collect();
        let folder_spaces: Vec<(Uuid, i64)> = folders
            .iter()
            .map(|folder| (folder.id, folder.space_id))
            .collect();
        for folder in &mut folders {
            if folder.parent == Some(folder.id)
                || folder.parent.is_some_and(|parent| {
                    !folder_ids.iter().any(|id| *id == parent)
                        || !folder_spaces
                            .iter()
                            .any(|(id, space)| *id == parent && *space == folder.space_id)
                })
            {
                folder.parent = None;
            }
        }

        // Loading already happened above (synchronously on native, awaited on
        // wasm32); from here on the store belongs to the writer thread/task
        // and the UI thread never blocks on it.
        let mut db = Self {
            next_note_number: next_note_number(&notes),
            next_space_id: next_space_id(&spaces),
            notes,
            spaces,
            spaces_rev: 0,
            folders,
            folders_rev: 0,
            blobs,
            blobs_rev: 0,
            store: Some(handle),
            meta,
        };
        db.persist_spaces()?;
        db.persist_folders()?;
        db.persist_blobs()?;
        db.flush_dirty()?;
        Ok(db)
    }

    /// App-level setting (server URL, nickname, …); empty values unset.
    pub fn meta_get(&self, key: &str) -> Option<&str> {
        self.meta.get(key).map(String::as_str)
    }

    pub fn meta_set(&mut self, key: &str, value: &str) {
        if value.is_empty() {
            self.meta.remove(key);
        } else {
            self.meta.insert(key.to_string(), value.to_string());
        }
        if let Some(store) = self.store.as_ref() {
            store.send(WriteOp::Meta(key.to_string(), value.to_string()));
        }
    }

    /// All spaces, in display order.
    pub fn spaces(&self) -> &[Space] {
        &self.spaces
    }

    /// The space new notes land in when none is specified.
    pub fn default_space_id(&self) -> i64 {
        self.spaces
            .first()
            .map(|space| space.id)
            .unwrap_or(DEFAULT_SPACE_ID)
    }

    pub fn create_space(&mut self) -> i64 {
        let name = format!("Space {}", self.spaces.len());
        self.create_space_named(name)
    }

    pub fn create_space_named(&mut self, name: impl Into<String>) -> i64 {
        let id = self.next_space_id;
        self.next_space_id += 1;
        self.meta_set(META_ALLOW_EMPTY_SPACES, "");
        self.spaces.push(Space {
            id,
            name: name.into(),
            remote: None,
            server: None,
        });
        if let Err(err) = self.persist_spaces() {
            eprintln!("Could not persist space: {err}");
        }
        self.spaces_rev += 1;
        id
    }

    pub fn rename_space(&mut self, space_id: i64, name: &str) {
        if let Some(space) = self.spaces.iter_mut().find(|space| space.id == space_id)
            && space.name != name
        {
            space.name = name.to_string();
            if let Err(err) = self.persist_spaces() {
                eprintln!("Could not persist space: {err}");
            }
            self.spaces_rev += 1;
        }
    }

    /// Delete a local space and all local notes/folders inside it.
    pub fn delete_space(&mut self, space_id: i64) -> bool {
        if !self.spaces.iter().any(|space| space.id == space_id) {
            return false;
        }

        let deleted_notes: Vec<String> = self
            .notes
            .iter()
            .filter(|note| note.space_id == space_id)
            .map(|note| note.id().to_string())
            .collect();
        self.notes.retain(|note| note.space_id != space_id);
        self.folders.retain(|folder| folder.space_id != space_id);
        self.spaces.retain(|space| space.id != space_id);
        if self.spaces.is_empty() {
            self.meta_set(META_ALLOW_EMPTY_SPACES, "1");
        }

        self.persist_note_deletions(deleted_notes);
        if let Err(err) = self.persist_spaces() {
            eprintln!("Could not persist spaces: {err}");
        }
        if let Err(err) = self.persist_folders() {
            eprintln!("Could not persist folders: {err}");
        }
        self.spaces_rev += 1;
        self.folders_rev += 1;
        true
    }

    /// Record (and persist) that a local space mirrors a remote sync space.
    pub fn set_space_remote(&mut self, space_id: i64, remote: Option<Uuid>) {
        if let Some(space) = self.spaces.iter_mut().find(|space| space.id == space_id) {
            space.remote = remote;
            if let Err(err) = self.persist_spaces() {
                eprintln!("Could not persist space: {err}");
            }
        }
    }

    pub fn space_remote(&self, space_id: i64) -> Option<Uuid> {
        self.spaces
            .iter()
            .find(|space| space.id == space_id)
            .and_then(|space| space.remote)
    }

    /// Bind (or unbind) a space to a sync server (normalized ws/wss URL). A
    /// space only ever syncs against its bound server, so this is what keeps a
    /// server-A space from being pushed to server B after an active-server
    /// switch.
    pub fn set_space_server(&mut self, space_id: i64, server: Option<String>) {
        if let Some(space) = self.spaces.iter_mut().find(|space| space.id == space_id) {
            space.server = server;
            if let Err(err) = self.persist_spaces() {
                eprintln!("Could not persist space: {err}");
            }
        }
    }

    pub fn space_server(&self, space_id: i64) -> Option<&str> {
        self.spaces
            .iter()
            .find(|space| space.id == space_id)
            .and_then(|space| space.server.as_deref())
    }

    pub fn space_by_remote(&self, remote: &Uuid) -> Option<i64> {
        self.spaces
            .iter()
            .find(|space| space.remote.as_ref() == Some(remote))
            .map(|space| space.id)
    }

    pub fn space_name(&self, space_id: i64) -> Option<&str> {
        self.spaces
            .iter()
            .find(|space| space.id == space_id)
            .map(|space| space.name.as_str())
    }

    /// Monotonic space metadata mutation counter.
    pub fn spaces_rev(&self) -> u64 {
        self.spaces_rev
    }

    // -- folders ------------------------------------------------------------

    /// All folders of a space, in display order.
    pub fn folders_in_space(&self, space_id: i64) -> impl Iterator<Item = &Folder> {
        self.folders
            .iter()
            .filter(move |folder| folder.space_id == space_id)
    }

    pub fn folder(&self, id: &Uuid) -> Option<&Folder> {
        self.folders.iter().find(|folder| folder.id == *id)
    }

    pub fn folder_subtree(&self, id: Uuid) -> Vec<Uuid> {
        let mut subtree = Vec::new();
        self.collect_folder_subtree(id, &mut subtree);
        subtree
    }

    /// Monotonic folder/assignment mutation counter (see field docs).
    pub fn folders_rev(&self) -> u64 {
        self.folders_rev
    }

    pub fn create_folder(&mut self, space_id: i64, name: &str) -> Option<Uuid> {
        self.create_folder_in(space_id, None, name)
    }

    pub fn create_folder_in(
        &mut self,
        space_id: i64,
        parent: Option<Uuid>,
        name: &str,
    ) -> Option<Uuid> {
        if !self.spaces.iter().any(|space| space.id == space_id) {
            return None;
        }
        if let Some(parent) = parent
            && !self
                .folders
                .iter()
                .any(|folder| folder.id == parent && folder.space_id == space_id)
        {
            return None;
        }
        let id = Uuid::new_v4();
        self.folders.push(Folder {
            id,
            space_id,
            parent,
            name: name.to_string(),
            folded: false,
            needs_push: true,
        });
        self.folders_changed();
        Some(id)
    }

    /// Materialize a folder from the space index doc: upsert without ever
    /// flagging `needs_push`, and never override a locally-flagged rename.
    pub fn adopt_remote_folder(
        &mut self,
        space_id: i64,
        id: Uuid,
        parent: Option<Uuid>,
        name: &str,
    ) {
        if let Some(folder) = self.folders.iter_mut().find(|folder| folder.id == id) {
            if !folder.needs_push && (folder.name != name || folder.parent != parent) {
                folder.name = name.to_string();
                folder.parent = parent.filter(|parent| *parent != id);
                self.folders_changed();
            }
            return;
        }
        if !self.spaces.iter().any(|space| space.id == space_id) {
            return;
        }
        self.folders.push(Folder {
            id,
            space_id,
            parent: parent.filter(|parent| *parent != id),
            name: name.to_string(),
            folded: false,
            needs_push: false,
        });
        self.folders_changed();
    }

    pub fn rename_folder(&mut self, id: &Uuid, name: &str) {
        if let Some(folder) = self.folders.iter_mut().find(|folder| folder.id == *id)
            && folder.name != name
        {
            folder.name = name.to_string();
            folder.needs_push = true;
            self.folders_changed();
        }
    }

    pub fn set_folder_folded(&mut self, id: &Uuid, folded: bool) {
        if let Some(folder) = self.folders.iter_mut().find(|folder| folder.id == *id)
            && folder.folded != folded
        {
            folder.folded = folded;
            if let Err(err) = self.persist_folders() {
                eprintln!("Could not persist folders: {err}");
            }
        }
    }

    /// The sync bridge acknowledged this folder's index entry.
    pub fn clear_folder_needs_push(&mut self, id: &Uuid) {
        if let Some(folder) = self.folders.iter_mut().find(|folder| folder.id == *id)
            && folder.needs_push
        {
            folder.needs_push = false;
            if let Err(err) = self.persist_folders() {
                eprintln!("Could not persist folders: {err}");
            }
        }
    }

    /// Delete a folder subtree; its notes fall back to the space root.
    pub fn delete_folder(&mut self, id: &Uuid) {
        let subtree = self.folder_subtree(*id);
        if subtree.is_empty() {
            return;
        }
        self.folders
            .retain(|folder| !subtree.iter().any(|id| *id == folder.id));
        for note in &mut self.notes {
            if note
                .folder
                .is_some_and(|folder| subtree.iter().any(|id| *id == folder))
            {
                note.folder = None;
                note.folder_needs_push = true;
                note.mark_dirty_preserve_updated();
            }
        }
        self.folders_changed();
    }

    /// Reparent a folder within its own space (or move it to the space root
    /// with `None`). Rejected if the target parent is the folder itself or sits
    /// inside the folder's own subtree (which would create a cycle), or lives in
    /// a different space.
    pub fn set_folder_parent(&mut self, id: &Uuid, parent: Option<Uuid>) {
        let Some(folder) = self.folder(id) else {
            return;
        };
        if folder.parent == parent {
            return;
        }
        let space = folder.space_id;
        if let Some(parent) = parent {
            if parent == *id {
                return;
            }
            let parent_ok = self
                .folders
                .iter()
                .any(|f| f.id == parent && f.space_id == space);
            if !parent_ok || self.folder_subtree(*id).contains(&parent) {
                return;
            }
        }
        if let Some(folder) = self.folders.iter_mut().find(|f| f.id == *id) {
            folder.parent = parent;
            folder.needs_push = true;
            self.folders_changed();
        }
    }

    /// Move a folder and its entire subtree (sub-folders and their notes) into
    /// another space. The dragged folder becomes a top-level folder there; its
    /// notes follow it as fresh docs in the destination, mirroring
    /// [`Self::move_note_to_space`].
    pub fn move_folder_to_space(&mut self, id: &Uuid, space_id: i64) {
        if !self.spaces.iter().any(|space| space.id == space_id) {
            return;
        }
        let Some(folder) = self.folder(id) else {
            return;
        };
        if folder.space_id == space_id {
            return;
        }
        let subtree = self.folder_subtree(*id);
        for folder in self.folders.iter_mut().filter(|f| subtree.contains(&f.id)) {
            folder.space_id = space_id;
            folder.needs_push = true;
            // The dragged folder roots into the destination; descendants keep
            // their (in-subtree) parent.
            if folder.id == *id {
                folder.parent = None;
            }
        }
        for note in self.notes.iter_mut() {
            if note.folder.is_some_and(|f| subtree.contains(&f)) {
                note.space_id = space_id;
                note.folder_needs_push = true;
                note.remote_doc = None;
                note.needs_push = true;
                note.mark_dirty_preserve_updated();
            }
        }
        self.folders_changed();
    }

    /// Move a note into a folder (or back to the space root with `None`).
    /// The folder must live in the note's space.
    pub fn set_note_folder(&mut self, note_id: &str, folder: Option<Uuid>) {
        self.set_note_folder_inner(note_id, folder, true);
    }

    pub fn move_note_to_space(&mut self, note_id: &str, space_id: i64) {
        if !self.spaces.iter().any(|space| space.id == space_id) {
            return;
        }
        if let Some(note) = self.notes.iter_mut().find(|note| note.id() == note_id)
            && note.space_id != space_id
        {
            note.space_id = space_id;
            note.folder = None;
            note.folder_needs_push = true;
            note.remote_doc = None;
            note.needs_push = true;
            note.mark_dirty_preserve_updated();
            self.folders_rev += 1;
        }
    }

    /// Like [`Self::set_note_folder`] for assignments arriving from the sync
    /// index — doesn't flag `folder_needs_push` (the index already has it).
    pub fn set_note_folder_from_remote(&mut self, note_id: &str, folder: Option<Uuid>) {
        self.set_note_folder_inner(note_id, folder, false);
    }

    fn set_note_folder_inner(&mut self, note_id: &str, folder: Option<Uuid>, local: bool) {
        let valid = match &folder {
            Some(id) => {
                let space = self
                    .notes
                    .iter()
                    .find(|n| n.id() == note_id)
                    .map(|n| n.space_id);
                self.folders
                    .iter()
                    .any(|f| f.id == *id && Some(f.space_id) == space)
            }
            None => true,
        };
        if !valid {
            return;
        }
        if let Some(note) = self.notes.iter_mut().find(|note| note.id() == note_id)
            && note.folder != folder
        {
            note.folder = folder;
            if local {
                note.folder_needs_push = true;
            }
            note.mark_dirty_preserve_updated();
            self.folders_rev += 1;
        }
    }

    /// The sync bridge acknowledged this note's index assignment entry.
    pub fn clear_note_folder_needs_push(&mut self, note_id: &str) {
        if let Some(note) = self.notes.iter_mut().find(|note| note.id() == note_id)
            && note.folder_needs_push
        {
            note.folder_needs_push = false;
            note.mark_dirty_preserve_updated();
        }
    }

    // -- blobs -----------------------------------------------------------------

    /// Bumped by every blob mutation; the sync bridge diffs against this.
    pub fn blobs_rev(&self) -> u64 {
        self.blobs_rev
    }

    pub fn blob(&self, id: &str) -> Option<&Blob> {
        self.blobs.iter().find(|blob| blob.id == id)
    }

    pub fn blobs_in_space(&self, space_id: i64) -> impl Iterator<Item = &Blob> {
        self.blobs
            .iter()
            .filter(move |blob| blob.space_id == space_id)
    }

    /// Resolve a `./blob/<name>` link to its blob within a space.
    pub fn blob_by_name(&self, space_id: i64, name: &str) -> Option<&Blob> {
        self.blobs
            .iter()
            .find(|blob| blob.space_id == space_id && blob.name == name)
    }

    /// Create a new blob from raw image bytes. Returns its id. The requested
    /// `name` is made unique within the space (so two pastes named `image.png`
    /// don't collide); `needs_push` is set so a synced space (re)uploads it.
    pub fn create_blob_in(
        &mut self,
        space_id: i64,
        name: &str,
        mime: ImageMime,
        bytes: Vec<u8>,
    ) -> String {
        let content_hash = content_hash(&bytes);
        // Space-scoped dedup: the same image pasted twice — by this device or by
        // any member whose index entry we have already adopted — reuses the
        // existing blob instead of storing and uploading a second copy. The
        // caller reads the name back from the id we return, so the new link just
        // points at the image that is already there.
        //
        // Scoped to the space deliberately, and done entirely client-side: the
        // relay must not be able to tell that two *different* spaces hold the
        // same file, since that would reveal content relationships between
        // groups that are meant to be unrelated to it. Matching on
        // `content_hash` locally leaks nothing — the relay only ever sees one
        // upload instead of two.
        //
        // What this does *not* catch: two members uploading the same file before
        // either has seen the other's index entry. Closing that needs
        // content-derived ids shared across members, which costs more than it
        // sounds (see `enkr/TODO.md`) — worth revisiting only if duplicate
        // uploads turn out to matter in practice.
        if let Some(existing) = self
            .blobs
            .iter_mut()
            .find(|blob| blob.space_id == space_id && blob.content_hash == content_hash)
        {
            let id = existing.id.clone();
            // A blob adopted from a peer's index carries metadata but no bytes
            // until it is fetched. We have those bytes right here, so fill them
            // in and skip the download too.
            if existing.bytes.is_empty() {
                existing.bytes = bytes;
                self.blobs_changed();
            }
            return id;
        }
        let name = self.unique_blob_name(space_id, name);
        let id = Uuid::new_v4().to_string();
        self.blobs.push(Blob {
            id: id.clone(),
            space_id,
            folder: None,
            name,
            mime,
            content_hash,
            key: BlobKey::generate().to_bytes(),
            bytes,
            needs_push: true,
        });
        self.blobs_changed();
        id
    }

    pub fn rename_blob(&mut self, id: &str, name: &str) {
        let Some(space_id) = self.blobs.iter().find(|b| b.id == id).map(|b| b.space_id) else {
            return;
        };
        let unique = self.unique_blob_name(space_id, name);
        let changed = match self.blobs.iter_mut().find(|blob| blob.id == id) {
            Some(blob) if blob.name != unique => {
                blob.name = unique;
                blob.needs_push = true;
                true
            }
            _ => false,
        };
        if changed {
            self.blobs_changed();
        }
    }

    pub fn set_blob_folder(&mut self, id: &str, folder: Option<Uuid>) {
        let changed = match self.blobs.iter_mut().find(|blob| blob.id == id) {
            Some(blob) if blob.folder != folder => {
                blob.folder = folder;
                blob.needs_push = true;
                true
            }
            _ => false,
        };
        if changed {
            self.blobs_changed();
        }
    }

    pub fn delete_blob(&mut self, id: &str) {
        let before = self.blobs.len();
        self.blobs.retain(|blob| blob.id != id);
        if self.blobs.len() != before {
            self.blobs_changed();
        }
    }

    /// The sync bridge acknowledged this blob's upload + index entry.
    pub fn clear_blob_needs_push(&mut self, id: &str) {
        let changed = match self.blobs.iter_mut().find(|blob| blob.id == id) {
            Some(blob) if blob.needs_push => {
                blob.needs_push = false;
                true
            }
            _ => false,
        };
        if changed {
            self.blobs_changed();
        }
    }

    /// Adopt (or update) a blob's metadata learned from a space index doc. The
    /// binary content is fetched separately; `set_blob_bytes_from_remote` fills
    /// it in. Returns true if the content still needs fetching.
    pub fn upsert_blob_meta_from_remote(
        &mut self,
        id: &str,
        space_id: i64,
        name: &str,
        mime: ImageMime,
        content_hash: [u8; 32],
        key: [u8; 32],
        folder: Option<Uuid>,
    ) -> bool {
        if let Some(blob) = self.blobs.iter_mut().find(|blob| blob.id == id) {
            let mut changed = false;
            if blob.name != name {
                blob.name = name.to_string();
                changed = true;
            }
            if blob.folder != folder {
                blob.folder = folder;
                changed = true;
            }
            if blob.key != key {
                blob.key = key;
                changed = true;
            }
            let needs_fetch = blob.content_hash != content_hash || blob.bytes.is_empty();
            if changed {
                self.blobs_changed();
            }
            return needs_fetch;
        }
        // New remote blob: metadata only, empty bytes until fetched. The key
        // comes from the index doc — without it the fetched ciphertext is
        // undecryptable, so it is as load-bearing as the content hash.
        self.blobs.push(Blob {
            id: id.to_string(),
            space_id,
            folder,
            name: name.to_string(),
            mime,
            content_hash,
            key,
            bytes: Vec::new(),
            needs_push: false,
        });
        self.blobs_changed();
        true
    }

    /// Fill in a remote blob's content once fetched. The bytes are verified
    /// against the blob's expected `content_hash` (carried in the authenticated
    /// index doc); a mismatch is rejected so a malicious relay can't swap
    /// content. Returns true if the content was accepted.
    pub fn set_blob_bytes_from_remote(&mut self, id: &str, bytes: Vec<u8>) -> bool {
        let accepted = match self.blobs.iter_mut().find(|blob| blob.id == id) {
            Some(blob) if blob.content_hash == content_hash(&bytes) => {
                blob.bytes = bytes;
                true
            }
            _ => false,
        };
        if accepted {
            self.blobs_changed();
        }
        accepted
    }

    /// Make `name` unique within a space by suffixing the stem (`a.png` →
    /// `a-1.png`) on collision.
    fn unique_blob_name(&self, space_id: i64, name: &str) -> String {
        let taken = |candidate: &str| {
            self.blobs
                .iter()
                .any(|blob| blob.space_id == space_id && blob.name == candidate)
        };
        if !taken(name) {
            return name.to_string();
        }
        let (stem, ext) = match name.rsplit_once('.') {
            Some((stem, ext)) => (stem.to_string(), format!(".{ext}")),
            None => (name.to_string(), String::new()),
        };
        let mut n = 1;
        loop {
            let candidate = format!("{stem}-{n}{ext}");
            if !taken(&candidate) {
                return candidate;
            }
            n += 1;
        }
    }

    fn blobs_changed(&mut self) {
        self.blobs_rev += 1;
        if let Err(err) = self.persist_blobs() {
            eprintln!("Could not persist blobs: {err}");
        }
    }

    fn persist_blobs(&self) -> NoteDbResult<()> {
        if let Some(store) = self.store.as_ref() {
            store.send(WriteOp::Blobs(self.blobs.clone()));
            if let Some(err) = store.take_error() {
                return Err(err.into());
            }
        }
        Ok(())
    }

    fn folders_changed(&mut self) {
        self.folders_rev += 1;
        if let Err(err) = self.persist_folders() {
            eprintln!("Could not persist folders: {err}");
        }
    }

    fn persist_folders(&self) -> NoteDbResult<()> {
        if let Some(store) = self.store.as_ref() {
            store.send(WriteOp::Folders(self.folders.clone()));
            if let Some(err) = store.take_error() {
                return Err(err.into());
            }
        }
        Ok(())
    }

    fn collect_folder_subtree(&self, id: Uuid, subtree: &mut Vec<Uuid>) {
        if subtree.iter().any(|seen| *seen == id) || self.folder(&id).is_none() {
            return;
        }
        subtree.push(id);
        for child in self
            .folders
            .iter()
            .filter(|folder| folder.parent == Some(id))
        {
            self.collect_folder_subtree(child.id, subtree);
        }
    }

    pub fn create_note(&mut self) -> String {
        self.create_note_in(self.default_space_id())
    }

    pub fn create_note_in(&mut self, space_id: i64) -> String {
        if self.spaces.is_empty() {
            self.create_space_named(DEFAULT_SPACE_NAME);
        }
        let space_id = if self.spaces.iter().any(|space| space.id == space_id) {
            space_id
        } else {
            self.default_space_id()
        };
        let id = loop {
            let candidate = format!("Untitled {}", self.next_note_number);
            self.next_note_number += 1;
            if !self.contains(&candidate) {
                break candidate;
            }
        };
        let mut note = Note::new(&id, "");
        note.space_id = space_id;
        note.mark_dirty();
        self.insert_note_ordered(note);
        id
    }

    /// Rename a note via its title (the editable top label). The title is the
    /// source of truth for the file name, so this also renames the backing file.
    pub fn set_note_title(&mut self, id: &str, title: &str) {
        let Some(note) = self.notes.iter_mut().find(|note| note.id() == id) else {
            return;
        };
        let before = note.file_path.clone();
        note.set_title(title);
        // The title *is* the file name, so this moves the note in the canonical
        // order. Only re-sort when it actually changed: this is called on every
        // keystroke in the title field.
        if note.file_path != before {
            self.resort_notes();
        }
    }

    pub fn delete_note(&mut self, id: &str) -> bool {
        let before = self.notes.len();
        self.notes.retain(|note| note.id() != id);
        if self.notes.len() == before {
            return false;
        }
        self.persist_note_deletions(vec![id.to_string()]);
        self.folders_rev += 1;
        true
    }

    /// Import a markdown tree into the default space (creating one if needed).
    pub fn import_folder(&mut self, root: impl AsRef<Path>) -> NoteDbResult<Vec<String>> {
        if self.spaces.is_empty() {
            self.create_space_named(DEFAULT_SPACE_NAME);
        }
        let space_id = self.default_space_id();
        self.import_folder_into(root, space_id)
    }

    /// Import a markdown tree into `space_id`. Re-importing the same path into
    /// the *same* space updates that note (idempotent); a note arriving in a
    /// different space is created fresh with a unique id, so importing one
    /// folder into several spaces never clobbers an earlier copy.
    ///
    /// Native only: there's no real filesystem to import from on wasm32 (see
    /// the `#[cfg(target_arch = "wasm32")]` stub below) — importing/exporting
    /// a local folder is a base-app scope cut for the web build, not (yet) a
    /// ported feature.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn import_folder_into(
        &mut self,
        root: impl AsRef<Path>,
        space_id: i64,
    ) -> NoteDbResult<Vec<String>> {
        let root = root.as_ref();
        let mut files = Vec::new();
        collect_importable_files(root, &mut files)?;

        let mut imported_ids = Vec::new();
        for path in files {
            // Only text we can hold in the (text-only) store: skip binary files,
            // even when a markdown doc references them.
            let Some(text) = read_text_file(&path)? else {
                continue;
            };
            let relative = normalized_relative_path(root, &path)?;
            let folder =
                self.ensure_folder_path(space_id, &folder_components_for_note_path(&relative));

            let existing_idx = self
                .notes
                .iter()
                .position(|note| note.space_id == space_id && note.file_path == relative);
            let id = match existing_idx {
                Some(idx) => self.notes[idx].id().to_string(),
                None => self.unique_note_id(&relative),
            };
            // `.md` files render as markdown; everything else keeps its extension
            // and stays plain source (see `Note::is_source_only`).
            let mut note = if is_markdown_path(&relative) {
                Note::from_imported_markdown(
                    id.clone(),
                    relative.clone(),
                    ParsedMarkdown::parse(&text),
                )
            } else {
                Note::from_imported_source(id.clone(), relative.clone(), &text)
            };
            note.space_id = space_id;
            note.folder = folder;
            note.folder_needs_push = folder.is_some();

            match existing_idx {
                Some(idx) => {
                    self.notes[idx].replace_with_note(note);
                    imported_ids.push(self.notes[idx].id().to_string());
                }
                None => {
                    self.insert_note_ordered(note);
                    imported_ids.push(id);
                }
            }
        }

        self.next_note_number = next_note_number(&self.notes);
        if !imported_ids.is_empty() {
            self.flush_dirty()?;
        }
        Ok(imported_ids)
    }

    /// wasm32 stub — see the native `import_folder_into` above.
    #[cfg(target_arch = "wasm32")]
    pub fn import_folder_into(
        &mut self,
        _root: impl AsRef<Path>,
        _space_id: i64,
    ) -> NoteDbResult<Vec<String>> {
        Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "importing a local folder isn't available on the web build",
        )))
    }

    /// A note id derived from `base` that no existing note uses (suffixing
    /// `#2`, `#3`, … on collision). Keeps the relative path readable while
    /// guaranteeing the primary-key uniqueness the store requires.
    fn unique_note_id(&self, base: &str) -> String {
        if !self.notes.iter().any(|note| note.id() == base) {
            return base.to_string();
        }
        (2..)
            .map(|n| format!("{base}#{n}"))
            .find(|candidate| !self.notes.iter().any(|note| note.id() == candidate))
            .expect("an unused note id always exists")
    }

    fn ensure_folder_path(&mut self, space_id: i64, components: &[String]) -> Option<Uuid> {
        let mut parent = None;
        for name in components {
            let existing = self
                .folders
                .iter()
                .find(|folder| {
                    folder.space_id == space_id
                        && folder.parent == parent
                        && folder.name.as_str() == name.as_str()
                })
                .map(|folder| folder.id);
            parent = match existing {
                Some(folder) => Some(folder),
                None => self.create_folder_in(space_id, parent, name),
            };
        }
        parent
    }

    /// Native only — see `import_folder_into`'s doc comment.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn export_folder(&mut self, root: impl AsRef<Path>) -> NoteDbResult<usize> {
        self.flush_dirty()?;
        let root = root.as_ref();
        fs::create_dir_all(root)?;

        let mut exported = 0;
        for note in &self.notes {
            let relative = safe_relative_path(&note.file_path);
            let path = root.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, note.to_markdown_file())?;
            exported += 1;
        }
        Ok(exported)
    }

    /// wasm32 stub — see `import_folder_into`'s wasm32 stub.
    #[cfg(target_arch = "wasm32")]
    pub fn export_folder(&mut self, _root: impl AsRef<Path>) -> NoteDbResult<usize> {
        Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "exporting to a local folder isn't available on the web build",
        )))
    }

    pub fn first_note_id(&self) -> Option<&str> {
        self.notes.first().map(Note::id)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.notes.iter().any(|note| note.id() == id)
    }

    pub fn note_mut(&mut self, id: &str) -> Option<&mut Note> {
        self.notes.iter_mut().find(|note| note.id() == id)
    }

    pub fn note(&self, id: &str) -> Option<&Note> {
        self.notes.iter().find(|note| note.id() == id)
    }

    pub fn note_id_by_remote_doc(&self, remote_doc: &Uuid) -> Option<&str> {
        self.notes
            .iter()
            .find(|note| note.remote_doc.as_ref() == Some(remote_doc))
            .map(Note::id)
    }

    /// Note ids of every note in a space, in display order.
    /// Insert a note at its canonical position — the same
    /// `(file_path, created, id)` order both load paths sort by.
    ///
    /// Appending would make the in-memory order depend on *when* a note was
    /// created or happened to arrive from sync, so the sidebar listed notes in
    /// one order during a session and a different one after a restart, and two
    /// clients holding the same space disagreed. Keeping the vector canonical
    /// by construction makes every consumer (sidebar, palettes, export) stable
    /// for free.
    fn insert_note_ordered(&mut self, note: Note) {
        let at = self.notes.partition_point(|existing| {
            (
                existing.file_path.as_str(),
                existing.created.as_str(),
                existing.id(),
            ) < (note.file_path.as_str(), note.created.as_str(), note.id())
        });
        self.notes.insert(at, note);
    }

    /// Restore the canonical order after a change to a sort key.
    ///
    /// `insert_note_ordered` keeps *insertions* in place, but a rename rewrites
    /// `file_path`, which is the primary key — and renaming is not rare, it is
    /// the first thing that happens to a new note. Without this the list is
    /// canonical right up until someone names something.
    fn resort_notes(&mut self) {
        self.notes.sort_by(|a, b| {
            a.file_path
                .cmp(&b.file_path)
                .then_with(|| a.created.cmp(&b.created))
                .then_with(|| a.id.cmp(&b.id))
        });
    }

    /// How many notes live in `space_id`.
    ///
    /// Counted from the store rather than from the app's per-frame summary
    /// buffer: that buffer is `mem::take`n during a frame and is empty outside
    /// one, so anything built off the render path (the space palette, which
    /// builds its rows when it opens) read every space as empty.
    pub fn note_count_in_space(&self, space_id: i64) -> usize {
        self.notes
            .iter()
            .filter(|note| note.space_id == space_id)
            .count()
    }

    pub fn note_ids_in_space(&self, space_id: i64) -> Vec<String> {
        self.notes
            .iter()
            .filter(|note| note.space_id == space_id)
            .map(|note| note.id().to_string())
            .collect()
    }

    /// Create an (empty) local note mirroring a remote sync doc; its content
    /// arrives through `apply_remote_update`. The local id is the doc uuid —
    /// unique and stable across devices.
    pub fn create_note_from_remote(&mut self, space_id: i64, remote_doc: Uuid) -> String {
        let id = remote_doc.to_string();
        if self.contains(&id) {
            return id;
        }
        let mut note = Note::new(&id, "");
        note.space_id = space_id;
        note.remote_doc = Some(remote_doc);
        note.mark_dirty_preserve_updated();
        self.insert_note_ordered(note);
        id
    }

    /// Allocating convenience wrapper over [`Self::summaries_into`], for cold
    /// paths (search corpus, tests). The per-frame UI path must use
    /// `summaries_into` with a retained buffer instead.
    pub fn summaries(&self) -> Vec<NoteSummary> {
        let mut out = Vec::new();
        self.summaries_into(&mut out);
        out
    }

    /// Refill `out` with one summary per note, reusing its existing `String`
    /// allocations. The sidebar rebuilds this every frame, so in the steady
    /// state (note count unchanged) this performs no allocation at all.
    pub fn summaries_into(&self, out: &mut Vec<NoteSummary>) {
        out.truncate(self.notes.len());
        let mut notes = self.notes.iter();
        for (slot, note) in out.iter_mut().zip(notes.by_ref()) {
            overwrite(&mut slot.id, note.id());
            overwrite(&mut slot.title, note.title());
            overwrite(&mut slot.file_path, &note.file_path);
            overwrite(&mut slot.preview, note.preview());
            overwrite(&mut slot.updated, &note.updated);
            slot.space_id = note.space_id;
            slot.folder = note.folder;
        }
        for note in notes {
            out.push(NoteSummary {
                id: note.id().to_string(),
                title: note.title().to_string(),
                file_path: note.file_path.clone(),
                space_id: note.space_id,
                folder: note.folder,
                preview: note.preview().to_string(),
                updated: note.updated.clone(),
            });
        }
    }

    /// The active note's title, borrowed — so the top bar doesn't have to build
    /// a whole summaries vector to draw one label.
    pub fn note_title(&self, id: &str) -> Option<&str> {
        self.note(id).map(|note| note.title())
    }

    /// The active note's last-edit timestamp, borrowed. See [`Self::note_title`].
    pub fn note_updated(&self, id: &str) -> Option<&str> {
        self.note(id).map(|note| note.updated.as_str())
    }

    fn persist_spaces(&self) -> NoteDbResult<()> {
        if let Some(store) = self.store.as_ref() {
            store.send(WriteOp::Spaces(self.spaces.clone()));
            if let Some(err) = store.take_error() {
                return Err(err.into());
            }
        }
        Ok(())
    }

    fn persist_note_deletions(&self, ids: Vec<String>) {
        if ids.is_empty() {
            return;
        }
        if let Some(store) = self.store.as_ref() {
            store.send(WriteOp::DeleteNotes(ids));
        }
    }

    pub fn flush_due(&mut self) -> NoteDbResult<()> {
        if self.notes.iter().any(|note| {
            note.dirty
                && note
                    .last_edit_at
                    .is_some_and(|at| at.elapsed() >= AUTOSAVE_DELAY)
        }) {
            self.flush_dirty()?;
        }
        Ok(())
    }

    pub fn flush_note(&mut self, id: &str) -> NoteDbResult<()> {
        let Some(store) = self.store.as_ref() else {
            self.mark_note_clean(id);
            return Ok(());
        };
        let Some(snapshot) = self
            .notes
            .iter()
            .find(|note| note.id() == id && note.dirty)
            .map(Note::snapshot)
        else {
            return Ok(());
        };

        store.send(WriteOp::Notes(vec![snapshot]));
        let error = store.take_error();
        self.mark_note_clean(id);
        match error {
            Some(err) => Err(err.into()),
            None => Ok(()),
        }
    }

    pub fn flush_dirty(&mut self) -> NoteDbResult<()> {
        let snapshots: Vec<_> = self
            .notes
            .iter()
            .filter(|note| note.dirty)
            .map(Note::snapshot)
            .collect();
        if snapshots.is_empty() {
            return Ok(());
        }

        let ids: Vec<String> = snapshots.iter().map(|s| s.id.clone()).collect();
        let mut error = None;
        if let Some(store) = self.store.as_ref() {
            store.send(WriteOp::Notes(snapshots));
            error = store.take_error();
        }

        for id in ids {
            self.mark_note_clean(&id);
        }
        match error {
            Some(err) => Err(err.into()),
            None => Ok(()),
        }
    }

    pub fn has_dirty_notes(&self) -> bool {
        self.notes.iter().any(|note| note.dirty)
    }

    fn mark_note_clean(&mut self, id: &str) {
        if let Some(note) = self.notes.iter_mut().find(|note| note.id() == id) {
            note.mark_clean();
        }
    }
}

impl Note {
    pub fn new(id: impl Into<String>, initial_text: impl AsRef<str>) -> Self {
        let id = id.into();
        let now = iso_timestamp_now();
        Self::new_with_metadata(
            id.clone(),
            default_note_file_path(&id),
            None,
            now.clone(),
            now,
            initial_text,
        )
    }

    fn new_with_metadata(
        id: impl Into<String>,
        file_path: impl Into<String>,
        frontmatter_title: Option<String>,
        created: impl Into<String>,
        updated: impl Into<String>,
        initial_text: impl AsRef<str>,
    ) -> Self {
        let doc = new_body_doc();
        let body = doc.get_or_insert_text("body");
        let note = Self {
            id: id.into(),
            file_path: normalize_note_file_path(file_path.into()),
            space_id: DEFAULT_SPACE_ID,
            frontmatter_title,
            created: created.into(),
            updated: updated.into(),
            folder: None,
            folder_needs_push: false,
            remote_doc: None,
            needs_push: false,
            doc,
            body,
            sync_observer: None,
            local_edit_clock: Arc::new(AtomicU64::new(0)),
            dirty: false,
            last_edit_at: None,
            title: String::new(),
            preview: String::new(),
        };
        if !initial_text.as_ref().is_empty() {
            note.body
                .insert(&mut note.doc.transact_mut(), 0, initial_text.as_ref());
        }
        let mut note = note;
        note.refresh_derived();
        note
    }

    #[allow(clippy::too_many_arguments)]
    fn from_yrs_state(
        id: impl Into<String>,
        file_path: impl Into<Option<String>>,
        space_id: Option<i64>,
        frontmatter_title: Option<String>,
        created: Option<String>,
        updated: Option<String>,
        folder: Option<Uuid>,
        folder_needs_push: bool,
        remote_doc: Option<Uuid>,
        needs_push: bool,
        state: &[u8],
    ) -> NoteDbResult<Self> {
        let id = id.into();
        let now = iso_timestamp_now();
        let doc = new_body_doc();
        if !state.is_empty() {
            let update = Update::decode_v1(state)?;
            doc.transact_mut().apply_update(update)?;
        }
        let body = doc.get_or_insert_text("body");
        let mut note = Self {
            file_path: normalize_note_file_path(
                file_path
                    .into()
                    .unwrap_or_else(|| default_note_file_path(&id)),
            ),
            id,
            space_id: space_id.unwrap_or(DEFAULT_SPACE_ID),
            frontmatter_title,
            created: created.unwrap_or_else(|| now.clone()),
            updated: updated.unwrap_or(now),
            folder,
            folder_needs_push,
            remote_doc,
            needs_push,
            doc,
            body,
            sync_observer: None,
            local_edit_clock: Arc::new(AtomicU64::new(0)),
            dirty: false,
            last_edit_at: None,
            title: String::new(),
            preview: String::new(),
        };
        note.refresh_derived();
        Ok(note)
    }

    fn from_imported_markdown(
        id: impl Into<String>,
        file_path: impl Into<String>,
        markdown: ParsedMarkdown,
    ) -> Self {
        let file_path = file_path.into();
        // The title comes from the file name (not the frontmatter or first line);
        // the file name is the source of truth for the title.
        let title = Some(title_from_file_path(&file_path));
        let now = iso_timestamp_now();
        let created = markdown.frontmatter.created.unwrap_or_else(|| now.clone());
        let updated = markdown.frontmatter.updated.unwrap_or(now);
        let mut note =
            Self::new_with_metadata(id, file_path, title, created, updated, markdown.body);
        note.mark_dirty_preserve_updated();
        note
    }

    /// A note imported from a non-markdown text file. Its body is the raw file
    /// content (no frontmatter parsing) and it keeps its original extension, so
    /// `is_source_only` reports true and it's shown as plain source. The title is
    /// taken from the file name (stored separately from the body).
    fn from_imported_source(
        id: impl Into<String>,
        file_path: impl Into<String>,
        text: impl AsRef<str>,
    ) -> Self {
        let file_path = file_path.into();
        let title = Some(title_from_file_path(&file_path));
        let now = iso_timestamp_now();
        let mut note = Self::new_with_metadata(id, file_path, title, now.clone(), now, text);
        note.mark_dirty_preserve_updated();
        note
    }

    fn replace_with_note(&mut self, mut note: Note) {
        note.id = self.id.clone();
        *self = note;
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn file_path(&self) -> &str {
        &self.file_path
    }

    /// True when this note came from a non-markdown text file (any other
    /// extension, or none). Such notes keep their extension and are shown as
    /// plain source — never parsed or rendered as markdown.
    pub fn is_source_only(&self) -> bool {
        !is_markdown_path(&self.file_path)
    }

    pub fn space_id(&self) -> i64 {
        self.space_id
    }

    /// Folder this note sits in (None = space root).
    pub fn folder(&self) -> Option<Uuid> {
        self.folder
    }

    /// True while the folder assignment awaits index-doc acknowledgement.
    pub fn folder_needs_push(&self) -> bool {
        self.folder_needs_push
    }

    pub fn remote_doc(&self) -> Option<Uuid> {
        self.remote_doc
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Map this note to a remote sync doc; persisted with the next flush.
    pub fn set_remote_doc(&mut self, remote_doc: Option<Uuid>) {
        if self.remote_doc != remote_doc {
            self.remote_doc = remote_doc;
            self.mark_dirty_preserve_updated();
        }
    }

    /// State vector of the local Yrs doc (for computing sync diffs).
    pub fn state_vector(&self) -> StateVector {
        self.doc.transact().state_vector()
    }

    /// Anchor a caret/selection char position into the CRDT. The anchor
    /// resolves to the same *logical* place even after concurrent edits
    /// shift the numeric index (Google-Docs-style caret stability). Left
    /// associated: text inserted exactly at the position lands after it.
    pub fn caret_anchor(&self, char_idx: usize) -> Option<StickyIndex> {
        let text = self.text();
        let utf16 = char_to_utf16(&text, char_idx);
        let txn = self.doc.transact();
        self.body.sticky_index(&txn, utf16 as u32, Assoc::Before)
    }

    /// Resolve an anchor back to a char position in the current text.
    pub fn caret_from_anchor(&self, anchor: &StickyIndex) -> Option<usize> {
        let utf16 = {
            let txn = self.doc.transact();
            anchor.get_offset(&txn)?.index as usize
        };
        let text = self.text();
        Some(utf16_to_char(&text, utf16))
    }

    /// Everything this note's doc holds that `since` doesn't — the update
    /// blob to hand to the sync engine.
    pub fn encode_update_since(&self, since: &StateVector) -> Vec<u8> {
        self.doc.transact().encode_state_as_update_v1(since)
    }

    /// Apply a remote (already-decrypted) Yrs update coming from the sync
    /// engine. Runs under [`REMOTE_ORIGIN`] so the sync observer doesn't echo
    /// it back; marks the note dirty so it persists and re-renders — without
    /// setting `needs_push` (the server already has this content).
    pub fn apply_remote_update(&mut self, update: &[u8]) -> NoteDbResult<()> {
        self.apply_remote_update_decoded(Update::decode_v1(update)?)
    }

    /// Like [`Self::apply_remote_update`] for an already-decoded update.
    pub fn apply_remote_update_decoded(&mut self, update: Update) -> NoteDbResult<()> {
        {
            let mut txn = self.doc.transact_mut_with(REMOTE_ORIGIN);
            txn.apply_update(update)?;
        }
        self.mark_dirty_remote();
        Ok(())
    }

    /// Apply a remote update and report where the author's caret most likely
    /// landed: the end of the last inserted run, or the point of a deletion.
    /// Returned as a [`StickyIndex`] on this replica so it tracks subsequent
    /// edits like any other presence anchor. `None` when the update didn't
    /// touch the body text (nothing to attribute a caret to). Used to place a
    /// remote collaborator's caret the instant their edit applies, instead of
    /// waiting for a trailing presence ping.
    pub fn apply_remote_update_tracking_caret(
        &mut self,
        update: Update,
    ) -> NoteDbResult<Option<StickyIndex>> {
        // `i64` cell: -1 = no text change seen, else the caret UTF-16 offset.
        // (The body doc indexes in UTF-16 units; deltas match — see `new_body_doc`.)
        let caret_utf16 = Arc::new(std::sync::atomic::AtomicI64::new(-1));
        let sink = caret_utf16.clone();
        let sub = self.body.observe(move |txn, event| {
            let mut index: u32 = 0;
            let mut caret: Option<u32> = None;
            for delta in event.delta(txn) {
                match delta {
                    Delta::Retain(len, _) => index += *len,
                    Delta::Deleted(_) => caret = Some(index),
                    Delta::Inserted(value, _) => {
                        let len = match value {
                            Out::Any(Any::String(s)) => s.encode_utf16().count() as u32,
                            // Embedded (non-string) content counts as one unit.
                            _ => 1,
                        };
                        index += len;
                        caret = Some(index);
                    }
                }
            }
            if let Some(utf16) = caret {
                sink.store(utf16 as i64, Ordering::Relaxed);
            }
        });
        {
            let mut txn = self.doc.transact_mut_with(REMOTE_ORIGIN);
            txn.apply_update(update)?;
        }
        drop(sub);
        self.mark_dirty_remote();
        let utf16 = caret_utf16.load(Ordering::Relaxed);
        if utf16 < 0 {
            return Ok(None);
        }
        let text = self.text();
        let char_idx = utf16_to_char(&text, utf16 as usize);
        Ok(self.caret_anchor(char_idx))
    }

    /// Attach the sync forwarder: every *locally-originated* update batch is
    /// handed to `forward` as Yrs v1 bytes (remote applies are skipped via
    /// their transaction origin). Replaces any previous forwarder.
    pub fn attach_sync_observer(&mut self, forward: impl Fn(Vec<u8>) + 'static) {
        let clock = self.local_edit_clock.clone();
        let observer = self
            .doc
            .observe_update_v1(move |txn, event| {
                let is_remote = txn
                    .origin()
                    .is_some_and(|origin| origin == &yrs::Origin::from(REMOTE_ORIGIN));
                if !is_remote {
                    clock.fetch_add(1, Ordering::Relaxed);
                    forward(event.update.clone());
                }
            })
            .expect("doc update observer");
        self.sync_observer = Some(observer);
    }

    /// Monotonic counter of *local* edits to this note's body (advanced by the
    /// sync observer). Unchanged across remote applies, so the UI can detect
    /// whether the caret moved because the user just typed.
    pub fn local_edit_clock(&self) -> u64 {
        self.local_edit_clock.load(Ordering::Relaxed)
    }

    pub fn detach_sync_observer(&mut self) {
        self.sync_observer = None;
    }

    pub fn needs_push(&self) -> bool {
        self.needs_push
    }

    /// Record (and schedule persistence of) the unacknowledged-content flag.
    pub fn set_needs_push(&mut self, needs_push: bool) {
        if self.needs_push != needs_push {
            self.needs_push = needs_push;
            self.mark_dirty_preserve_updated();
        }
    }

    pub fn text(&self) -> String {
        self.body.get_string(&self.doc.transact())
    }

    /// The note's title. It is an explicit field (the editable top label), never
    /// derived from the body. Legacy/empty titles fall back to the file name.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Recompute the cached `title`/`preview`. Called from the constructors and
    /// from the mutations that can change them: a content edit
    /// ([`Self::mark_dirty`], [`Self::mark_dirty_remote`]) or a retitle
    /// ([`Self::set_title`]). Metadata-only writes (folder, `remote_doc`,
    /// `needs_push`) go through `mark_dirty_preserve_updated` and deliberately
    /// skip this — they fire on every sync ack and must not materialize the body.
    fn refresh_derived(&mut self) {
        self.title = match self
            .frontmatter_title
            .as_ref()
            .filter(|title| !title.is_empty())
        {
            Some(title) => title.clone(),
            None => title_from_file_path(&self.file_path),
        };
        let text = self.text();
        let title = &self.title;
        self.preview = text
            .lines()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty())
            .map(strip_markdown_marks)
            .find(|line| !line.is_empty() && line != title)
            .unwrap_or_default();
    }

    /// Set the title and rename the backing file to match — the title is the
    /// source of truth for the file name. The folder prefix and extension are
    /// preserved, so a `.txt` source note stays `.txt` and stays in its folder.
    /// Empty titles are ignored so the file never loses its name.
    pub fn set_title(&mut self, title: &str) {
        let title = title.trim();
        if title.is_empty() || self.title == title {
            return;
        }
        self.frontmatter_title = Some(title.to_string());
        self.file_path = file_path_for_title(&self.file_path, title);
        self.refresh_derived();
        self.mark_dirty_preserve_updated();
    }

    /// First meaningful body line after the title, plain-text, for list previews.
    /// Cached — see [`Self::refresh_derived`].
    pub fn preview(&self) -> &str {
        &self.preview
    }

    pub fn insert_text(&mut self, index: usize, text: &str) {
        if text.is_empty() {
            return;
        }
        // The body doc indexes in UTF-16 units (see `new_body_doc`).
        let index = char_to_utf16(&self.text(), index);
        {
            let mut txn = self.doc.transact_mut();
            self.body.insert(&mut txn, index as u32, text);
        }
        self.mark_dirty();
    }

    pub fn delete_range(&mut self, range: (usize, usize)) {
        if range.0 >= range.1 {
            return;
        }
        let text = self.text();
        let start = char_to_utf16(&text, range.0);
        let end = char_to_utf16(&text, range.1);
        if start >= end {
            return;
        }
        {
            let mut txn = self.doc.transact_mut();
            self.body
                .remove_range(&mut txn, start as u32, (end - start) as u32);
        }
        self.mark_dirty();
    }

    fn snapshot(&self) -> NoteSnapshot {
        NoteSnapshot {
            id: self.id().to_string(),
            title: self.title().to_string(),
            file_path: self.file_path.clone(),
            space_id: self.space_id,
            frontmatter_title: self.frontmatter_title.clone(),
            created: self.created.clone(),
            updated: self.updated.clone(),
            folder: self.folder,
            folder_needs_push: self.folder_needs_push,
            remote_doc: self.remote_doc,
            needs_push: self.needs_push,
            yrs_state: self
                .doc
                .transact()
                .encode_state_as_update_v1(&StateVector::default()),
            updated_at: unix_timestamp(),
        }
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
        self.last_edit_at = Some(Instant::now());
        self.updated = iso_timestamp_now();
        // A local edit is, by definition, content the server doesn't have.
        self.needs_push = true;
        self.refresh_derived();
    }

    /// Like [`Self::mark_dirty`] for remote applies: persist + re-render, but
    /// the content came *from* the server so `needs_push` stays untouched.
    fn mark_dirty_remote(&mut self) {
        self.dirty = true;
        self.last_edit_at = Some(Instant::now());
        self.updated = iso_timestamp_now();
        self.refresh_derived();
    }

    fn mark_dirty_preserve_updated(&mut self) {
        self.dirty = true;
        self.last_edit_at = Some(Instant::now());
    }

    fn mark_clean(&mut self) {
        self.dirty = false;
        self.last_edit_at = None;
    }

    fn to_markdown_file(&self) -> String {
        // Source notes round-trip verbatim: no frontmatter, original extension.
        if self.is_source_only() {
            return self.text();
        }
        format!(
            "---\ntitle: \"{}\"\ncreated: {}\nupdated: {}\n---\n{}",
            escape_frontmatter_string(self.title()),
            self.created,
            self.updated,
            ensure_leading_body_newline(&self.text())
        )
    }
}

impl TextEditBuffer for Note {
    fn text(&self) -> String {
        Note::text(self)
    }

    fn insert_text(&mut self, index: usize, text: &str) {
        Note::insert_text(self, index, text);
    }

    fn delete_range(&mut self, range: (usize, usize)) {
        Note::delete_range(self, range);
    }
}

/// Char index -> UTF-16 code-unit offset. The body doc indexes every offset in
/// UTF-16 units (see [`new_body_doc`]), so all `char` positions the UI works in
/// must convert through here before touching the Yrs text.
fn char_to_utf16(text: &str, char_idx: usize) -> usize {
    text.chars().take(char_idx).map(char::len_utf16).sum()
}

/// UTF-16 code-unit offset -> char index (clamped to the nearest char boundary
/// at or before the offset).
fn utf16_to_char(text: &str, utf16_idx: usize) -> usize {
    let mut units = 0;
    for (chars, ch) in text.chars().enumerate() {
        if units >= utf16_idx {
            return chars;
        }
        units += ch.len_utf16();
    }
    text.chars().count()
}

/// Strip leading markdown heading/list markers and surrounding emphasis for a clean preview.
fn strip_markdown_marks(line: &str) -> String {
    let trimmed = line
        .trim_start_matches('#')
        .trim_start_matches(['-', '*', '>', ' ', '\t'])
        .trim();
    trimmed.trim_matches(['*', '_', '`']).trim().to_string()
}

/// Human-friendly title from a file path: the file stem with word separators
/// (`-`, `_`) turned into spaces. Used as the title for imported files and as
/// the fallback when a note has no explicit title yet.
fn title_from_file_path(file_path: &str) -> String {
    let stem = Path::new(file_path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("");
    let pretty = stem.replace(['-', '_'], " ");
    let pretty = pretty.trim();
    if pretty.is_empty() {
        "Untitled".to_string()
    } else {
        pretty.to_string()
    }
}

/// Rebuild a note's file path for a new title: spaces become `-`, the original
/// folder prefix and extension are kept (markdown defaults to `.md`).
fn file_path_for_title(current_path: &str, title: &str) -> String {
    let path = Path::new(current_path);
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("md");
    let stem = sanitize_path_segment(&title.trim().replace(' ', "-"));
    let file_name = format!("{stem}.{extension}");
    match path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        Some(parent) => normalize_note_file_path(format!("{}/{file_name}", parent.display())),
        None => normalize_note_file_path(file_name),
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct NoteSnapshot {
    id: String,
    title: String,
    file_path: String,
    space_id: i64,
    frontmatter_title: Option<String>,
    created: String,
    updated: String,
    folder: Option<Uuid>,
    folder_needs_push: bool,
    remote_doc: Option<Uuid>,
    needs_push: bool,
    yrs_state: Vec<u8>,
    updated_at: i64,
}

enum WriteOp {
    Notes(Vec<NoteSnapshot>),
    DeleteNotes(Vec<String>),
    Spaces(Vec<Space>),
    Folders(Vec<Folder>),
    Blobs(Vec<Blob>),
    Meta(String, String),
    Shutdown,
}

#[cfg(not(target_arch = "wasm32"))]
fn mime_to_i64(mime: ImageMime) -> i64 {
    mime as u8 as i64
}

#[cfg(not(target_arch = "wasm32"))]
fn mime_from_i64(value: i64) -> ImageMime {
    match value {
        2 => ImageMime::Jpeg,
        _ => ImageMime::Png,
    }
}

/// Handle to the dedicated persistence thread. All SQLite writes happen
/// there — the UI thread only enqueues fully-owned snapshots, so disk I/O
/// never runs inside a frame. Dropping the handle drains the queue and joins
/// (the write barrier for shutdown and for tests that reopen the database).
///
/// Native only — see `IndexedDbNoteStore`/its `NoteStoreHandle` further down
/// in this file for the wasm32 counterpart (same
/// name/API: `spawn`/`send`/`take_error`, driven by IndexedDB instead of
/// SQLite on a dedicated thread — `NoteDatabase`'s `finish_open` and every
/// other caller of `self.store` don't need to know which).
#[cfg(not(target_arch = "wasm32"))]
struct NoteStoreHandle {
    tx: std::sync::mpsc::Sender<WriteOp>,
    thread: Option<std::thread::JoinHandle<()>>,
    error: Arc<Mutex<Option<String>>>,
}

#[cfg(not(target_arch = "wasm32"))]
impl NoteStoreHandle {
    fn spawn(store: SqliteNoteStore) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<WriteOp>();
        let error: Arc<Mutex<Option<String>>> = Arc::default();
        let error_slot = error.clone();
        let thread = std::thread::Builder::new()
            .name("enkr-note-store".into())
            .spawn(move || {
                while let Ok(op) = rx.recv() {
                    let result = match op {
                        WriteOp::Notes(snapshots) => store.save_notes(&snapshots),
                        WriteOp::DeleteNotes(ids) => store.delete_notes(&ids),
                        WriteOp::Spaces(spaces) => store.save_spaces(&spaces),
                        WriteOp::Folders(folders) => store.save_folders(&folders),
                        WriteOp::Blobs(blobs) => store.save_blobs(&blobs),
                        WriteOp::Meta(key, value) => store.save_meta(&key, &value),
                        WriteOp::Shutdown => break,
                    };
                    if let Err(err) = result {
                        eprintln!("note store write failed: {err}");
                        *error_slot.lock().unwrap() = Some(err.to_string());
                    }
                }
            })
            .expect("spawn note store thread");
        Self {
            tx,
            thread: Some(thread),
            error,
        }
    }

    fn send(&self, op: WriteOp) {
        let _ = self.tx.send(op);
    }

    /// Last write error, if any (writes are asynchronous; callers poll).
    fn take_error(&self) -> Option<String> {
        self.error.lock().unwrap().take()
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for NoteStoreHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(WriteOp::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
struct SqliteNoteStore {
    conn: Connection,
}

#[cfg(not(target_arch = "wasm32"))]
impl SqliteNoteStore {
    fn open(path: impl AsRef<Path>) -> NoteDbResult<Self> {
        if let Some(parent) = path.as_ref().parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS notes (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                file_path TEXT,
                frontmatter_title TEXT,
                created TEXT,
                updated TEXT,
                yrs_state BLOB NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                deleted_at INTEGER
            );

            CREATE TABLE IF NOT EXISTS app_meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS spaces (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                position INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                remote_space_id BLOB,
                sync_server TEXT
            );

            CREATE TABLE IF NOT EXISTS folders (
                id TEXT PRIMARY KEY,
                space_id INTEGER NOT NULL,
                parent_id TEXT,
                name TEXT NOT NULL,
                position INTEGER NOT NULL,
                folded INTEGER NOT NULL DEFAULT 0,
                needs_push INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS blobs (
                id TEXT PRIMARY KEY,
                space_id INTEGER NOT NULL,
                folder_id TEXT,
                name TEXT NOT NULL,
                mime INTEGER NOT NULL,
                content_hash BLOB NOT NULL,
                key BLOB NOT NULL,
                bytes BLOB NOT NULL,
                needs_push INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL
            );
            ",
        )?;
        Self::migrate(&conn)?;
        Ok(Self { conn })
    }

    fn migrate(conn: &Connection) -> NoteDbResult<()> {
        if sqlite_table_exists(conn, "notebooks")? && sqlite_column_exists(conn, "notebooks", "id")?
        {
            conn.execute(
                "
                INSERT OR IGNORE INTO spaces (id, name, position, created_at)
                SELECT id, name, position, created_at FROM notebooks
                ",
                [],
            )?;
        }

        if !sqlite_column_exists(conn, "spaces", "id")? {
            conn.execute("DROP TABLE IF EXISTS spaces", [])?;
            conn.execute_batch(
                "
                CREATE TABLE spaces (
                    id INTEGER PRIMARY KEY,
                    name TEXT NOT NULL,
                    position INTEGER NOT NULL,
                    created_at INTEGER NOT NULL
                );
                ",
            )?;
        }

        for (column, ty) in [
            ("file_path", "TEXT"),
            ("frontmatter_title", "TEXT"),
            ("created", "TEXT"),
            ("updated", "TEXT"),
            ("space_id", "INTEGER REFERENCES spaces(id)"),
            ("remote_doc_id", "BLOB"),
            ("needs_push", "INTEGER"),
            ("folder_id", "TEXT"),
            ("folder_needs_push", "INTEGER"),
        ] {
            if !sqlite_column_exists(conn, "notes", column)? {
                conn.execute(&format!("ALTER TABLE notes ADD COLUMN {column} {ty}"), [])?;
            }
        }
        if sqlite_table_exists(conn, "folders")?
            && !sqlite_column_exists(conn, "folders", "parent_id")?
        {
            conn.execute("ALTER TABLE folders ADD COLUMN parent_id TEXT", [])?;
        }
        if sqlite_table_exists(conn, "folders")?
            && !sqlite_column_exists(conn, "folders", "folded")?
        {
            conn.execute(
                "ALTER TABLE folders ADD COLUMN folded INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }

        if !sqlite_column_exists(conn, "spaces", "remote_space_id")? {
            conn.execute("ALTER TABLE spaces ADD COLUMN remote_space_id BLOB", [])?;
        }
        if !sqlite_column_exists(conn, "spaces", "sync_server")? {
            conn.execute("ALTER TABLE spaces ADD COLUMN sync_server TEXT", [])?;
        }

        if sqlite_column_exists(conn, "notes", "notebook_id")? {
            conn.execute(
                "UPDATE notes SET space_id = notebook_id WHERE space_id IS NULL",
                [],
            )?;
        }
        Ok(())
    }

    fn load_spaces(&self) -> NoteDbResult<Vec<Space>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, remote_space_id, sync_server FROM spaces ORDER BY position ASC, id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Space {
                id: row.get(0)?,
                name: row.get(1)?,
                remote: row
                    .get::<_, Option<Vec<u8>>>(2)?
                    .and_then(|bytes| Uuid::from_slice(&bytes).ok()),
                server: row.get::<_, Option<String>>(3)?,
            })
        })?;
        let mut spaces = Vec::new();
        for row in rows {
            spaces.push(row?);
        }
        Ok(spaces)
    }

    fn save_spaces(&self, spaces: &[Space]) -> NoteDbResult<()> {
        let now = unix_timestamp();
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare(
                "
                INSERT INTO spaces (id, name, position, created_at, remote_space_id, sync_server)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                ON CONFLICT(id) DO UPDATE SET
                    name = excluded.name,
                    position = excluded.position,
                    remote_space_id = excluded.remote_space_id,
                    sync_server = excluded.sync_server
                ",
            )?;
            for (position, space) in spaces.iter().enumerate() {
                stmt.execute(params![
                    space.id,
                    space.name,
                    position as i64,
                    now,
                    space.remote.map(|uuid| uuid.as_bytes().to_vec()),
                    space.server,
                ])?;
            }
        }
        let mut existing = Vec::new();
        {
            let mut stmt = tx.prepare("SELECT id FROM spaces")?;
            let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
            for row in rows {
                existing.push(row?);
            }
        }
        for id in existing {
            if !spaces.iter().any(|space| space.id == id) {
                tx.execute("DELETE FROM spaces WHERE id = ?1", [id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn load_folders(&self) -> NoteDbResult<Vec<Folder>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, space_id, parent_id, name, folded, needs_push FROM folders ORDER BY position ASC, id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            let id: String = row.get(0)?;
            let space_id: i64 = row.get(1)?;
            let parent: Option<String> = row.get(2)?;
            let name: String = row.get(3)?;
            let folded: i64 = row.get(4)?;
            let needs_push: i64 = row.get(5)?;
            Ok((id, space_id, parent, name, folded, needs_push))
        })?;
        let mut folders = Vec::new();
        for row in rows {
            let (id, space_id, parent, name, folded, needs_push) = row?;
            let Ok(id) = Uuid::parse_str(&id) else {
                continue;
            };
            folders.push(Folder {
                id,
                space_id,
                parent: parent.and_then(|parent| Uuid::parse_str(&parent).ok()),
                name,
                folded: folded != 0,
                needs_push: needs_push != 0,
            });
        }
        Ok(folders)
    }

    /// Replace the whole folder set (small table; one transaction so a crash
    /// can't leave it half-written).
    fn save_folders(&self, folders: &[Folder]) -> NoteDbResult<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM folders", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO folders (id, space_id, parent_id, name, position, folded, needs_push)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for (position, folder) in folders.iter().enumerate() {
                stmt.execute(params![
                    folder.id.to_string(),
                    folder.space_id,
                    folder.parent.map(|uuid| uuid.to_string()),
                    folder.name,
                    position as i64,
                    folder.folded as i64,
                    folder.needs_push as i64,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn load_blobs(&self) -> NoteDbResult<Vec<Blob>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, space_id, folder_id, name, mime, content_hash, key, bytes, needs_push
             FROM blobs ORDER BY created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            let id: String = row.get(0)?;
            let space_id: i64 = row.get(1)?;
            let folder_id: Option<String> = row.get(2)?;
            let name: String = row.get(3)?;
            let mime: i64 = row.get(4)?;
            let content_hash: Vec<u8> = row.get(5)?;
            let key: Vec<u8> = row.get(6)?;
            let bytes: Vec<u8> = row.get(7)?;
            let needs_push: i64 = row.get(8)?;
            Ok((
                id,
                space_id,
                folder_id,
                name,
                mime,
                content_hash,
                key,
                bytes,
                needs_push,
            ))
        })?;
        let mut blobs = Vec::new();
        for row in rows {
            let (id, space_id, folder_id, name, mime, content_hash, key, bytes, needs_push) = row?;
            blobs.push(Blob {
                id,
                space_id,
                folder: folder_id.and_then(|f| Uuid::parse_str(&f).ok()),
                name,
                mime: mime_from_i64(mime),
                content_hash: content_hash.try_into().unwrap_or([0u8; 32]),
                key: key.try_into().unwrap_or([0u8; 32]),
                bytes,
                needs_push: needs_push != 0,
            });
        }
        Ok(blobs)
    }

    /// Upsert every blob and drop rows no longer present. Unlike folders (a
    /// tiny full-replace table) blobs can be large, so this avoids deleting +
    /// re-inserting the whole set on every change.
    fn save_blobs(&self, blobs: &[Blob]) -> NoteDbResult<()> {
        let now = unix_timestamp();
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO blobs (id, space_id, folder_id, name, mime, content_hash, key, bytes, needs_push, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(id) DO UPDATE SET
                     space_id = excluded.space_id,
                     folder_id = excluded.folder_id,
                     name = excluded.name,
                     mime = excluded.mime,
                     content_hash = excluded.content_hash,
                     key = excluded.key,
                     bytes = excluded.bytes,
                     needs_push = excluded.needs_push",
            )?;
            for blob in blobs {
                stmt.execute(params![
                    blob.id,
                    blob.space_id,
                    blob.folder.map(|f| f.to_string()),
                    blob.name,
                    mime_to_i64(blob.mime),
                    blob.content_hash.to_vec(),
                    blob.key.to_vec(),
                    blob.bytes,
                    blob.needs_push as i64,
                    now,
                ])?;
            }
        }
        let mut existing = Vec::new();
        {
            let mut stmt = tx.prepare("SELECT id FROM blobs")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            for row in rows {
                existing.push(row?);
            }
        }
        for id in existing {
            if !blobs.iter().any(|blob| blob.id == id) {
                tx.execute("DELETE FROM blobs WHERE id = ?1", params![id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn load_meta(&self) -> NoteDbResult<HashMap<String, String>> {
        let mut stmt = self.conn.prepare("SELECT key, value FROM app_meta")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        let mut meta = HashMap::new();
        for row in rows {
            let (key, value): (String, String) = row?;
            meta.insert(key, value);
        }
        Ok(meta)
    }

    fn save_meta(&self, key: &str, value: &str) -> NoteDbResult<()> {
        if value.is_empty() {
            self.conn
                .execute("DELETE FROM app_meta WHERE key = ?1", params![key])?;
        } else {
            self.conn.execute(
                "INSERT OR REPLACE INTO app_meta (key, value) VALUES (?1, ?2)",
                params![key, value],
            )?;
        }
        Ok(())
    }

    fn load_notes(&self) -> NoteDbResult<Vec<Note>> {
        let mut stmt = self.conn.prepare(
            "
            SELECT id, file_path, space_id, frontmatter_title, created, updated, remote_doc_id,
                   needs_push, folder_id, folder_needs_push, yrs_state
            FROM notes
            WHERE deleted_at IS NULL
            ORDER BY file_path ASC, created_at ASC, id ASC
            ",
        )?;
        let rows = stmt.query_map([], |row| {
            let id: String = row.get(0)?;
            let file_path: Option<String> = row.get(1)?;
            let space_id: Option<i64> = row.get(2)?;
            let frontmatter_title: Option<String> = row.get(3)?;
            let created: Option<String> = row.get(4)?;
            let updated: Option<String> = row.get(5)?;
            let remote_doc: Option<Vec<u8>> = row.get(6)?;
            let needs_push: Option<i64> = row.get(7)?;
            let folder_id: Option<String> = row.get(8)?;
            let folder_needs_push: Option<i64> = row.get(9)?;
            let yrs_state: Vec<u8> = row.get(10)?;
            Ok((
                id,
                file_path,
                space_id,
                frontmatter_title,
                created,
                updated,
                remote_doc,
                needs_push,
                folder_id,
                folder_needs_push,
                yrs_state,
            ))
        })?;

        let mut notes = Vec::new();
        for row in rows {
            let (
                id,
                file_path,
                space_id,
                frontmatter_title,
                created,
                updated,
                remote_doc,
                needs_push,
                folder_id,
                folder_needs_push,
                yrs_state,
            ) = row?;
            notes.push(Note::from_yrs_state(
                id,
                file_path,
                space_id,
                frontmatter_title,
                created,
                updated,
                folder_id.and_then(|id| Uuid::parse_str(&id).ok()),
                folder_needs_push.unwrap_or(0) != 0,
                remote_doc.and_then(|bytes| Uuid::from_slice(&bytes).ok()),
                needs_push.unwrap_or(0) != 0,
                &yrs_state,
            )?);
        }
        Ok(notes)
    }

    fn save_notes(&self, notes: &[NoteSnapshot]) -> NoteDbResult<()> {
        let mut stmt = self.conn.prepare(
            "
            INSERT INTO notes (
                id,
                title,
                file_path,
                space_id,
                frontmatter_title,
                created,
                updated,
                remote_doc_id,
                needs_push,
                folder_id,
                folder_needs_push,
                yrs_state,
                created_at,
                updated_at,
                deleted_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13, NULL)
            ON CONFLICT(id) DO UPDATE SET
                title = excluded.title,
                file_path = excluded.file_path,
                space_id = excluded.space_id,
                frontmatter_title = excluded.frontmatter_title,
                created = excluded.created,
                updated = excluded.updated,
                remote_doc_id = excluded.remote_doc_id,
                needs_push = excluded.needs_push,
                folder_id = excluded.folder_id,
                folder_needs_push = excluded.folder_needs_push,
                yrs_state = excluded.yrs_state,
                updated_at = excluded.updated_at,
                deleted_at = NULL
            ",
        )?;

        for note in notes {
            stmt.execute(params![
                &note.id,
                &note.title,
                &note.file_path,
                note.space_id,
                &note.frontmatter_title,
                &note.created,
                &note.updated,
                note.remote_doc.map(|uuid| uuid.as_bytes().to_vec()),
                note.needs_push as i64,
                note.folder.map(|uuid| uuid.to_string()),
                note.folder_needs_push as i64,
                &note.yrs_state,
                note.updated_at
            ])?;
        }
        Ok(())
    }

    fn delete_notes(&self, ids: &[String]) -> NoteDbResult<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let now = unix_timestamp();
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare(
                "
                UPDATE notes
                SET deleted_at = ?2,
                    updated_at = ?2,
                    space_id = NULL,
                    folder_id = NULL,
                    folder_needs_push = 0
                WHERE id = ?1
                ",
            )?;
            for id in ids {
                stmt.execute(params![id, now])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

fn next_note_number(notes: &[Note]) -> usize {
    notes
        .iter()
        .filter_map(|note| note.id().strip_prefix("Untitled "))
        .filter_map(|suffix| suffix.parse::<usize>().ok())
        .max()
        .map(|max| max + 1)
        .unwrap_or(1)
}

/// Meta map pre-marked as onboarded, for the in-memory constructors.
///
/// Kept beside `META_ONBOARDED`'s only other user (`app::state`) by name rather
/// than by import, because `note` must not depend on `app`.
fn onboarded_meta() -> HashMap<String, String> {
    HashMap::from([("onboarded".to_string(), "1".to_string())])
}

fn default_space() -> Space {
    Space {
        id: DEFAULT_SPACE_ID,
        name: DEFAULT_SPACE_NAME.to_string(),
        remote: None,
        server: None,
    }
}

fn normalize_default_space_name(spaces: &mut [Space]) {
    for space in spaces {
        if space.id == DEFAULT_SPACE_ID && LEGACY_DEFAULT_SPACE_NAMES.contains(&space.name.as_str())
        {
            space.name = DEFAULT_SPACE_NAME.to_string();
            break;
        }
    }
}

fn next_space_id(spaces: &[Space]) -> i64 {
    spaces
        .iter()
        .map(|space| space.id)
        .max()
        .map(|max| max + 1)
        .unwrap_or(DEFAULT_SPACE_ID)
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn iso_timestamp_now() -> String {
    unix_to_utc_iso(unix_timestamp())
}

fn unix_to_utc_iso(timestamp: i64) -> String {
    let days = timestamp.div_euclid(86_400);
    let seconds_of_day = timestamp.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}+00:00")
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096).div_euclid(365);
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2).div_euclid(153);
    let d = doy - (153 * mp + 2).div_euclid(5) + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    (year, m, d)
}

#[derive(Default)]
struct Frontmatter {
    title: Option<String>,
    created: Option<String>,
    updated: Option<String>,
}

struct ParsedMarkdown {
    frontmatter: Frontmatter,
    body: String,
}

impl ParsedMarkdown {
    fn parse(text: &str) -> Self {
        let Some((frontmatter, body)) = split_frontmatter(text) else {
            return Self {
                frontmatter: Frontmatter::default(),
                body: text.to_string(),
            };
        };

        Self {
            frontmatter: parse_frontmatter(frontmatter),
            body: body.to_string(),
        }
    }
}

fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let mut line_start = 0;
    let first_end = find_line_end(text, line_start)?;
    if text[line_start..first_end].trim_end_matches('\r') != "---" {
        return None;
    }
    line_start = next_line_start(text, first_end);

    let frontmatter_start = line_start;
    while line_start <= text.len() {
        let line_end = find_line_end(text, line_start).unwrap_or(text.len());
        if text[line_start..line_end].trim_end_matches('\r') == "---" {
            let body_start = next_line_start(text, line_end);
            return Some((&text[frontmatter_start..line_start], &text[body_start..]));
        }
        if line_end == text.len() {
            break;
        }
        line_start = next_line_start(text, line_end);
    }
    None
}

fn find_line_end(text: &str, start: usize) -> Option<usize> {
    text[start..].find('\n').map(|offset| start + offset)
}

fn next_line_start(text: &str, line_end: usize) -> usize {
    (line_end + 1).min(text.len())
}

fn parse_frontmatter(text: &str) -> Frontmatter {
    let mut frontmatter = Frontmatter::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = parse_frontmatter_value(value.trim());
        match key.trim() {
            "title" => frontmatter.title = Some(value),
            "created" => frontmatter.created = Some(value),
            "updated" => frontmatter.updated = Some(value),
            _ => {}
        }
    }
    frontmatter
}

fn parse_frontmatter_value(value: &str) -> String {
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        value[1..value.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\")
    } else {
        value.to_string()
    }
}

fn escape_frontmatter_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn ensure_leading_body_newline(body: &str) -> String {
    if body.is_empty() {
        "\n".to_string()
    } else if body.starts_with('\n') {
        body.to_string()
    } else {
        format!("\n{body}")
    }
}

/// Gather every regular file under `root` (any extension). Import filters out
/// non-text files later, once their contents can be inspected.
///
/// Native only — only called from `import_folder_into`'s native body.
#[cfg(not(target_arch = "wasm32"))]
fn collect_importable_files(root: &Path, files: &mut Vec<PathBuf>) -> NoteDbResult<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        // Skip hidden files and folders (`.git`, `.DS_Store`, dotfiles, …).
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with('.'))
        {
            continue;
        }
        let path = entry.path();
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            collect_importable_files(&path, files)?;
        } else if metadata.is_file() {
            files.push(path);
        }
    }
    files.sort();
    Ok(())
}

/// Read `path` as UTF-8 text, returning `None` for binary files. A NUL byte is
/// treated as the binary tell-tale; the note store holds text only, so binary
/// files (even ones referenced from a markdown doc) can't be imported.
///
/// Text is canonicalised to NFC so an accent like `é` is always the single
/// precomposed scalar (`U+00E9`), never the decomposed `e` + combining `´`
/// pair macOS tends to produce. The decomposed form splits one grapheme into
/// two cursor positions (the combining mark renders zero-width), which makes the
/// caret land "inside" the letter and jump across wrap boundaries.
///
/// Native only — only called from `import_folder_into`'s native body.
#[cfg(not(target_arch = "wasm32"))]
fn read_text_file(path: &Path) -> NoteDbResult<Option<String>> {
    let bytes = fs::read(path)?;
    if bytes.contains(&0) {
        return Ok(None);
    }
    Ok(String::from_utf8(bytes)
        .ok()
        .map(|text| text.nfc().collect()))
}

/// Whether a (relative) note path carries the markdown extension.
fn is_markdown_path(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
}

fn normalized_relative_path(root: &Path, path: &Path) -> NoteDbResult<String> {
    let relative = path.strip_prefix(root)?;
    Ok(normalize_note_file_path(
        relative.to_string_lossy().to_string(),
    ))
}

fn folder_components_for_note_path(path: &str) -> Vec<String> {
    let normalized = normalize_note_file_path(path.to_string());
    let mut parts: Vec<String> = normalized.split('/').map(str::to_string).collect();
    parts.pop();
    parts
}

fn default_note_file_path(id: &str) -> String {
    normalize_note_file_path(format!("{}.md", sanitize_path_segment(id)))
}

fn normalize_note_file_path(path: String) -> String {
    let mut parts = Vec::new();
    for component in Path::new(&path).components() {
        if let Component::Normal(part) = component {
            let part = part.to_string_lossy();
            if !part.is_empty() {
                parts.push(part.to_string());
            }
        }
    }

    if parts.is_empty() {
        parts.push("note.md".to_string());
    }

    // Default a bare name (no extension) to markdown, but preserve any explicit
    // extension so imported source files (`.txt`, `.rs`, …) keep theirs.
    if let Some(last) = parts.last_mut()
        && Path::new(last.as_str()).extension().is_none()
    {
        last.push_str(".md");
    }

    parts.join("/")
}

fn safe_relative_path(path: &str) -> PathBuf {
    let normalized = normalize_note_file_path(path.to_string());
    normalized.split('/').collect()
}

fn sanitize_path_segment(segment: &str) -> String {
    let sanitized: String = segment
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            ch if ch.is_control() => '-',
            ch => ch,
        })
        .collect();
    let sanitized = sanitized.trim().trim_matches('.');
    if sanitized.is_empty() {
        "note".to_string()
    } else {
        sanitized.to_string()
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn sqlite_table_exists(conn: &Connection, table: &str) -> NoteDbResult<bool> {
    let mut stmt =
        conn.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1 LIMIT 1")?;
    let mut rows = stmt.query([table])?;
    Ok(rows.next()?.is_some())
}

#[cfg(not(target_arch = "wasm32"))]
fn sqlite_column_exists(conn: &Connection, table: &str, column: &str) -> NoteDbResult<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// wasm32 persistence backend, standing in for `SqliteNoteStore` above.
/// Deliberately **not** a port of that struct's 5-table relational schema
/// (notes/spaces/folders/blobs/app_meta — foreign keys, position ordering, a
/// migration system) into IndexedDB's own object-store model; that's a lot
/// of moving parts to get right a second time. Instead the entire database
/// is one JSON document (every persisted type here already derives
/// `Serialize`/`Deserialize`) stored as a single record in one IndexedDB
/// object store. For a single-user, single-device notes app with a
/// realistically small/personal corpus, the write amplification of
/// re-serializing the whole thing on every mutation is a fine trade for a
/// much smaller, much easier to get right implementation — and unlike
/// `localStorage`, IndexedDB has no meaningful size ceiling and stores
/// binary blobs (image bytes, Yrs state) natively, not just strings.
#[cfg(target_arch = "wasm32")]
#[derive(Default)]
struct WasmState {
    notes: Vec<NoteSnapshot>,
    spaces: Vec<Space>,
    folders: Vec<Folder>,
    blobs: Vec<Blob>,
    meta: HashMap<String, String>,
}

/// One persisted row. Spaces and folders carry their list position because
/// nothing about them implies an order (native keeps a `position` column and
/// sorts by it); notes and blobs need no such field, since native orders
/// those by their own columns and `load_all` reproduces that here.
#[cfg(target_arch = "wasm32")]
#[derive(Serialize, Deserialize)]
enum WasmRecord {
    Note(NoteSnapshot),
    Space(usize, Space),
    Folder(usize, Folder),
    Blob(Blob),
    Meta(String, String),
}

/// A blob's row minus its `bytes`, which are stored alongside as a real
/// `Uint8Array` instead of being folded into the JSON (see
/// `IndexedDbNoteStore::apply`). Local to this store rather than
/// `#[serde(skip)]` on `Blob` itself, whose own derives are shared with the
/// sync layer.
#[cfg(target_arch = "wasm32")]
#[derive(Serialize, Deserialize)]
struct BlobMeta {
    id: String,
    space_id: i64,
    folder: Option<Uuid>,
    name: String,
    mime: ImageMime,
    content_hash: [u8; 32],
    /// The blob's own content key — as load-bearing as `content_hash`, since
    /// without it the stored ciphertext can't be reopened after a restart.
    key: [u8; 32],
    needs_push: bool,
}

#[cfg(target_arch = "wasm32")]
impl BlobMeta {
    fn split(blob: &Blob) -> (Self, &[u8]) {
        (
            Self {
                id: blob.id.clone(),
                space_id: blob.space_id,
                folder: blob.folder,
                name: blob.name.clone(),
                mime: blob.mime,
                content_hash: blob.content_hash,
                key: blob.key,
                needs_push: blob.needs_push,
            },
            &blob.bytes,
        )
    }

    fn join(self, bytes: Vec<u8>) -> Blob {
        Blob {
            id: self.id,
            space_id: self.space_id,
            folder: self.folder,
            name: self.name,
            mime: self.mime,
            content_hash: self.content_hash,
            key: self.key,
            bytes,
            needs_push: self.needs_push,
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl WasmRecord {
    /// The IndexedDB key this row is stored under. Prefixed per kind so one
    /// object store can hold them all and a `getAll` can be sorted back into
    /// per-kind lists by `load_all`.
    fn key(&self) -> String {
        match self {
            WasmRecord::Note(n) => format!("note:{}", n.id),
            WasmRecord::Space(_, s) => format!("space:{}", s.id),
            WasmRecord::Folder(_, f) => format!("folder:{}", f.id),
            WasmRecord::Blob(b) => format!("blob:{}", b.id),
            WasmRecord::Meta(k, _) => format!("meta:{k}"),
        }
    }
}

/// Bumped from 1, which stored the whole database as a single JSON record.
/// The upgrade drops that store and creates an empty one — deliberately not
/// a migration (see this crate's AGENTS.md), so anything a browser profile
/// held under the old scheme is discarded.
#[cfg(target_arch = "wasm32")]
const IDB_VERSION: u32 = 2;
#[cfg(target_arch = "wasm32")]
const IDB_STORE_NAME: &str = "records";
/// Field names of a blob row's two halves — see `IndexedDbNoteStore::apply`.
#[cfg(target_arch = "wasm32")]
const BLOB_META_FIELD: &str = "meta";
#[cfg(target_arch = "wasm32")]
const BLOB_BYTES_FIELD: &str = "bytes";

#[cfg(target_arch = "wasm32")]
fn js_err(e: wasm_bindgen::JsValue) -> Box<dyn Error> {
    format!("{e:?}").into()
}

/// Waits for an already-dispatched `IdbRequest` (a `get`/`put`, or the open
/// request itself) to settle, bridging its onsuccess/onerror callbacks into
/// a `oneshot` — the same shape `sync/transport/wasm.rs` uses for
/// `WebSocket`'s open event, since IndexedDB's request objects follow the
/// identical "dispatch now, callback later" pattern.
#[cfg(target_arch = "wasm32")]
async fn await_idb_request(req: &web_sys::IdbRequest) -> NoteDbResult<wasm_bindgen::JsValue> {
    use wasm_bindgen::JsCast;
    use wasm_bindgen::prelude::*;

    let (tx, rx) = tokio::sync::oneshot::channel::<Result<wasm_bindgen::JsValue, Box<dyn Error>>>();
    let tx = Rc::new(RefCell::new(Some(tx)));

    let on_success = {
        let tx = tx.clone();
        let req = req.clone();
        Closure::<dyn FnMut(web_sys::Event)>::new(move |_e: web_sys::Event| {
            if let Some(tx) = tx.borrow_mut().take() {
                let _ = tx.send(req.result().map_err(js_err));
            }
        })
    };
    req.set_onsuccess(Some(on_success.as_ref().unchecked_ref()));

    let on_error = {
        let tx = tx.clone();
        Closure::<dyn FnMut(web_sys::Event)>::new(move |_e: web_sys::Event| {
            if let Some(tx) = tx.borrow_mut().take() {
                let _ = tx.send(Err("IndexedDB request failed".into()));
            }
        })
    };
    req.set_onerror(Some(on_error.as_ref().unchecked_ref()));

    let result = rx
        .await
        .map_err(|_| -> Box<dyn Error> { "IndexedDB request cancelled".into() })?;
    drop(on_success);
    drop(on_error);
    result
}

#[cfg(target_arch = "wasm32")]
async fn open_idb(name: &str) -> NoteDbResult<web_sys::IdbDatabase> {
    use wasm_bindgen::JsCast;
    use wasm_bindgen::prelude::*;

    let window = web_sys::window().ok_or("no global `window`")?;
    let factory = window
        .indexed_db()
        .map_err(js_err)?
        .ok_or("IndexedDB is not available in this browser")?;
    let open_req = factory.open_with_u32(name, IDB_VERSION).map_err(js_err)?;

    let (tx, rx) = tokio::sync::oneshot::channel::<Result<web_sys::IdbDatabase, Box<dyn Error>>>();
    let tx = Rc::new(RefCell::new(Some(tx)));

    // Only fires the very first time this origin opens `name` (or after a
    // version bump) — exactly where IndexedDB requires object stores to be
    // created, unlike SQLite's `CREATE TABLE IF NOT EXISTS` running on every
    // open.
    let on_upgrade = {
        let open_req = open_req.clone();
        Closure::<dyn FnMut(web_sys::Event)>::new(move |_e: web_sys::Event| {
            if let Ok(result) = open_req.result() {
                let db: web_sys::IdbDatabase = result.unchecked_into();
                if !db.object_store_names().contains(IDB_STORE_NAME) {
                    let _ = db.create_object_store(IDB_STORE_NAME);
                }
            }
        })
    };
    open_req.set_onupgradeneeded(Some(on_upgrade.as_ref().unchecked_ref()));

    let on_success = {
        let tx = tx.clone();
        let open_req = open_req.clone();
        Closure::<dyn FnMut(web_sys::Event)>::new(move |_e: web_sys::Event| {
            if let Some(tx) = tx.borrow_mut().take() {
                let _ = tx.send(
                    open_req
                        .result()
                        .map(|v| v.unchecked_into())
                        .map_err(js_err),
                );
            }
        })
    };
    open_req.set_onsuccess(Some(on_success.as_ref().unchecked_ref()));

    let on_error = {
        let tx = tx.clone();
        Closure::<dyn FnMut(web_sys::Event)>::new(move |_e: web_sys::Event| {
            if let Some(tx) = tx.borrow_mut().take() {
                let _ = tx.send(Err("failed to open IndexedDB database".into()));
            }
        })
    };
    open_req.set_onerror(Some(on_error.as_ref().unchecked_ref()));

    let result = rx
        .await
        .map_err(|_| -> Box<dyn Error> { "IndexedDB open cancelled".into() })?;
    drop(on_upgrade);
    drop(on_success);
    drop(on_error);
    result
}

#[cfg(target_arch = "wasm32")]
struct IndexedDbNoteStore {
    db: web_sys::IdbDatabase,
}

#[cfg(target_arch = "wasm32")]
impl IndexedDbNoteStore {
    async fn open(name: &str) -> NoteDbResult<Self> {
        Ok(Self {
            db: open_idb(name).await?,
        })
    }

    /// Every stored row, in no particular order — `load_all` sorts them.
    async fn load_records(&self) -> NoteDbResult<Vec<WasmRecord>> {
        let tx = self
            .db
            .transaction_with_str(IDB_STORE_NAME)
            .map_err(js_err)?;
        let store = tx.object_store(IDB_STORE_NAME).map_err(js_err)?;
        let req = store.get_all().map_err(js_err)?;
        let value = await_idb_request(&req).await?;
        let array = js_sys::Array::from(&value);
        let mut out = Vec::with_capacity(array.length() as usize);
        for entry in array.iter() {
            // A row that no longer parses is skipped rather than failing the
            // whole load: one unreadable note should not make the rest of the
            // database inaccessible, which is exactly the failure mode the
            // old single-record layout had by construction.
            match entry.as_string() {
                // Everything except a blob is a plain JSON string.
                Some(json) => {
                    if let Ok(record) = serde_json::from_str::<WasmRecord>(&json) {
                        out.push(record);
                    }
                }
                // A blob: JSON metadata plus its bytes as a `Uint8Array`.
                None => {
                    let meta = js_sys::Reflect::get(
                        &entry,
                        &wasm_bindgen::JsValue::from_str(BLOB_META_FIELD),
                    )
                    .ok()
                    .and_then(|v| v.as_string())
                    .and_then(|json| serde_json::from_str::<BlobMeta>(&json).ok());
                    let bytes = js_sys::Reflect::get(
                        &entry,
                        &wasm_bindgen::JsValue::from_str(BLOB_BYTES_FIELD),
                    )
                    .ok()
                    .map(|v| js_sys::Uint8Array::new(&v).to_vec());
                    if let (Some(meta), Some(bytes)) = (meta, bytes) {
                        out.push(WasmRecord::Blob(meta.join(bytes)));
                    }
                }
            }
        }
        Ok(out)
    }

    /// Write `records` and delete `deleted_keys`, in one transaction — so a
    /// single `WriteOp` is still atomic even though it now touches several
    /// rows instead of rewriting one big one.
    async fn apply(&self, records: &[WasmRecord], deleted_keys: &[String]) -> NoteDbResult<()> {
        if records.is_empty() && deleted_keys.is_empty() {
            return Ok(());
        }
        let tx = self
            .db
            .transaction_with_str_and_mode(IDB_STORE_NAME, web_sys::IdbTransactionMode::Readwrite)
            .map_err(js_err)?;
        let store = tx.object_store(IDB_STORE_NAME).map_err(js_err)?;
        let mut last = None;
        for record in records {
            // A blob's image bytes are stored as a real `Uint8Array`
            // alongside its JSON metadata, not inside it: IndexedDB holds
            // binary natively, whereas `serde_json` has to render a
            // `Vec<u8>` as an array of decimal numbers — roughly 4x the
            // size, and paid twice (encode on write, parse on read) for
            // data that is never inspected, only handed back to the
            // browser. Everything else is small and scalar, so it stays a
            // plain JSON string.
            let value: wasm_bindgen::JsValue = match record {
                WasmRecord::Blob(blob) => {
                    let (meta, bytes) = BlobMeta::split(blob);
                    let object = js_sys::Object::new();
                    let meta_json = serde_json::to_string(&meta)?;
                    let _ = js_sys::Reflect::set(
                        &object,
                        &wasm_bindgen::JsValue::from_str(BLOB_META_FIELD),
                        &wasm_bindgen::JsValue::from_str(&meta_json),
                    );
                    // `Uint8Array::from` copies out of wasm memory, which is
                    // required: a view over it would dangle the moment the
                    // heap grew.
                    let _ = js_sys::Reflect::set(
                        &object,
                        &wasm_bindgen::JsValue::from_str(BLOB_BYTES_FIELD),
                        &js_sys::Uint8Array::from(bytes),
                    );
                    object.into()
                }
                other => wasm_bindgen::JsValue::from_str(&serde_json::to_string(other)?),
            };
            last = Some(
                store
                    .put_with_key(&value, &wasm_bindgen::JsValue::from_str(&record.key()))
                    .map_err(js_err)?,
            );
        }
        for key in deleted_keys {
            last = Some(
                store
                    .delete(&wasm_bindgen::JsValue::from_str(key))
                    .map_err(js_err)?,
            );
        }
        // Awaiting the last request is enough to know the transaction ran:
        // requests in one transaction complete in order.
        if let Some(req) = last {
            await_idb_request(&req).await?;
        }
        Ok(())
    }

    /// The `open`/`open_wasm` counterpart to `SqliteNoteStore::load_notes`
    /// etc. combined: `Note::from_yrs_state` (unchanged, shared with native)
    /// reconstructs each real `Note` — including its live `yrs::Doc` — from
    /// the plain scalar fields + raw update bytes a `NoteSnapshot` already
    /// carries.
    async fn load_all(
        &self,
    ) -> NoteDbResult<(
        Vec<Note>,
        Vec<Space>,
        Vec<Folder>,
        Vec<Blob>,
        HashMap<String, String>,
    )> {
        let doc = collect_state(self.load_records().await?);
        let mut notes = Vec::with_capacity(doc.notes.len());
        for snap in doc.notes {
            notes.push(Note::from_yrs_state(
                snap.id,
                Some(snap.file_path),
                Some(snap.space_id),
                snap.frontmatter_title,
                Some(snap.created),
                Some(snap.updated),
                snap.folder,
                snap.folder_needs_push,
                snap.remote_doc,
                snap.needs_push,
                &snap.yrs_state,
            )?);
        }
        Ok((notes, doc.spaces, doc.folders, doc.blobs, doc.meta))
    }
}

/// Sort loose rows back into the ordered lists the app expects, matching
/// native's own `ORDER BY` clauses: spaces and folders by their stored
/// position (then id), notes by `(file_path, created, id)`, blobs by id.
///
/// Ordering has to be reconstructed rather than inherited, because
/// IndexedDB returns rows in key order — unlike the old single-record
/// layout, which got the app's ordering for free by storing whole `Vec`s.
#[cfg(target_arch = "wasm32")]
fn collect_state(records: Vec<WasmRecord>) -> WasmState {
    let mut state = WasmState::default();
    let mut spaces: Vec<(usize, Space)> = Vec::new();
    let mut folders: Vec<(usize, Folder)> = Vec::new();
    for record in records {
        match record {
            WasmRecord::Note(n) => state.notes.push(n),
            WasmRecord::Space(pos, s) => spaces.push((pos, s)),
            WasmRecord::Folder(pos, f) => folders.push((pos, f)),
            WasmRecord::Blob(b) => state.blobs.push(b),
            WasmRecord::Meta(k, v) => {
                state.meta.insert(k, v);
            }
        }
    }
    spaces.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.id.cmp(&b.1.id)));
    folders.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.id.cmp(&b.1.id)));
    state.spaces = spaces.into_iter().map(|(_, s)| s).collect();
    state.folders = folders.into_iter().map(|(_, f)| f).collect();
    state.notes.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then_with(|| a.created.cmp(&b.created))
            .then_with(|| a.id.cmp(&b.id))
    });
    state.blobs.sort_by(|a, b| a.id.cmp(&b.id));
    state
}

/// Real-browser check that the per-record IndexedDB layout both persists
/// across a reload *and* actually stores one row per item, rather than the
/// single whole-database record it replaced.
///
/// CDP-only, and against the real web app rather than the fixture harness:
/// the harness seeds `NoteDatabase::demo()`, which has no store behind it,
/// so nothing it writes reaches IndexedDB at all.
#[cfg(all(test, feature = "cdp", not(target_arch = "wasm32")))]
mod wasm_store_tests {
    use mae::testkit::UiDriver;

    /// Every key in the object store, as JSON.
    const READ_KEYS: &str = "new Promise(done => { \
         const open = indexedDB.open('enkr'); \
         open.onerror = () => done('OPEN FAILED'); \
         open.onsuccess = () => { \
           const db = open.result; \
           if (!db.objectStoreNames.contains('records')) return done('NO STORE'); \
           const req = db.transaction('records').objectStore('records').getAllKeys(); \
           req.onsuccess = () => done(JSON.stringify(req.result)); \
           req.onerror = () => done('READ FAILED'); \
         }; \
       })";

    /// A blob's image bytes are stored as real binary, and come back
    /// byte-identical across a reload.
    ///
    /// Worth asserting on the stored *shape*, not just the round trip: the
    /// bytes went through JSON before (a `Vec<u8>` renders as an array of
    /// decimal numbers, ~4x the size, parsed back on every load), and a
    /// regression there would still round-trip correctly while quietly
    /// costing all of that again.
    #[test]
    #[ignore = "needs enkr/www/build.sh run first, plus a local chromium and python3 on PATH"]
    fn blob_bytes_are_stored_as_binary_and_survive_a_reload() {
        const PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        /// The stored blob row's shape: what `bytes` actually is, and how
        /// long it is.
        const READ_BLOB: &str = "new Promise(done => { \
             const open = indexedDB.open('enkr'); \
             open.onerror = () => done('OPEN FAILED'); \
             open.onsuccess = () => { \
               const db = open.result; \
               if (!db.objectStoreNames.contains('records')) return done('NO STORE'); \
               const tx = db.transaction('records').objectStore('records'); \
               const keys = tx.getAllKeys(); \
               keys.onsuccess = () => { \
                 const key = keys.result.find(k => String(k).startsWith('blob:')); \
                 if (!key) return done('NO BLOB ROW'); \
                 const row = db.transaction('records').objectStore('records').get(key); \
                 row.onsuccess = () => { \
                   const v = row.result; \
                   const b = v && v.bytes; \
                   done(JSON.stringify({ \
                     kind: Object.prototype.toString.call(b), \
                     len: b ? b.length : -1, \
                     head: b ? Array.from(b.slice(0, 4)) : [] \
                   })); \
                 }; \
               }; \
             }; \
           })";

        let mut driver = crate::testkit_support::launch_web_app();
        driver.click("Welcome");

        // Paste a real PNG so a blob row actually exists.
        let dispatched = driver.debug_eval(&format!(
            "(function(){{
                const host = document.querySelector('[contenteditable=\"true\"]') || document.querySelector('textarea');
                if (!host) return 'NO EDITOR';
                const bin = atob('{PNG_B64}');
                const arr = new Uint8Array(bin.length);
                for (let i = 0; i < bin.length; i++) arr[i] = bin.charCodeAt(i);
                const dt = new DataTransfer();
                dt.items.add(new File([arr], 'x.png', {{type: 'image/png'}}));
                host.dispatchEvent(new ClipboardEvent('paste', {{
                    clipboardData: dt, bubbles: true, cancelable: true
                }}));
                return 'ok';
            }})()"
        ));
        assert_eq!(
            dispatched.as_str(),
            Some("ok"),
            "could not dispatch the paste"
        );

        let mut stored = String::new();
        for _ in 0..40 {
            driver.debug_eval(
                "new Promise(d => requestAnimationFrame(() => requestAnimationFrame(() => d(0))))",
            );
            stored = driver
                .debug_eval(READ_BLOB)
                .as_str()
                .unwrap_or_default()
                .to_string();
            if stored.contains("Uint8Array") {
                break;
            }
        }
        assert!(
            stored.contains("[object Uint8Array]"),
            "blob bytes should be stored as binary, not JSON — got {stored}"
        );
        // A PNG's first four bytes; proves the *content* survived, not just
        // the type.
        assert!(
            stored.contains("[137,80,78,71]"),
            "the stored bytes should be the PNG that was pasted — got {stored}"
        );

        driver.reload();
        let after = driver.debug_eval(READ_BLOB);
        assert_eq!(
            after.as_str(),
            Some(stored.as_str()),
            "the blob row should be unchanged by a reload"
        );
    }

    #[test]
    #[ignore = "needs enkr/www/build.sh run first, plus a local chromium and python3 on PATH"]
    fn records_are_stored_one_per_item_and_survive_a_reload() {
        let mut driver = crate::testkit_support::launch_web_app();

        // Type into the note so the store definitely has content to write.
        driver.click("Welcome");
        driver.type_text("PERSISTED");

        // Give the writer task a moment to flush to IndexedDB.
        for _ in 0..30 {
            driver.debug_eval(
                "new Promise(d => requestAnimationFrame(() => requestAnimationFrame(() => d(0))))",
            );
        }

        let keys = driver.debug_eval(READ_KEYS);
        let keys = keys
            .as_str()
            .expect("could not read the object store's keys");
        assert!(
            keys.contains("note:"),
            "notes should each be their own row — got {keys}"
        );
        assert!(
            !keys.contains("\"doc\""),
            "the single whole-database record should be gone — got {keys}"
        );
        // A fresh profile starts with the single built-in "Welcome" note,
        // so this is about the *shape* (a row per note, a row per space),
        // not the count.
        let count = keys.matches("note:").count();
        assert!(
            count >= 1,
            "expected at least the built-in note's own row — got {keys}"
        );
        assert!(
            keys.contains("space:"),
            "spaces get their own rows too — got {keys}"
        );

        driver.reload();
        let after = driver.debug_eval(READ_KEYS);
        assert_eq!(
            after.as_str().map(|s| s.matches("note:").count()),
            Some(count),
            "the same per-note rows should still be there after a reload"
        );
    }
}

/// wasm32 counterpart to native's `NoteStoreHandle` above — same name and
/// API (`spawn`/`send`/`take_error`), so `NoteDatabase` needs no `#[cfg]`s
/// beyond this pair of definitions. No `Drop`/thread-join: there's nothing
/// to synchronously wait for on wasm32 (see `sync/thread.rs` for the same
/// reasoning applied to the sync engine) — the spawned task just stops
/// itself once it sees `WriteOp::Shutdown` or the channel closes, on a
/// later microtask tick rather than before the handle is dropped.
#[cfg(target_arch = "wasm32")]
struct NoteStoreHandle {
    tx: tokio::sync::mpsc::UnboundedSender<WriteOp>,
    error: Rc<RefCell<Option<String>>>,
}

#[cfg(target_arch = "wasm32")]
impl NoteStoreHandle {
    fn spawn(store: IndexedDbNoteStore) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WriteOp>();
        let error: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let error_slot = error.clone();
        wasm_bindgen_futures::spawn_local(async move {
            // Re-derived from the same IndexedDB read `open_wasm` already
            // did, rather than threaded through — this task starts from
            // nothing but `store` on purpose, mirroring how the native
            // thread only ever receives `SqliteNoteStore` (not the already-
            // loaded rows) and re-reads nothing (SQLite is a single
            // connection there); here it costs one extra IndexedDB read at
            // startup for a much simpler handoff.
            let mut state = match store.load_records().await {
                Ok(records) => collect_state(records),
                Err(err) => {
                    *error_slot.borrow_mut() = Some(err.to_string());
                    return;
                }
            };
            while let Some(op) = rx.recv().await {
                // Each op writes only the rows it actually changed. This is
                // the whole point of the per-record layout: the previous
                // single-record one re-serialized every note, space, folder
                // *and blob* on every keystroke, so a note edit paid for the
                // full corpus — image bytes very much included.
                let mut records: Vec<WasmRecord> = Vec::new();
                let mut deleted: Vec<String> = Vec::new();
                match op {
                    WriteOp::Notes(snapshots) => {
                        for snap in snapshots {
                            match state.notes.iter_mut().find(|n| n.id == snap.id) {
                                Some(existing) => *existing = snap.clone(),
                                None => state.notes.push(snap.clone()),
                            }
                            records.push(WasmRecord::Note(snap));
                        }
                    }
                    WriteOp::DeleteNotes(ids) => {
                        state.notes.retain(|n| !ids.contains(&n.id));
                        deleted.extend(ids.iter().map(|id| format!("note:{id}")));
                    }
                    // Whole-list ops: the sender always passes the complete
                    // collection, so "what changed" has to be derived by
                    // comparing against what is already stored — including
                    // position, since that is part of the row.
                    WriteOp::Spaces(spaces) => {
                        for (pos, space) in spaces.iter().enumerate() {
                            if state.spaces.get(pos) != Some(space) {
                                records.push(WasmRecord::Space(pos, space.clone()));
                            }
                        }
                        for gone in state
                            .spaces
                            .iter()
                            .filter(|old| !spaces.iter().any(|s| s.id == old.id))
                        {
                            deleted.push(format!("space:{}", gone.id));
                        }
                        state.spaces = spaces;
                    }
                    WriteOp::Folders(folders) => {
                        for (pos, folder) in folders.iter().enumerate() {
                            if state.folders.get(pos) != Some(folder) {
                                records.push(WasmRecord::Folder(pos, folder.clone()));
                            }
                        }
                        for gone in state
                            .folders
                            .iter()
                            .filter(|old| !folders.iter().any(|f| f.id == old.id))
                        {
                            deleted.push(format!("folder:{}", gone.id));
                        }
                        state.folders = folders;
                    }
                    WriteOp::Blobs(blobs) => {
                        for blob in &blobs {
                            if state.blobs.iter().find(|b| b.id == blob.id) != Some(blob) {
                                records.push(WasmRecord::Blob(blob.clone()));
                            }
                        }
                        for gone in state
                            .blobs
                            .iter()
                            .filter(|old| !blobs.iter().any(|b| b.id == old.id))
                        {
                            deleted.push(format!("blob:{}", gone.id));
                        }
                        state.blobs = blobs;
                    }
                    WriteOp::Meta(key, value) => {
                        if value.is_empty() {
                            state.meta.remove(&key);
                            deleted.push(format!("meta:{key}"));
                        } else {
                            state.meta.insert(key.clone(), value.clone());
                            records.push(WasmRecord::Meta(key, value));
                        }
                    }
                    WriteOp::Shutdown => break,
                }
                if let Err(err) = store.apply(&records, &deleted).await {
                    *error_slot.borrow_mut() = Some(err.to_string());
                }
            }
        });
        Self { tx, error }
    }

    fn send(&self, op: WriteOp) {
        let _ = self.tx.send(op);
    }

    fn take_error(&self) -> Option<String> {
        self.error.borrow_mut().take()
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_SPACE_ID, DEFAULT_SPACE_NAME, Note, NoteDatabase, unix_timestamp};
    use rusqlite::{Connection, params};
    use uuid::Uuid;

    /// `title`/`preview` are cached rather than derived on read (deriving the
    /// preview materializes the whole Yrs body, and the sidebar asks once per
    /// note per frame). These pin the invalidation: every mutation that can
    /// change either must refresh the cache.
    #[test]
    fn preview_and_title_track_local_edits() {
        let mut note = Note::new("n", "# Heading\nfirst line\n");
        // The title is the file name, never the body; the preview is the first
        // body line with its markdown marks stripped.
        assert_eq!(note.title(), "n");
        assert_eq!(note.preview(), "Heading");

        // Insert ahead of it: the preview follows the new first body line.
        note.insert_text(0, "brand new\n");
        assert_eq!(note.preview(), "brand new");

        // Delete it again: back to the original.
        note.delete_range((0, 10));
        assert_eq!(note.preview(), "Heading");

        // A retitle refreshes the title *and* the preview, since the preview
        // deliberately skips a body line that merely repeats the title.
        note.set_title("Heading");
        assert_eq!(note.title(), "Heading");
        assert_eq!(note.preview(), "first line");
    }

    #[test]
    fn preview_tracks_remote_updates() {
        let mut local = Note::new("n", "hello\n");
        // A true replica, not an independent doc: two concurrent inserts at
        // index 0 would merge in a CRDT-defined order and make this ambiguous.
        let mut remote = Note::new("n", "");
        remote
            .apply_remote_update(&local.encode_update_since(&Default::default()))
            .unwrap();
        remote.insert_text(0, "remote wrote this\n");
        let update = remote.encode_update_since(&local.state_vector());

        local.apply_remote_update(&update).unwrap();
        assert_eq!(local.text(), "remote wrote this\nhello\n");
        assert_eq!(local.preview(), "remote wrote this");
    }

    /// The per-frame path refills a retained buffer; it must produce exactly
    /// what the allocating wrapper does, whether the buffer is short, exact, or
    /// long relative to the note count.
    #[test]
    fn summaries_into_matches_summaries_and_reuses_the_buffer() {
        let mut db = NoteDatabase::new_in_memory();
        db.create_note();
        db.create_note();
        let expected = db.summaries();

        let mut buf = Vec::new();
        db.summaries_into(&mut buf); // grow from empty
        assert_eq!(buf, expected);
        db.summaries_into(&mut buf); // steady state: same length, no growth
        assert_eq!(buf, expected);

        // Shrinking: a stale trailing entry must not survive the refill.
        db.delete_note(&expected[2].id);
        let after_delete = db.summaries();
        db.summaries_into(&mut buf);
        assert_eq!(buf, after_delete);
    }

    /// Repro: the synced caret guard anchors the caret as a Yrs `StickyIndex`
    /// every frame and restores it. After MANY small incremental edits (typing
    /// char by char) around a multi-byte char, `caret_anchor(idx)` must resolve
    /// back to the same `idx`; otherwise the guard shifts the caret each frame.
    #[test]
    fn caret_anchor_round_trips_after_incremental_multibyte_edits() {
        // Several typing patterns that fragment the Yrs item structure around a
        // multi-byte char. A non-end anomaly that resolves to a *different*
        // position is the reported caret-jump bug.
        let patterns: &[(&str, &dyn Fn() -> Note)] = &[
            ("paste line then type next line", &|| {
                let mut n = Note::new("n", "");
                n.insert_text(0, "ça\n");
                let mut at = n.text().chars().count();
                for ch in "hello".chars() {
                    n.insert_text(at, &ch.to_string());
                    at += 1;
                }
                n
            }),
            ("type the multibyte line char by char", &|| {
                let mut n = Note::new("n", "");
                for (i, ch) in "si je ça saute".chars().enumerate() {
                    n.insert_text(i, &ch.to_string());
                }
                n
            }),
            ("prepend lines above a pasted multibyte line", &|| {
                let mut n = Note::new("n", "");
                n.insert_text(0, "si je ça saute\n");
                n.insert_text(0, "\n"); // add newline above
                n.insert_text(0, "\n"); // add another newline above
                n
            }),
            ("insert chars before the multibyte char", &|| {
                let mut n = Note::new("n", "");
                n.insert_text(0, "ça\n");
                // type "abc" one char at a time at the very start (fragmenting).
                for ch in "abc".chars() {
                    n.insert_text(0, &ch.to_string());
                }
                n
            }),
        ];

        let mut failures = Vec::new();
        for (label, make) in patterns {
            let note = make();
            let text = note.text();
            let char_len = text.chars().count();
            for idx in 0..=char_len {
                let resolved = note
                    .caret_anchor(idx)
                    .and_then(|a| note.caret_from_anchor(&a));
                if resolved != Some(idx) {
                    failures.push(format!(
                        "[{label}] text={text:?} caret {idx} -> {resolved:?}"
                    ));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "caret anchor anomalies:\n{}",
            failures.join("\n")
        );
    }

    // Pseudo-random "type chars at moving caret positions" sequences, including
    // multi-byte chars, to find a fragmented Yrs item structure where the caret
    // anchor round-trip is not identity (which makes the synced guard jump it).
    #[test]
    fn caret_anchor_round_trip_fuzz_multibyte() {
        let alphabet = ['a', 'b', 'ç', '\n', 'é', 'x', ' ', '👍'];
        let mut seed: u64 = 0x1234_5678_9abc_def0;
        let mut rng = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        let mut failures = Vec::new();
        'outer: for trial in 0..400 {
            let mut note = Note::new("n", "");
            for _ in 0..30 {
                let len = note.text().chars().count();
                let pos = (rng() as usize) % (len + 1);
                let ch = alphabet[(rng() as usize) % alphabet.len()];
                note.insert_text(pos, &ch.to_string());
            }
            let text = note.text();
            let char_len = text.chars().count();
            for idx in 0..=char_len {
                let resolved = note
                    .caret_anchor(idx)
                    .and_then(|a| note.caret_from_anchor(&a));
                if resolved != Some(idx) {
                    failures.push(format!(
                        "trial {trial}: text={text:?} caret {idx} -> {resolved:?}"
                    ));
                    if failures.len() > 5 {
                        break 'outer;
                    }
                }
            }
        }
        assert!(
            failures.is_empty(),
            "caret anchor anomalies:\n{}",
            failures.join("\n")
        );
    }

    #[test]
    fn new_in_memory_has_default_space() {
        let db = NoteDatabase::new_in_memory();

        assert_eq!(db.spaces().len(), 1);
        assert_eq!(db.spaces()[0].id, DEFAULT_SPACE_ID);
        assert_eq!(db.spaces()[0].name, DEFAULT_SPACE_NAME);
        assert_eq!(db.default_space_id(), DEFAULT_SPACE_ID);
        assert_eq!(db.summaries()[0].space_id, DEFAULT_SPACE_ID);
    }

    #[test]
    fn create_space_and_place_note_in_it() {
        let mut db = NoteDatabase::new_in_memory();

        let space_id = db.create_space();
        assert_ne!(space_id, DEFAULT_SPACE_ID);
        assert!(db.spaces().iter().any(|space| space.id == space_id));

        let id = db.create_note_in(space_id);
        let summary = db
            .summaries()
            .into_iter()
            .find(|summary| summary.id == id)
            .expect("created note summary");
        assert_eq!(summary.space_id, space_id);
    }

    #[test]
    fn create_note_in_unknown_space_falls_back_to_default() {
        let mut db = NoteDatabase::new_in_memory();

        let id = db.create_note_in(9999);
        let summary = db
            .summaries()
            .into_iter()
            .find(|summary| summary.id == id)
            .expect("created note summary");
        assert_eq!(summary.space_id, DEFAULT_SPACE_ID);
    }

    #[test]
    fn folder_lifecycle_keeps_note_assignments_consistent() {
        let mut db = NoteDatabase::new_in_memory();
        let space_id = db.create_space_named("Projects");
        let other_space = db.create_space_named("Archive");
        let folder = db.create_folder(space_id, "Roadmap").expect("folder");
        let other_folder = db
            .create_folder(other_space, "Wrong space")
            .expect("other folder");
        let child = db
            .create_folder_in(space_id, Some(folder), "Child")
            .expect("child folder");
        let note_id = db.create_note_in(space_id);

        db.set_note_folder(&note_id, Some(other_folder));
        assert_eq!(
            db.note(&note_id).expect("note").folder(),
            None,
            "folders from another space must be ignored"
        );

        db.set_note_folder(&note_id, Some(child));
        assert_eq!(db.note(&note_id).expect("note").folder(), Some(child));
        assert_eq!(
            db.summaries()
                .into_iter()
                .find(|summary| summary.id == note_id)
                .expect("summary")
                .folder,
            Some(child)
        );
        assert_eq!(db.folder(&child).expect("child").parent, Some(folder));

        db.delete_folder(&folder);
        assert!(db.folder(&folder).is_none());
        assert!(db.folder(&child).is_none());
        let note = db.note(&note_id).expect("note");
        assert_eq!(note.folder(), None);
        assert!(note.folder_needs_push());
    }

    #[test]
    fn move_note_to_space_clears_folder_and_remote_mapping() {
        let mut db = NoteDatabase::new_in_memory();
        let source = db.default_space_id();
        let target = db.create_space_named("Archive");
        let folder = db.create_folder(source, "Inbox").expect("folder");
        let note_id = db.create_note_in(source);
        let remote = Uuid::new_v4();

        db.set_note_folder(&note_id, Some(folder));
        db.note_mut(&note_id)
            .expect("note")
            .set_remote_doc(Some(remote));
        db.move_note_to_space(&note_id, target);

        let note = db.note(&note_id).expect("note");
        assert_eq!(note.space_id(), target);
        assert_eq!(note.folder(), None);
        assert!(note.folder_needs_push());
        assert_eq!(note.remote_doc(), None);
        assert!(note.needs_push());
    }

    #[test]
    fn folders_and_assignments_persist_across_reopen() {
        let path = temp_db_path("folders");

        let space_id;
        let folder_id;
        let child_id;
        let note_id;
        {
            let mut db = NoteDatabase::open(&path).expect("open database");
            space_id = db.create_space_named("Projects");
            folder_id = db.create_folder(space_id, "Roadmap").expect("folder");
            child_id = db
                .create_folder_in(space_id, Some(folder_id), "Milestone")
                .expect("child folder");
            db.rename_folder(&folder_id, "Planning");
            note_id = db.create_note_in(space_id);
            db.set_note_folder(&note_id, Some(child_id));
            db.flush_dirty().expect("flush dirty notes");
        }

        let db = NoteDatabase::open(&path).expect("reopen database");
        let folders: Vec<_> = db.folders_in_space(space_id).collect();
        assert_eq!(folders.len(), 2);
        assert_eq!(folders[0].id, folder_id);
        assert_eq!(folders[0].name, "Planning");
        assert_eq!(folders[0].parent, None);
        assert!(folders[0].needs_push);
        assert_eq!(folders[1].id, child_id);
        assert_eq!(folders[1].parent, Some(folder_id));

        let note = db.note(&note_id).expect("persisted note");
        assert_eq!(note.folder(), Some(child_id));
        assert!(note.folder_needs_push());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn folder_folded_state_persists_across_reopen() {
        let path = temp_db_path("folder_folded");
        let space_id;
        let folder_id;
        {
            let mut db = NoteDatabase::open(&path).expect("open database");
            space_id = db.create_space_named("Projects");
            folder_id = db.create_folder(space_id, "Roadmap").expect("folder");
            db.set_folder_folded(&folder_id, true);
        }

        let db = NoteDatabase::open(&path).expect("reopen database");
        let folder = db.folder(&folder_id).expect("persisted folder");
        assert_eq!(folder.space_id, space_id);
        assert!(folder.folded);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn spaces_and_membership_persist_across_reopen() {
        let path = temp_db_path("spaces");

        let note_id;
        let space_id;
        {
            let mut db = NoteDatabase::open(&path).expect("open database");
            space_id = db.create_space();
            note_id = db.create_note_in(space_id);
            db.note_mut(&note_id)
                .expect("created note exists")
                .insert_text(0, "# In space");
            db.flush_dirty().expect("flush dirty notes");
        }

        let db = NoteDatabase::open(&path).expect("reopen database");

        assert!(db.spaces().iter().any(|space| space.id == space_id));
        let summary = db
            .summaries()
            .into_iter()
            .find(|summary| summary.id == note_id)
            .expect("persisted note summary");
        assert_eq!(summary.space_id, space_id);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn legacy_storage_migrates_to_spaces() {
        let path = temp_db_path("legacy_storage");
        {
            let conn = Connection::open(&path).expect("open legacy database");
            conn.execute_batch(
                "
                CREATE TABLE notes (
                    id TEXT PRIMARY KEY,
                    title TEXT NOT NULL,
                    file_path TEXT,
                    frontmatter_title TEXT,
                    created TEXT,
                    updated TEXT,
                    notebook_id INTEGER,
                    yrs_state BLOB NOT NULL,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    deleted_at INTEGER
                );

                CREATE TABLE notebooks (
                    id INTEGER PRIMARY KEY,
                    name TEXT NOT NULL,
                    position INTEGER NOT NULL,
                    created_at INTEGER NOT NULL
                );
                ",
            )
            .expect("create legacy schema");
            conn.execute(
                "INSERT INTO notebooks (id, name, position, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![DEFAULT_SPACE_ID, "My Notes", 0_i64, 0_i64],
            )
            .expect("insert legacy default space");
            conn.execute(
                "INSERT INTO notebooks (id, name, position, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![2_i64, "Research", 1_i64, 0_i64],
            )
            .expect("insert legacy secondary space");
            conn.execute(
                "
                INSERT INTO notes (
                    id,
                    title,
                    file_path,
                    notebook_id,
                    yrs_state,
                    created_at,
                    updated_at
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                ",
                params![
                    "legacy",
                    "Legacy",
                    "legacy.md",
                    2_i64,
                    Vec::<u8>::new(),
                    0_i64,
                    0_i64
                ],
            )
            .expect("insert legacy note");
        }

        let db = NoteDatabase::open(&path).expect("migrate legacy database");

        assert_eq!(db.spaces()[0].name, DEFAULT_SPACE_NAME);
        assert!(db.spaces().iter().any(|space| space.name == "Research"));
        let summary = db
            .summaries()
            .into_iter()
            .find(|summary| summary.id == "legacy")
            .expect("legacy note summary");
        assert_eq!(summary.space_id, 2);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn needs_push_tracks_local_edits_only() {
        let mut note = Note::new("note", "hello");
        assert!(!note.needs_push());

        // Local edits flag unacknowledged content…
        note.insert_text(5, "!");
        assert!(note.needs_push());
        note.set_needs_push(false);
        assert!(!note.needs_push());

        // …but remote applies don't: the server already has that content.
        let mut other = Note::new("other", "remote text");
        other.insert_text(0, ">");
        let update = other.encode_update_since(&yrs::StateVector::default());
        note.apply_remote_update(&update).unwrap();
        assert!(!note.needs_push());
        assert!(note.is_dirty(), "remote applies still persist");
    }

    #[test]
    fn needs_push_persists_with_the_note() {
        let path = temp_db_path("needs_push");
        let note_id;
        {
            let mut db = NoteDatabase::open(&path).expect("open database");
            note_id = db.create_note();
            db.note_mut(&note_id).unwrap().insert_text(0, "# Unacked");
            db.flush_dirty().expect("flush");
        }
        {
            let db = NoteDatabase::open(&path).expect("reopen database");
            assert!(
                db.note(&note_id).unwrap().needs_push(),
                "unacknowledged edit must survive restart"
            );
        }
        {
            let mut db = NoteDatabase::open(&path).expect("reopen database");
            db.note_mut(&note_id).unwrap().set_needs_push(false);
            db.flush_dirty().expect("flush");
        }
        let db = NoteDatabase::open(&path).expect("reopen database");
        assert!(!db.note(&note_id).unwrap().needs_push());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn sync_observer_forwards_local_edits_and_skips_remote_applies() {
        use std::cell::RefCell;
        use std::rc::Rc;
        let forwarded: Rc<RefCell<Vec<Vec<u8>>>> = Rc::default();
        let sink = forwarded.clone();

        let mut note = Note::new("note", "");
        note.attach_sync_observer(move |update| sink.borrow_mut().push(update));

        note.insert_text(0, "local");
        assert_eq!(forwarded.borrow().len(), 1, "local edit must forward");

        let mut other = Note::new("other", "remote");
        other.insert_text(0, "!");
        let update = other.encode_update_since(&yrs::StateVector::default());
        note.apply_remote_update(&update).unwrap();
        assert_eq!(
            forwarded.borrow().len(),
            1,
            "remote applies must not echo through the observer"
        );
    }

    #[test]
    fn caret_anchor_sticks_through_remote_edits() {
        use yrs::StateVector;
        // Two replicas of the same note; a caret anchored in one must keep
        // its logical position when the other's edits are merged in.
        let mut local = Note::new("note", "hello world");
        let mut remote = Note::new("remote", "");
        remote
            .apply_remote_update(&local.encode_update_since(&StateVector::default()))
            .unwrap();

        // Caret after "hello" (index 5), selection anchor at 2.
        let caret = local.caret_anchor(5).expect("caret anchor");
        let anchor = local.caret_anchor(2).expect("selection anchor");

        // Remote prepends text; merge it into the local note.
        remote.insert_text(0, ">>> ");
        let update = remote.encode_update_since(&local.state_vector());
        local.apply_remote_update(&update).unwrap();
        assert_eq!(local.text(), ">>> hello world");

        assert_eq!(local.caret_from_anchor(&caret), Some(9));
        assert_eq!(local.caret_from_anchor(&anchor), Some(6));
    }

    #[test]
    fn tracking_apply_reports_caret_at_remote_insert_end() {
        use yrs::StateVector;
        use yrs::updates::decoder::Decode;
        let mut local = Note::new("note", "hello world");
        let mut remote = Note::new("remote", "");
        remote
            .apply_remote_update(&local.encode_update_since(&StateVector::default()))
            .unwrap();

        // Remote types " there" after "hello" (its caret ends at index 11).
        remote.insert_text(5, " there");
        let update = remote.encode_update_since(&local.state_vector());
        let decoded = yrs::Update::decode_v1(&update).unwrap();
        let caret = local.apply_remote_update_tracking_caret(decoded).unwrap();

        assert_eq!(local.text(), "hello there world");
        let caret = caret.expect("an insert must yield a caret");
        assert_eq!(local.caret_from_anchor(&caret), Some(11));
    }

    #[test]
    fn tracking_apply_reports_caret_in_char_units_with_multibyte() {
        use yrs::StateVector;
        use yrs::updates::decoder::Decode;
        // Multi-byte chars both in the retained prefix and the inserted text, so a
        // byte-vs-UTF-16 confusion would land the tracked caret off the real char.
        let mut local = Note::new("note", "héllo wörld");
        let mut remote = Note::new("remote", "");
        remote
            .apply_remote_update(&local.encode_update_since(&StateVector::default()))
            .unwrap();

        // Remote types " thére" after "héllo" (char index 5). The author's caret
        // ends one char past the inserted run: 5 + len(" thére") = 11.
        remote.insert_text(5, " thére");
        let update = remote.encode_update_since(&local.state_vector());
        let decoded = yrs::Update::decode_v1(&update).unwrap();
        let caret = local.apply_remote_update_tracking_caret(decoded).unwrap();

        assert_eq!(local.text(), "héllo thére wörld");
        let caret = caret.expect("an insert must yield a caret");
        assert_eq!(local.caret_from_anchor(&caret), Some(11));
    }

    #[test]
    fn tracking_apply_reports_caret_at_deletion_point() {
        use yrs::StateVector;
        use yrs::updates::decoder::Decode;
        let mut local = Note::new("note", "hello world");
        let mut remote = Note::new("remote", "");
        remote
            .apply_remote_update(&local.encode_update_since(&StateVector::default()))
            .unwrap();

        // Remote deletes the leading "hello " — caret collapses to index 0.
        remote.delete_range((0, 6));
        let update = remote.encode_update_since(&local.state_vector());
        let decoded = yrs::Update::decode_v1(&update).unwrap();
        let caret = local.apply_remote_update_tracking_caret(decoded).unwrap();

        assert_eq!(local.text(), "world");
        let caret = caret.expect("a deletion must yield a caret");
        assert_eq!(local.caret_from_anchor(&caret), Some(0));
    }

    #[test]
    fn local_edit_clock_tracks_only_local_edits() {
        let mut note = Note::new("note", "");
        note.attach_sync_observer(|_| {});
        assert_eq!(note.local_edit_clock(), 0);

        note.insert_text(0, "hi");
        let after_local = note.local_edit_clock();
        assert!(after_local > 0, "local edit must advance the clock");

        let other = Note::new("other", "x");
        let update = other.encode_update_since(&yrs::StateVector::default());
        note.apply_remote_update(&update).unwrap();
        assert_eq!(
            note.local_edit_clock(),
            after_local,
            "remote apply must not advance the local edit clock"
        );
    }

    #[test]
    fn note_stores_initial_text_in_yrs() {
        let note = Note::new("note", "hello");

        assert_eq!(note.id(), "note");
        assert_eq!(note.text(), "hello");
    }

    #[test]
    fn insert_text_applies_yrs_insert() {
        let mut note = Note::new("note", "hello world");

        note.insert_text(6, "cruel ");

        assert_eq!(note.text(), "hello cruel world");
    }

    #[test]
    fn delete_range_applies_yrs_delete() {
        let mut note = Note::new("note", "hello world");

        note.delete_range((5, 11));

        assert_eq!(note.text(), "hello");
    }

    #[test]
    fn text_operations_handle_unicode_boundaries() {
        let mut note = Note::new("note", "hello caf\u{e9}");

        note.insert_text(10, " menu");
        note.delete_range((6, 10));

        assert_eq!(note.text(), "hello  menu");
    }

    #[test]
    fn database_starts_with_memory_notes() {
        let db = NoteDatabase::new_in_memory();

        assert_eq!(db.first_note_id(), Some("Welcome"));
        assert_eq!(db.summaries()[0].title, "Welcome");
    }

    #[test]
    fn database_creates_notes_and_returns_them() {
        let mut db = NoteDatabase::new_in_memory();

        let id = db.create_note();
        let note = db.note_mut(&id).expect("created note exists");
        note.insert_text(0, "# Draft");

        assert_eq!(id, "Untitled 1");
        // The title is the file name, never derived from the body. Looked up by
        // id rather than by position: notes are held in the canonical
        // `(file_path, created, id)` order, so a new one lands where it sorts
        // rather than at the end.
        let summaries = db.summaries();
        let created = summaries
            .iter()
            .find(|summary| summary.id == id)
            .expect("created note is listed");
        assert_eq!(created.title, "Untitled 1");
    }

    #[test]
    fn sqlite_database_persists_notes() {
        let path = temp_db_path("persist");

        {
            let mut db = NoteDatabase::open(&path).expect("open database");
            let id = db.create_note();
            db.note_mut(&id)
                .expect("created note exists")
                .insert_text(0, "# Persisted");
            db.flush_dirty().expect("flush dirty notes");
        }

        let db = NoteDatabase::open(&path).expect("reopen database");

        assert!(db.contains("Untitled 1"));
        assert_eq!(
            db.summaries()
                .iter()
                .find(|summary| summary.id == "Untitled 1")
                .unwrap()
                .title,
            "Untitled 1"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn sqlite_database_persists_note_deletion() {
        let path = temp_db_path("delete_note");
        let note_id;
        {
            let mut db = NoteDatabase::open(&path).expect("open database");
            note_id = db.create_note();
            db.note_mut(&note_id)
                .expect("created note exists")
                .insert_text(0, "# Delete me");
            db.flush_dirty().expect("flush dirty notes");
            assert!(db.delete_note(&note_id));
        }

        let db = NoteDatabase::open(&path).expect("reopen database");
        assert!(!db.contains(&note_id));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn sqlite_database_loads_next_untitled_number() {
        let path = temp_db_path("next_number");

        {
            let mut db = NoteDatabase::open(&path).expect("open database");
            assert_eq!(db.create_note(), "Untitled 1");
            assert_eq!(db.create_note(), "Untitled 2");
            db.flush_dirty().expect("flush dirty notes");
        }

        let mut db = NoteDatabase::open(&path).expect("reopen database");

        assert_eq!(db.create_note(), "Untitled 3");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn delete_space_removes_its_notes_and_folders() {
        let path = temp_db_path("delete_space");
        let space_id;
        let folder_id;
        let note_id;
        {
            let mut db = NoteDatabase::open(&path).expect("open database");
            space_id = db.create_space_named("Temporary");
            folder_id = db.create_folder(space_id, "Scratch").expect("folder");
            note_id = db.create_note_in(space_id);
            db.set_note_folder(&note_id, Some(folder_id));
            db.flush_dirty().expect("flush dirty notes");
            assert!(db.delete_space(space_id));
        }

        let db = NoteDatabase::open(&path).expect("reopen database");
        assert!(!db.spaces().iter().any(|space| space.id == space_id));
        assert!(db.folder(&folder_id).is_none());
        assert!(!db.contains(&note_id));
        assert_eq!(db.spaces().len(), 1);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn set_folder_parent_nests_folder_and_rejects_cycles() {
        let mut db = NoteDatabase::new_in_memory();
        let space = db.default_space_id();
        let parent = db.create_folder(space, "Parent").expect("parent");
        let child = db.create_folder(space, "Child").expect("child");
        let grandchild = db
            .create_folder_in(space, Some(child), "Grandchild")
            .expect("grandchild");

        // Nest the child under the parent.
        db.set_folder_parent(&child, Some(parent));
        assert_eq!(db.folder(&child).unwrap().parent, Some(parent));

        // A cycle (parent dropped into its own descendant) is rejected.
        db.set_folder_parent(&parent, Some(grandchild));
        assert_eq!(db.folder(&parent).unwrap().parent, None);

        // Dropping a folder onto itself is rejected.
        db.set_folder_parent(&child, Some(child));
        assert_eq!(db.folder(&child).unwrap().parent, Some(parent));

        // Back to the space root.
        db.set_folder_parent(&child, None);
        assert_eq!(db.folder(&child).unwrap().parent, None);
    }

    #[test]
    fn move_folder_to_space_carries_subtree_and_notes() {
        let mut db = NoteDatabase::new_in_memory();
        let src = db.default_space_id();
        let dst = db.create_space_named("Destination");
        let parent = db.create_folder(src, "Parent").expect("parent");
        let child = db
            .create_folder_in(src, Some(parent), "Child")
            .expect("child");
        let note = db.create_note_in(src);
        db.set_note_folder(&note, Some(child));

        db.move_folder_to_space(&parent, dst);

        // The whole subtree changed space; the dragged folder roots in the dest.
        assert_eq!(db.folder(&parent).unwrap().space_id, dst);
        assert_eq!(db.folder(&parent).unwrap().parent, None);
        assert_eq!(db.folder(&child).unwrap().space_id, dst);
        assert_eq!(db.folder(&child).unwrap().parent, Some(parent));
        // The contained note followed, keeping its folder assignment.
        let summary = db
            .summaries()
            .into_iter()
            .find(|s| s.id == note)
            .expect("note summary");
        assert_eq!(summary.space_id, dst);
        assert_eq!(summary.folder, Some(child));
    }

    #[test]
    fn deleting_last_space_survives_reopen_without_recreating_default_space() {
        let path = temp_db_path("delete_last_space");

        {
            let mut db = NoteDatabase::open(&path).expect("open database");
            let only_space = db.default_space_id();
            assert!(db.delete_space(only_space));
        }

        let db = NoteDatabase::open(&path).expect("reopen database");
        assert!(db.spaces().is_empty());
        assert!(db.summaries().is_empty());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn import_folder_reads_markdown_tree_and_frontmatter() {
        let root = temp_dir_path("import");
        std::fs::create_dir_all(root.join("Topics").join("AI")).expect("create import tree");
        std::fs::write(
            root.join("Topics").join("AI").join("Overview.md"),
            "---\ntitle: \"AI\"\ncreated: 2024-02-16T16:46:31+00:00\nupdated: 2024-02-16T16:46:31+00:00\n---\n\nBody",
        )
        .expect("write markdown note");

        let mut db = NoteDatabase::new_in_memory();
        let imported = db.import_folder(&root).expect("import folder");

        assert_eq!(imported, vec!["Topics/AI/Overview.md"]);
        let summary = db
            .summaries()
            .into_iter()
            .find(|summary| summary.file_path == "Topics/AI/Overview.md")
            .expect("imported note summary");
        // The title is the file name, not the frontmatter `title:`.
        assert_eq!(summary.title, "Overview");
        let topics = db
            .folders_in_space(db.default_space_id())
            .find(|folder| folder.name == "Topics" && folder.parent.is_none())
            .expect("top-level imported folder");
        let topics_id = topics.id;
        let ai = db
            .folders_in_space(db.default_space_id())
            .find(|folder| folder.name == "AI" && folder.parent == Some(topics_id))
            .expect("nested imported folder");
        assert_eq!(summary.folder, Some(ai.id));
        assert_eq!(
            db.note_mut("Topics/AI/Overview.md")
                .expect("imported note")
                .text(),
            "\nBody"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn import_folder_imports_text_files_verbatim_and_skips_binary() {
        let root = temp_dir_path("import_source");
        std::fs::create_dir_all(&root).expect("create import dir");
        // A non-markdown text file: kept verbatim, with its extension.
        std::fs::write(root.join("snippet.rs"), "fn main() {}\n# not a heading\n")
            .expect("write source file");
        // An extension-less text file (e.g. a Makefile) is still text.
        std::fs::write(root.join("README"), "plain readme").expect("write readme");
        // A binary file: skipped, since the text store can't hold it.
        std::fs::write(root.join("logo.png"), [0u8, 159, 146, 150]).expect("write binary");
        // Hidden files and folders are ignored entirely.
        std::fs::write(root.join(".gitignore"), "target/").expect("write dotfile");
        std::fs::create_dir_all(root.join(".git")).expect("create dot dir");
        std::fs::write(root.join(".git").join("config"), "[core]").expect("write in dot dir");

        let mut db = NoteDatabase::new_in_memory();
        let imported = db.import_folder(&root).expect("import folder");

        // Binary, dotfile and dot-folder contents are excluded; README has no
        // extension so it defaults to markdown (`README.md`).
        assert_eq!(
            imported,
            vec!["README.md".to_string(), "snippet.rs".to_string()]
        );

        let source = db.note("snippet.rs").expect("imported source note");
        assert!(source.is_source_only());
        // Body is the raw file content, not markdown-parsed.
        assert_eq!(source.text(), "fn main() {}\n# not a heading\n");
        // The title comes from the file name (stem), not the first body line.
        assert_eq!(source.title(), "snippet");
        // Source notes export back without frontmatter.
        assert_eq!(source.to_markdown_file(), "fn main() {}\n# not a heading\n");

        let readme = db.note("README.md").expect("imported readme note");
        assert!(!readme.is_source_only());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn import_normalizes_decomposed_accents_to_nfc() {
        let root = temp_dir_path("import_nfc");
        std::fs::create_dir_all(&root).expect("create import dir");
        // Decomposed (NFD) "café": the final é is `e` + U+0301 combining acute.
        let decomposed = "cafe\u{301} notes";
        assert_eq!(decomposed.chars().count(), 11);
        std::fs::write(root.join("notes.txt"), decomposed).expect("write nfd file");

        let mut db = NoteDatabase::new_in_memory();
        db.import_folder(&root).expect("import folder");

        let text = db.note("notes.txt").expect("imported note").text();
        // Stored precomposed: one `é` scalar (U+00E9), so the grapheme is a single
        // cursor position — no zero-width combining mark to split the caret.
        assert_eq!(text, "caf\u{e9} notes");
        assert_eq!(text.chars().count(), 10);
        assert!(!text.contains('\u{301}'));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn import_title_replaces_word_separators_with_spaces() {
        let root = temp_dir_path("import_separators");
        std::fs::create_dir_all(&root).expect("create import dir");
        std::fs::write(root.join("my-first_note.md"), "Body").expect("write note");

        let mut db = NoteDatabase::new_in_memory();
        db.import_folder(&root).expect("import folder");

        let summary = &db.summaries()[db.summaries().len() - 1];
        assert_eq!(summary.title, "my first note");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn setting_title_renames_the_file_keeping_folder_and_extension() {
        let mut note = Note::from_imported_source("Topics/snippet.rs", "Topics/snippet.rs", "code");
        assert_eq!(note.title(), "snippet");

        note.set_title("My Cool Script");
        // Title is stored verbatim; the file is renamed (spaces -> `-`), keeping
        // the folder prefix and the original extension.
        assert_eq!(note.title(), "My Cool Script");
        assert_eq!(note.file_path(), "Topics/My-Cool-Script.rs");
        assert!(note.is_source_only());

        // A markdown note keeps the `.md` extension.
        let mut md = Note::new("Untitled 1", "");
        md.set_title("Release notes");
        assert_eq!(md.file_path(), "Release-notes.md");

        // Empty titles are ignored — the file never loses its name.
        let before = md.file_path().to_string();
        md.set_title("   ");
        assert_eq!(md.file_path(), before);
    }

    #[test]
    fn import_folder_into_new_space_keeps_a_separate_copy() {
        let root = temp_dir_path("import_new_space");
        std::fs::create_dir_all(&root).expect("create import dir");
        std::fs::write(root.join("Note.md"), "Body text").expect("write note");

        let mut db = NoteDatabase::new_in_memory();
        let first = db.default_space_id();
        let imported_first = db
            .import_folder_into(&root, first)
            .expect("import into first");
        assert_eq!(imported_first, vec!["Note.md"]);

        // Same folder, a different space: a fresh copy, not a move of the first.
        let second = db.create_space_named("Second");
        let imported_second = db
            .import_folder_into(&root, second)
            .expect("import into second");
        assert_eq!(imported_second.len(), 1);
        assert_ne!(imported_second[0], "Note.md", "new copy gets a unique id");

        // The copy lives in the second space; both point at the same path.
        assert!(db.note_ids_in_space(first).contains(&"Note.md".to_string()));
        assert_eq!(db.note_ids_in_space(second), imported_second);
        assert_eq!(
            db.note(&imported_second[0]).map(|n| n.file_path()),
            Some("Note.md")
        );

        // Re-importing into the first space updates in place (idempotent): the
        // id stays "Note.md" and no duplicate is created.
        let reimport = db
            .import_folder_into(&root, first)
            .expect("re-import first");
        assert_eq!(reimport, vec!["Note.md"]);
        assert_eq!(
            db.note_ids_in_space(first)
                .iter()
                .filter(|id| id.as_str() == "Note.md")
                .count(),
            1
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn pasting_the_same_image_twice_reuses_one_blob() {
        use enkr_proto::wire::ImageMime;
        let mut db = NoteDatabase::new_in_memory();
        let space = db.default_space_id();
        let bytes = b"\x89PNG\r\n\x1a\n pretend image".to_vec();

        let first = db.create_blob_in(space, "a.png", ImageMime::Png, bytes.clone());
        let second = db.create_blob_in(space, "b.png", ImageMime::Png, bytes.clone());
        assert_eq!(first, second, "identical bytes should reuse the same blob");
        assert_eq!(db.blobs_in_space(space).count(), 1);

        // Different content still gets its own blob.
        let other = db.create_blob_in(space, "c.png", ImageMime::Png, b"different".to_vec());
        assert_ne!(other, first);
        assert_eq!(db.blobs_in_space(space).count(), 2);
    }

    #[test]
    fn dedup_is_scoped_to_a_space() {
        use enkr_proto::wire::ImageMime;
        let mut db = NoteDatabase::new_in_memory();
        let first_space = db.default_space_id();
        let second_space = db.create_space_named("Other");
        let bytes = b"\x89PNG\r\n\x1a\n pretend image".to_vec();

        let a = db.create_blob_in(first_space, "a.png", ImageMime::Png, bytes.clone());
        let b = db.create_blob_in(second_space, "a.png", ImageMime::Png, bytes);
        // Deliberate: sharing one blob across spaces would let the relay see
        // that two spaces hold the same file.
        assert_ne!(a, b, "dedup must not cross a space boundary");
    }

    #[test]
    fn pasting_an_image_a_peer_advertised_fills_in_its_bytes() {
        use enkr_proto::wire::ImageMime;
        let mut db = NoteDatabase::new_in_memory();
        let space = db.default_space_id();
        let bytes = b"\x89PNG\r\n\x1a\n pretend image".to_vec();
        let hash = enkr_proto::crypto::content_hash(&bytes);

        // A peer's index entry: metadata known, content not fetched yet.
        let remote_id = Uuid::new_v4();
        db.upsert_blob_meta_from_remote(
            &remote_id.to_string(),
            space,
            "shared.png",
            ImageMime::Png,
            hash,
            [7u8; 32],
            None,
        );
        assert!(
            db.blob(&remote_id.to_string())
                .expect("blob")
                .bytes
                .is_empty()
        );

        // Pasting the same image locally should adopt that entry and supply the
        // bytes, rather than minting a second blob and downloading the first.
        let id = db.create_blob_in(space, "mine.png", ImageMime::Png, bytes.clone());
        assert_eq!(id, remote_id.to_string());
        assert_eq!(db.blob(&id).expect("blob").bytes, bytes);
        assert_eq!(db.blobs_in_space(space).count(), 1);
    }

    #[test]
    fn export_folder_writes_markdown_tree_with_frontmatter() {
        let import_root = temp_dir_path("export_import");
        let export_root = temp_dir_path("export");
        std::fs::create_dir_all(import_root.join("Topics")).expect("create import tree");
        std::fs::write(
            import_root.join("Topics").join("AI.md"),
            "---\ntitle: \"AI\"\ncreated: 2024-02-16T16:46:31+00:00\nupdated: 2024-02-16T16:46:31+00:00\n---\n\nBody",
        )
        .expect("write markdown note");

        let mut db = NoteDatabase::new_in_memory();
        db.import_folder(&import_root).expect("import folder");
        let exported = db.export_folder(&export_root).expect("export folder");

        let exported_note = std::fs::read_to_string(export_root.join("Topics").join("AI.md"))
            .expect("read exported note");
        assert_eq!(exported, 2);
        assert!(exported_note.contains("title: \"AI\""));
        assert!(exported_note.contains("created: 2024-02-16T16:46:31+00:00"));
        assert!(exported_note.contains("updated: 2024-02-16T16:46:31+00:00"));
        assert!(exported_note.ends_with("\nBody"));

        let _ = std::fs::remove_dir_all(import_root);
        let _ = std::fs::remove_dir_all(export_root);
    }

    fn temp_db_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "enkr_note_db_{name}_{}_{}.sqlite3",
            std::process::id(),
            unix_timestamp()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn temp_dir_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        path.push(format!(
            "enkr_note_dir_{name}_{}_{}",
            std::process::id(),
            nonce
        ));
        let _ = std::fs::remove_dir_all(&path);
        path
    }
}
