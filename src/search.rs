//! Local full-text search.
//!
//! A dedicated worker thread scans a snapshot of every note's decoded text for
//! a query and streams matches back to the UI thread, which repaints as each
//! result arrives. The scan never runs on the UI thread: the corpus snapshot is
//! built once when the search palette opens (cloning each note's text) and
//! handed to the worker, so per-keystroke matching is fully off-thread. A
//! `generation` counter lets the UI discard results from superseded queries,
//! and the worker abandons an in-flight scan the moment a newer query queues up.

#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
#[cfg(not(target_arch = "wasm32"))]
use std::thread;

use mae::imui::RepaintWaker;

/// Chars of surrounding context kept on each side of a match in the excerpt.
const EXCERPT_RADIUS: usize = 48;

/// One note in the searchable corpus snapshot.
pub struct SearchDoc {
    pub note_id: String,
    /// Human-readable location, e.g. `Space / Folder / Title`.
    pub full_name: String,
    pub text: String,
}

/// A match surfaced to the UI: which note, where it lives, and a one-line
/// excerpt around the first occurrence.
#[derive(Clone)]
pub struct SearchHit {
    pub note_id: String,
    /// Secondary label: the note's path (global search) or `Line N` (document
    /// search) — set by the worker per query mode.
    pub full_name: String,
    pub excerpt: String,
    /// Byte ranges within `excerpt` covering each occurrence of the query,
    /// so the UI can highlight the matched runs without re-searching.
    pub match_ranges: Vec<(usize, usize)>,
    /// Char offset of this match's start within the note's full text, so the
    /// UI can jump the editor caret to the occurrence.
    pub offset: usize,
}

/// A worker result drained by the UI for the current query generation.
pub enum SearchUpdate {
    Hit(SearchHit),
    /// The current generation's scan finished (no more hits coming).
    Done,
}

/// UI → worker messages.
#[cfg(not(target_arch = "wasm32"))]
enum Command {
    /// Replace the searchable corpus (sent when the palette (re)opens).
    SetCorpus(Vec<SearchDoc>),
    /// Run a query; `generation` tags every result so stale ones are dropped.
    /// When `all_occurrences` is set (document search) every match in every doc
    /// is emitted; otherwise (global search) only the first match per doc.
    Query {
        generation: u64,
        query: String,
        all_occurrences: bool,
    },
}

/// Worker → UI messages, each tagged with the generation that produced it.
enum Outcome {
    Hit { generation: u64, hit: SearchHit },
    Done { generation: u64 },
}

/// Handle to the background search worker. Dropping it (closing the palette)
/// disconnects the command channel and lets the worker thread exit.
///
/// Native only — wasm32 has no real thread to run the scan off of. See the
/// wasm32 `SearchEngine` below: same public API (so `app.rs` needs no
/// `#[cfg]`s of its own), but the scan runs synchronously and immediately
/// inside `query()` instead of being streamed back from a worker — perfectly
/// fine for a local notes corpus (no I/O, dozens–hundreds of notes, not
/// millions), and it sidesteps needing any cancellation/generation-race
/// handling at all, since there's no concurrent scan to race with.
#[cfg(not(target_arch = "wasm32"))]
pub struct SearchEngine {
    tx: Sender<Command>,
    rx: Receiver<Outcome>,
}

#[cfg(not(target_arch = "wasm32"))]
impl SearchEngine {
    /// Spawn the worker thread. `waker` is pulsed after each match so the idle
    /// event loop wakes and repaints while the scan is still running.
    pub fn spawn(waker: RepaintWaker) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
        let (out_tx, out_rx) = mpsc::channel::<Outcome>();
        thread::Builder::new()
            .name("enkr-search".into())
            .spawn(move || worker(cmd_rx, out_tx, waker))
            .expect("spawn enkr-search worker");
        Self {
            tx: cmd_tx,
            rx: out_rx,
        }
    }

    pub fn set_corpus(&mut self, docs: Vec<SearchDoc>) {
        let _ = self.tx.send(Command::SetCorpus(docs));
    }

    pub fn query(&mut self, generation: u64, query: String, all_occurrences: bool) {
        let _ = self.tx.send(Command::Query {
            generation,
            query,
            all_occurrences,
        });
    }

    /// Drain every worker message that has arrived since the last call,
    /// invoking `on_update` for those matching `generation` and dropping the
    /// rest. Allocation-free: nothing is allocated when the channel is empty.
    pub fn poll(&mut self, generation: u64, mut on_update: impl FnMut(SearchUpdate)) {
        loop {
            match self.rx.try_recv() {
                Ok(Outcome::Hit { generation: g, hit }) if g == generation => {
                    on_update(SearchUpdate::Hit(hit));
                }
                Ok(Outcome::Done { generation: g }) if g == generation => {
                    on_update(SearchUpdate::Done);
                }
                // Stale result from a superseded query: discard.
                Ok(_) => {}
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
    }
}

/// wasm32: no worker thread — `query()` scans synchronously and buffers
/// [`Outcome`]s for `poll()` to drain, matching the native worker's output
/// shape/ordering exactly so callers see no behavioral difference beyond
/// results arriving all at once instead of trickling in.
#[cfg(target_arch = "wasm32")]
pub struct SearchEngine {
    corpus: Vec<SearchDoc>,
    pending: std::collections::VecDeque<Outcome>,
}

#[cfg(target_arch = "wasm32")]
impl SearchEngine {
    /// `waker` is unused here (results are ready by the time `query()`
    /// returns, nothing to wake later for) but kept in the signature so
    /// `app.rs`'s call site needs no `#[cfg]`.
    pub fn spawn(_waker: RepaintWaker) -> Self {
        Self {
            corpus: Vec::new(),
            pending: std::collections::VecDeque::new(),
        }
    }

    pub fn set_corpus(&mut self, docs: Vec<SearchDoc>) {
        self.corpus = docs;
    }

    pub fn query(&mut self, generation: u64, query: String, all_occurrences: bool) {
        // A fresh query supersedes any not-yet-drained results from the last
        // one — matches the native worker abandoning a superseded scan.
        self.pending.clear();
        let needle = lower_chars(query.trim());
        if !needle.is_empty() {
            for doc in &self.corpus {
                let chars: Vec<(usize, char)> = doc.text.char_indices().collect();
                let lowered: Vec<char> = chars
                    .iter()
                    .map(|&(_, c)| c.to_lowercase().next().unwrap_or(c))
                    .collect();
                let mut from = 0;
                while let Some(start) = find_from(&lowered, &needle, from) {
                    let hit = build_hit(doc, &chars, start, &needle, all_occurrences);
                    self.pending.push_back(Outcome::Hit { generation, hit });
                    if !all_occurrences {
                        break;
                    }
                    from = start + needle.len();
                }
            }
        }
        self.pending.push_back(Outcome::Done { generation });
    }

    pub fn poll(&mut self, generation: u64, mut on_update: impl FnMut(SearchUpdate)) {
        while let Some(outcome) = self.pending.pop_front() {
            match outcome {
                Outcome::Hit { generation: g, hit } if g == generation => {
                    on_update(SearchUpdate::Hit(hit));
                }
                Outcome::Done { generation: g } if g == generation => {
                    on_update(SearchUpdate::Done);
                }
                _ => {}
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn worker(cmd_rx: Receiver<Command>, out_tx: Sender<Outcome>, waker: RepaintWaker) {
    let mut corpus: Vec<SearchDoc> = Vec::new();
    // A command pulled off the channel mid-scan that we still owe processing.
    let mut pending: Option<Command> = None;
    loop {
        let cmd = match pending.take() {
            Some(cmd) => cmd,
            None => match cmd_rx.recv() {
                Ok(cmd) => cmd,
                // UI dropped the engine: shut the thread down.
                Err(_) => return,
            },
        };
        match cmd {
            Command::SetCorpus(docs) => corpus = docs,
            Command::Query {
                generation,
                query,
                all_occurrences,
            } => {
                let needle = lower_chars(query.trim());
                if needle.is_empty() {
                    let _ = out_tx.send(Outcome::Done { generation });
                    waker.wake();
                    continue;
                }
                let mut superseded = false;
                'docs: for doc in &corpus {
                    // Char/byte index of every char + a parallel lowercased view,
                    // so matching and excerpt slicing stay on char boundaries.
                    let chars: Vec<(usize, char)> = doc.text.char_indices().collect();
                    let lowered: Vec<char> = chars
                        .iter()
                        .map(|&(_, c)| c.to_lowercase().next().unwrap_or(c))
                        .collect();

                    let mut from = 0;
                    loop {
                        // Abandon the scan as soon as a newer command queues up
                        // so fast typing never waits on a stale pass.
                        match cmd_rx.try_recv() {
                            Ok(cmd) => {
                                pending = Some(cmd);
                                superseded = true;
                                break 'docs;
                            }
                            Err(TryRecvError::Empty) => {}
                            Err(TryRecvError::Disconnected) => return,
                        }
                        let Some(start) = find_from(&lowered, &needle, from) else {
                            break;
                        };
                        let hit = build_hit(doc, &chars, start, &needle, all_occurrences);
                        if out_tx.send(Outcome::Hit { generation, hit }).is_err() {
                            return;
                        }
                        // Repaint after every occurrence found (incremental view).
                        waker.wake();
                        if !all_occurrences {
                            break;
                        }
                        from = start + needle.len(); // non-overlapping
                    }
                }
                if !superseded {
                    let _ = out_tx.send(Outcome::Done { generation });
                    waker.wake();
                }
            }
        }
    }
}

/// Lowercase `s` to a per-char vector that stays 1:1 with the original chars
/// (only the first char of each Unicode lowercase mapping is kept). Good enough
/// for case-insensitive substring search until full case folding lands.
fn lower_chars(s: &str) -> Vec<char> {
    s.chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect()
}

/// Index of the first case-insensitive match of `needle` (already lowercased,
/// one char per original char) in `lowered` at or after char index `from`.
fn find_from(lowered: &[char], needle: &[char], from: usize) -> Option<usize> {
    let n = needle.len();
    if n == 0 || lowered.len() < n {
        return None;
    }
    let limit = lowered.len() - n;
    (from..=limit).find(|&i| (0..n).all(|k| lowered[i + k] == needle[k]))
}

/// Build a [`SearchHit`] for the match at char index `start`: a tidy one-line
/// excerpt around it (with highlight ranges), the caret jump offset, and a
/// secondary label — the note path for global search, `Line N` for document
/// search (`all_occurrences`). `chars` is `text.char_indices()` collected.
fn build_hit(
    doc: &SearchDoc,
    chars: &[(usize, char)],
    start: usize,
    needle: &[char],
    document: bool,
) -> SearchHit {
    let text = &doc.text;
    let end = start + needle.len();

    // Window the excerpt in char space, then map back to byte offsets.
    let from = start.saturating_sub(EXCERPT_RADIUS);
    let to = (end + EXCERPT_RADIUS).min(chars.len());
    let byte_from = chars[from].0;
    let byte_to = if to < chars.len() {
        chars[to].0
    } else {
        text.len()
    };

    let mut excerpt = String::new();
    if from > 0 {
        excerpt.push('\u{2026}');
    }
    collapse_whitespace(&text[byte_from..byte_to], &mut excerpt);
    if to < chars.len() {
        excerpt.push('\u{2026}');
    }
    let match_ranges = match_ranges(&excerpt, needle);

    let full_name = if document {
        let line = text[..chars[start].0]
            .bytes()
            .filter(|&b| b == b'\n')
            .count()
            + 1;
        format!("Line {line}")
    } else {
        doc.full_name.clone()
    };

    SearchHit {
        note_id: doc.note_id.clone(),
        full_name,
        excerpt,
        match_ranges,
        offset: start,
    }
}

/// Byte ranges of every non-overlapping case-insensitive occurrence of `needle`
/// (already lowercased, one char per original char) in `text`.
fn match_ranges(text: &str, needle: &[char]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let n = needle.len();
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    if n == 0 || chars.len() < n {
        return ranges;
    }
    let lowered: Vec<char> = chars
        .iter()
        .map(|&(_, c)| c.to_lowercase().next().unwrap_or(c))
        .collect();
    let mut i = 0;
    while i + n <= lowered.len() {
        if (0..n).all(|k| lowered[i + k] == needle[k]) {
            let start = chars[i].0;
            let byte_end = if i + n < chars.len() {
                chars[i + n].0
            } else {
                text.len()
            };
            ranges.push((start, byte_end));
            i += n; // non-overlapping
        } else {
            i += 1;
        }
    }
    ranges
}

/// Append `s` to `out` with every whitespace run (incl. newlines) collapsed to
/// a single space, and leading/trailing whitespace trimmed, so a multi-line
/// match renders as one compact preview line.
fn collapse_whitespace(s: &str, out: &mut String) {
    // Start as if the previous char were whitespace so leading space is dropped.
    let mut prev_ws = true;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(c);
            prev_ws = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> SearchDoc {
        SearchDoc {
            note_id: "n".into(),
            full_name: "Space / Note".into(),
            text: text.into(),
        }
    }

    /// First match's excerpt, mirroring how the worker builds a global-search hit.
    fn first_hit(text: &str, query: &str) -> Option<SearchHit> {
        let d = doc(text);
        let needle = lower_chars(query);
        let chars: Vec<(usize, char)> = d.text.char_indices().collect();
        let lowered: Vec<char> = chars
            .iter()
            .map(|&(_, c)| c.to_lowercase().next().unwrap_or(c))
            .collect();
        let start = find_from(&lowered, &needle, 0)?;
        Some(build_hit(&d, &chars, start, &needle, false))
    }

    fn excerpt(text: &str, query: &str) -> Option<String> {
        first_hit(text, query).map(|h| h.excerpt)
    }

    #[test]
    fn matches_case_insensitively() {
        assert!(excerpt("The Quick Brown Fox", "quick").is_some());
        assert!(excerpt("the quick brown fox", "QUICK").is_some());
        assert!(excerpt("nothing here", "absent").is_none());
    }

    #[test]
    fn excerpt_collapses_whitespace_and_adds_ellipses() {
        let text = "alpha beta\n\n   gamma needle delta\tepsilon zeta";
        let got = excerpt(text, "needle").unwrap();
        assert!(got.contains("needle"));
        assert!(!got.contains('\n'));
        assert!(!got.contains("  "));
    }

    #[test]
    fn short_match_keeps_full_text_without_ellipses() {
        let got = excerpt("hello world", "world").unwrap();
        assert_eq!(got, "hello world");
    }

    #[test]
    fn match_ranges_cover_each_occurrence_in_excerpt() {
        let hit = first_hit("a needle in a needle stack", "needle").unwrap();
        assert_eq!(hit.match_ranges.len(), 2);
        for (s, e) in hit.match_ranges {
            assert_eq!(&hit.excerpt[s..e].to_lowercase(), "needle");
        }
    }

    #[test]
    fn hit_offset_points_at_the_match_in_full_text() {
        let hit = first_hit("zero one needle two", "needle").unwrap();
        // "zero one " is 9 chars; the match starts there.
        assert_eq!(hit.offset, 9);
    }

    #[test]
    fn document_mode_labels_hits_with_line_numbers() {
        let d = doc("first line\nsecond needle line\nthird");
        let needle = lower_chars("needle");
        let chars: Vec<(usize, char)> = d.text.char_indices().collect();
        let lowered: Vec<char> = chars
            .iter()
            .map(|&(_, c)| c.to_lowercase().next().unwrap_or(c))
            .collect();
        let start = find_from(&lowered, &needle, 0).unwrap();
        let hit = build_hit(&d, &chars, start, &needle, true);
        assert_eq!(hit.full_name, "Line 2");
    }

    #[test]
    fn long_prefix_and_suffix_are_truncated() {
        let text = format!("{} needle {}", "a ".repeat(80), "b ".repeat(80));
        let got = excerpt(&text, "needle").unwrap();
        assert!(got.starts_with('\u{2026}'));
        assert!(got.ends_with('\u{2026}'));
    }
}
