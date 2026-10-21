//! `[bible]` in the TUI — quoting verse text under Bible references, on top
//! of `shiki_core::bible`. Lookups (EPUB parse, `curl` to jw.org) never run
//! on the render loop: misses are queued to one background worker thread,
//! and its answers are drained by `poll_bible_channel` in `run()`, the same
//! `mpsc` shape `image_art` uses.
//!
//! Two modes (`[bible] mode`):
//! - `insert`: a save (inline `i` or external `E`) writes the quotes into
//!   the note. When some reference isn't cached yet, the note is saved as
//!   typed and remembered in `pending`; once the worker has answered every
//!   outstanding lookup, those files are rewritten with their quotes.
//! - `preview`: the file is never touched — only PREVIEW renders the quotes.
//!
//! Independently of the mode, `[bible] study_notes` (toggled live with
//! leader+`n`) shows each quote's study notes under it in PREVIEW only, as
//! a collapsible block — never written into the note, so flipping it is
//! instant and touches no file.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};

use shiki_config::Config;
use shiki_core::bible::{self, Bible, BibleRef, Books, Source, StudyNotes};

use crate::app::App;

/// One lookup for the worker thread.
enum Job {
    /// `Bible::prepare` — queued first, when the state is built.
    Prepare,
    Verse(BibleRef),
    Notes(BibleRef),
}

impl Job {
    /// `v:`/`n:`-prefixed key, unique per job (a verse and its notes are
    /// two separate lookups).
    fn key(&self) -> String {
        match self {
            Job::Prepare => "p:".to_string(),
            Job::Verse(r) => format!("v:{}", r.key()),
            Job::Notes(r) => format!("n:{}", r.key()),
        }
    }
}

pub(crate) struct BibleState {
    bible: Arc<Bible>,
    preview_only: bool,
    study_notes: bool,
    /// Job keys queued to the worker and not answered yet.
    inflight: HashSet<String>,
    /// References whose stale study notes were already re-asked for this
    /// session (see `StudyNotes::needs_recheck`).
    rechecked: HashSet<String>,
    requests: mpsc::Sender<Job>,
    answers: mpsc::Receiver<String>,
    /// Notes saved (insert mode) while some of their references were still
    /// being looked up — rewritten once `inflight` drains.
    pending: HashSet<PathBuf>,
    /// Why `bible-livres.toml` couldn't be used (the built-in list is used
    /// instead) — reported once, from the first `poll_bible_channel`.
    books_error: Option<String>,
    /// The spellings recognized in notes — `bible.books`, until this
    /// language's own list shows up after its EPUB's first download and
    /// gets merged in (`own_merged`).
    books: Arc<Books>,
    own_merged: bool,
}

impl BibleState {
    /// `None` when `[bible]` is off.
    pub(crate) fn from_config(config: &Config) -> Option<Self> {
        let cfg = &config.bible;
        if !cfg.enabled {
            return None;
        }
        let dir = Config::default_bible_dir()
            .unwrap_or_else(|_| std::env::temp_dir().join("shiki-bible"));
        let lang = bible::jw_lang(&cfg.language);
        let epub = bible::epub_path(&dir, &cfg.epub_path, &lang);
        let source = if cfg.online_only() {
            Source::Online
        } else {
            Source::Local
        };
        let books_path =
            Config::default_bible_books_path().unwrap_or_else(|_| dir.join("bible-livres.toml"));
        let (books, books_error) = match Books::load(&books_path) {
            Ok(books) => (books, None),
            Err(e) => (Books::default(), Some(e.to_string())),
        };
        let bible = Arc::new(Bible::new(
            source,
            cfg.edition.clone(),
            lang,
            books,
            &books_path,
            epub,
            &dir,
        ));

        let (requests, worker_rx) = mpsc::channel::<Job>();
        let (worker_tx, answers) = mpsc::channel();
        let worker = Arc::clone(&bible);
        std::thread::spawn(move || {
            for job in worker_rx {
                match &job {
                    Job::Prepare => worker.prepare(),
                    Job::Verse(r) => {
                        worker.resolve(r);
                    }
                    Job::Notes(r) => {
                        worker.resolve_notes(r);
                    }
                }
                if worker_tx.send(job.key()).is_err() {
                    break;
                }
            }
        });
        let _ = requests.send(Job::Prepare);
        Some(Self {
            preview_only: cfg.preview_only(),
            study_notes: cfg.study_notes,
            inflight: HashSet::new(),
            rechecked: HashSet::new(),
            requests,
            answers,
            pending: HashSet::new(),
            books_error,
            own_merged: bible.books_with_own().is_some() || bible.lang == bible::BOOKS_LANG,
            books: Arc::new(bible.books.clone()),
            bible,
        })
    }

    /// Quote for `r` from the cache, queueing a lookup on a miss. `Err`
    /// means "not known yet"; `Ok(None)` means "looked up, not found".
    fn quote(&mut self, r: &BibleRef) -> Result<Option<String>, ()> {
        match self.bible.cached(r) {
            Some(hit) => Ok(hit.map(|p| self.bible.quote_line(r, &p))),
            None => {
                self.request(Job::Verse(*r));
                Err(())
            }
        }
    }

    /// Study notes for `r` from the cache, queueing a lookup on a miss
    /// (`None` until it's answered, or if it failed). Cached English (or
    /// empty) notes are still shown while a due recheck for French ones
    /// runs in the background — at most once per session per reference.
    fn notes(&mut self, r: &BibleRef) -> Option<StudyNotes> {
        match self.bible.cached_notes(r) {
            Some(hit) => {
                if self.bible.notes_need_recheck(r) && self.rechecked.insert(r.key()) {
                    self.request(Job::Notes(*r));
                }
                hit
            }
            None => {
                self.request(Job::Notes(*r));
                None
            }
        }
    }

    fn request(&mut self, job: Job) {
        if self.inflight.insert(job.key()) {
            let _ = self.requests.send(job);
        }
    }

    /// `text` with its quotes recomputed from the cache. With `partial`,
    /// whatever's known is used and misses are simply left out; without it,
    /// a single miss returns `None` (the caller keeps `text` as it is).
    fn expand(&mut self, text: &str, partial: bool, whole_file: bool) -> Option<String> {
        let books = Arc::clone(&self.books);
        let mut complete = true;
        let mut lookup = |r: &BibleRef| {
            self.quote(r).unwrap_or_else(|()| {
                complete = false;
                None
            })
        };
        let out = if whole_file {
            books.expand_document(text, &mut lookup)
        } else {
            books.expand_body(text, &mut lookup)
        };
        (complete || partial).then_some(out)
    }
}

impl App {
    /// Insert mode: `body` with its quotes, or `None` to save it unchanged
    /// (`[bible]` off, preview mode, or some lookup still pending — in which
    /// case `path` is rewritten later by `flush_bible_pending`).
    pub(crate) fn bible_expand_for_save(&mut self, body: &str, path: &Path) -> Option<String> {
        let state = self.bible.as_mut().filter(|s| !s.preview_only)?;
        let expanded = state.expand(body, false, false);
        if expanded.is_none() {
            state.pending.insert(path.to_path_buf());
        }
        expanded
    }

    /// Insert mode, after an external edit: rewrites `path` in place if its
    /// quotes are out of date (frontmatter untouched).
    pub(crate) fn bible_apply_to_file(&mut self, path: &Path) {
        let Some(state) = self.bible.as_mut().filter(|s| !s.preview_only) else {
            return;
        };
        let Ok(contents) = std::fs::read_to_string(path) else {
            return;
        };
        match state.expand(&contents, false, true) {
            Some(new) if new != contents => {
                let _ = std::fs::write(path, new);
            }
            Some(_) => {}
            None => {
                state.pending.insert(path.to_path_buf());
            }
        }
    }

    /// What PREVIEW should render instead of `body` — quotes added (preview
    /// mode) and/or study notes under each quote (`study_notes`), as far as
    /// they're known — plus, per rendered line, the note line it comes
    /// from. `None` when there's nothing to add.
    pub(crate) fn bible_preview_body(&mut self, body: &str) -> Option<(String, Vec<usize>)> {
        let state = self.bible.as_mut()?;
        if !state.preview_only && !state.study_notes {
            return None;
        }
        let bible = Arc::clone(&state.bible);
        let books = Arc::clone(&state.books);
        let mut lines = bible::numbered(body);
        if state.preview_only {
            lines = books.expand_lines(lines, &mut |r| state.quote(r).ok().flatten());
        }
        if state.study_notes {
            lines = books.annotate_study_notes(lines, &mut |r| state.notes(r), &bible.lang, &|r| {
                bible.citation(r)
            })
        }
        let (text, origins): (Vec<String>, Vec<usize>) = lines.into_iter().unzip();
        Some((text.join("\n"), origins))
    }

    /// leader+`n`: shows/hides the study notes in PREVIEW, and remembers
    /// the choice in `config.toml`.
    pub(crate) fn toggle_bible_study_notes(&mut self) {
        let Some(state) = self.bible.as_mut() else {
            self.set_status("Bible study notes: enable [bible] in config.toml first".into());
            return;
        };
        state.study_notes = !state.study_notes;
        let on = state.study_notes;
        self.config.bible.study_notes = on;
        self.save_config();
        self.note_preview_cache = None;
        self.set_status(format!(
            "Bible study notes: {}",
            if on { "shown" } else { "hidden" }
        ));
    }

    /// leader+`v`: switches the quotes to the next of `[bible] languages`
    /// (remembered in `config.toml`). PREVIEW follows at once; in insert
    /// mode the selected note is requoted right away too, the others the
    /// next time they're saved.
    pub(crate) fn cycle_bible_language(&mut self) {
        if self.bible.is_none() {
            self.set_status("Bible language: enable [bible] in config.toml first".into());
            return;
        }
        let languages = &self.config.bible.languages;
        if languages.is_empty() {
            self.set_status("Bible language: [bible] languages is empty".into());
            return;
        }
        let current = bible::jw_lang(&self.config.bible.language);
        let next = languages
            .iter()
            .position(|l| bible::jw_lang(l) == current)
            .map_or(0, |i| (i + 1) % languages.len());
        let language = languages[next].clone();
        self.config.bible.language = language.clone();
        self.save_config();
        self.reset_bible();
        self.note_preview_cache = None;
        if !self.config.bible.preview_only() {
            if let Some(path) = self.selected_note().map(|n| n.path.clone()) {
                self.bible_apply_to_file(&path);
            }
        }
        self.set_status(format!(
            "Bible language: {language} ({})",
            bible::jw_lang(&language)
        ));
    }

    /// Drains the worker's answers; once nothing is in flight anymore,
    /// rewrites the notes that were waiting on them.
    pub(crate) fn poll_bible_channel(&mut self) {
        let Some(state) = self.bible.as_mut() else {
            return;
        };
        if let Some(e) = state.books_error.take() {
            self.set_status(format!("{e} — using the built-in book list"));
            return;
        }
        let mut answered = false;
        while let Ok(key) = state.answers.try_recv() {
            state.inflight.remove(&key);
            answered = true;
        }
        if !answered {
            return;
        }
        if !state.own_merged {
            if let Some(books) = state.bible.books_with_own() {
                state.books = Arc::new(books);
                state.own_merged = true;
            }
        }
        if state.preview_only || state.study_notes {
            self.note_preview_cache = None;
        }
        let verses_done = !state.inflight.iter().any(|k| k.starts_with("v:"));
        if !state.preview_only && verses_done && !state.pending.is_empty() {
            self.flush_bible_pending();
        }
    }

    fn flush_bible_pending(&mut self) {
        let Some(state) = self.bible.as_mut() else {
            return;
        };
        let pending: Vec<PathBuf> = state.pending.drain().collect();
        let mut touched: Vec<String> = Vec::new();
        for path in pending {
            // An encrypted notebook's file is ciphertext on disk — its
            // quotes are added on its next save instead.
            let Some(nb) = self.notebooks.iter().find(|nb| path.starts_with(&nb.path)) else {
                continue;
            };
            if self.config.encrypt_for(&nb.name) {
                continue;
            }
            let name = nb.name.clone();
            let Some(state) = self.bible.as_mut() else {
                return;
            };
            let Ok(contents) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Some(new) = state.expand(&contents, false, true) {
                if new != contents && std::fs::write(&path, new).is_ok() && !touched.contains(&name)
                {
                    touched.push(name);
                }
            } else {
                // A reference edited in since — wait for that lookup too.
                state.pending.insert(path);
            }
        }
        if !touched.is_empty() {
            self.refresh_notes_preserve_selection();
            for name in touched {
                self.note_changed(&name);
            }
        }
    }

    /// Rebuilds the `[bible]` state after a config change.
    pub(crate) fn reset_bible(&mut self) {
        self.bible = BibleState::from_config(&self.config);
    }
}
