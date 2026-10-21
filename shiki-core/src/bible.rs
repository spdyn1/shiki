//! Bible references in notes (`Jn 3:16`, `1 Jean 4:8, 16`, `Ps 83:18`) and
//! the verse text quoted under them — French New World Translation, looked
//! up either in the official EPUB downloaded once from jw.org (`nwt_F.epub`)
//! or online through jw.org's own Bible JSON endpoint.
//!
//! A quoted verse is a single generated line placed right under the line
//! holding its reference:
//!
//! ```text
//! Jn 3:16
//! > « Car Dieu a tellement aimé le monde … » — [Jean 3:16](https://www.jw.org/finder?…)
//! ```
//!
//! Generated lines are recognized by their shape (a `>` quote ending in a
//! `[citation](https://www.jw.org/…)` link) and recomputed on every pass, so
//! editing or deleting a reference updates or removes its quote — while any
//! `>` quote the user wrote by hand is left alone. Network access shells out
//! to `curl`, the same external-binary pattern `publish`/`voice` use.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// The default book list (`bible_books.toml`), written out as
/// `bible-livres.toml` in the config dir on first use so it can be edited.
pub const DEFAULT_BOOKS: &str = include_str!("bible_books.toml");

/// Study-notes cache, next to the verse cache.
const NOTES_FILE: &str = "study-notes";

/// How long a study-notes lookup that didn't find French notes is trusted
/// before asking jw.org again — the French study edition is still being
/// extended, so an English fallback (or nothing) can become French later.
const NOTES_RECHECK_SECS: u64 = 7 * 24 * 60 * 60;

/// Official download of the French NWT (2018 revision) EPUB — same verse
/// text as the study edition, minus its study notes.
const EPUB_URL: &str = "https://b.jw-cdn.org/apis/pub-media/GETPUBMEDIALINKS?output=json&pub=nwt&fileformat=EPUB&alllangs=0&langwritten=";

/// jw.org's own language code (`F`, `E`, `S`…) for a config value: a usual
/// ISO 639-1 code (`fr`, `en`, `es`…) for the common languages, or any jw.org
/// code given as is (`CHS`, `TPO`…) for the others.
pub fn jw_lang(lang: &str) -> String {
    let code = match lang.trim().to_lowercase().as_str() {
        "fr" => "F",
        "en" => "E",
        "es" => "S",
        "pt" => "T",
        "de" => "X",
        "it" => "I",
        "nl" => "O",
        "ru" => "U",
        "pl" => "P",
        "ro" => "M",
        "el" => "G",
        "sv" => "Z",
        "da" => "D",
        "no" | "nb" => "N",
        "fi" => "FI",
        "uk" => "K",
        "cs" => "B",
        "hu" => "H",
        "tr" => "TK",
        "ja" => "J",
        "ko" => "KO",
        "zh" => "CHS",
        "ar" => "A",
        "sw" => "SW",
        "ht" => "CR",
        _ => return lang.trim().to_uppercase(),
    };
    code.to_string()
}

/// Where the EPUB for jw.org language `lang` lives: `configured` (with
/// `{lang}` substituted) when set, else `{dir}/nwt_{lang}.epub`.
pub fn epub_path(dir: &Path, configured: &str, lang: &str) -> PathBuf {
    if configured.is_empty() {
        dir.join(format!("nwt_{lang}.epub"))
    } else {
        crate::process::expand_home(&configured.replace("{lang}", lang))
    }
}

/// The jw.org language the book list (`bible-livres.toml`) is written in —
/// for that language, citations use its names (`Psaume 83:18`); for any
/// other, the Bible's own book names in that language.
pub const BOOKS_LANG: &str = "F";

/// One reference: a single verse (`first == last`) or a contiguous range,
/// always within one chapter. `1 Jean 4:8, 16` yields two of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BibleRef {
    pub book: u8,
    pub chapter: u16,
    pub first: u16,
    pub last: u16,
}

impl BibleRef {
    /// Stable string key (`43:3:16-16`) for caches and channels.
    pub fn key(&self) -> String {
        format!(
            "{}:{}:{}-{}",
            self.book, self.chapter, self.first, self.last
        )
    }

    fn verse_id(&self, verse: u16) -> String {
        format!("{}{:03}{:03}", self.book, self.chapter, verse)
    }

    /// The reference a generated quote line was made for, read back from
    /// its link (`…&bible=43003016` or `…&bible=40024003-40024005`).
    pub fn from_quote(line: &str) -> Option<Self> {
        static RE: OnceLock<Regex> = OnceLock::new();
        let caps = RE
            .get_or_init(|| {
                Regex::new(r"bible=(\d{1,2})(\d{3})(\d{3})(?:-\d{1,2}\d{3}(\d{3}))?\)$")
                    .expect("valid regex")
            })
            .captures(line)?;
        let first = caps[3].parse().ok()?;
        Some(Self {
            book: caps[1].parse().ok().filter(|b| (1..=66).contains(b))?,
            chapter: caps[2].parse().ok()?,
            first,
            last: caps
                .get(4)
                .map_or(Some(first), |m| m.as_str().parse().ok())?,
        })
    }

    /// jw.org's own share link, which redirects to the verse in `edition`
    /// (`nwtsty` = study Bible, `nwt` = regular edition) in jw.org language
    /// `lang` (`F`, `E`…).
    pub fn link(&self, edition: &str, lang: &str) -> String {
        let mut range = self.verse_id(self.first);
        if self.last != self.first {
            range = format!("{range}-{}", self.verse_id(self.last));
        }
        format!("https://www.jw.org/finder?wtlocale={lang}&pub={edition}&bible={range}")
    }
}

/// Verse texts of one `BibleRef`, in order, plus the book's name as that
/// Bible prints it (`John`, `Juan`…) when the source gave one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Passage {
    pub verses: Vec<(u16, String)>,
    #[serde(default)]
    pub book: Option<String>,
}

impl Passage {
    pub fn new(verses: Vec<(u16, String)>) -> Self {
        Self { verses, book: None }
    }

    /// The generated quote line for `r` — verse numbers in bold only when
    /// the passage spans more than one verse.
    pub fn quote_line(&self, citation: &str, link: &str) -> String {
        let text = if self.verses.len() == 1 {
            self.verses[0].1.clone()
        } else {
            self.verses
                .iter()
                .map(|(n, t)| format!("**{n}** {t}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        format!("> {text} — [{citation}]({link})")
    }
}

fn normalize(s: &str) -> String {
    // Enough of NFD for French: map accented letters to their base letter.
    s.chars()
        .filter_map(|c| {
            let c = c.to_lowercase().next().unwrap_or(c);
            let base = match c {
                'à' | 'â' | 'ä' => 'a',
                'é' | 'è' | 'ê' | 'ë' => 'e',
                'î' | 'ï' => 'i',
                'ô' | 'ö' => 'o',
                'ù' | 'û' | 'ü' => 'u',
                'ç' => 'c',
                c if c.is_alphanumeric() => c,
                _ => return None,
            };
            Some(base)
        })
        .collect()
}

/// Lowercased, letters and digits only — accents kept (see `normalize`
/// for the folded form).
fn exact_key(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

#[derive(Deserialize)]
struct BookFile {
    livre: Vec<BookEntry>,
}

#[derive(Deserialize)]
struct BookEntry {
    nom: String,
    #[serde(default)]
    citation: Option<String>,
    #[serde(default)]
    abreviations: Vec<String>,
}

/// The 66 books: how each is cited, and every spelling recognized in notes
/// (keyed by `normalize`d form). Loaded from `bible-livres.toml`.
#[derive(Debug, Clone)]
pub struct Books {
    citations: Vec<String>,
    /// Spellings as written (lowercased, punctuation/spaces dropped, accents
    /// kept): German `1Kö` (Kings) and `1Ko` (Corinthians) stay apart.
    exact: HashMap<String, u8>,
    /// The same with accents folded too (`Re` for `Ré`) — `None` where
    /// folding makes two books' spellings meet, which then need the exact one.
    folded: HashMap<String, Option<u8>>,
}

impl Default for Books {
    fn default() -> Self {
        Self::parse(DEFAULT_BOOKS).expect("bundled bible_books.toml is valid")
    }
}

impl Books {
    /// Parses a book list, rejecting anything that would make references
    /// ambiguous or point at the wrong book: not exactly 66 entries, or one
    /// spelling shared by two books.
    pub fn parse(toml_text: &str) -> Result<Self> {
        let file: BookFile =
            toml::from_str(toml_text).map_err(|e| Error::Bible(format!("book list: {e}")))?;
        if file.livre.len() != 66 {
            return Err(Error::Bible(format!(
                "book list: expected 66 [[livre]] entries, found {}",
                file.livre.len()
            )));
        }
        let mut books = Self {
            citations: Vec::with_capacity(66),
            exact: HashMap::new(),
            folded: HashMap::new(),
        };
        for (i, entry) in file.livre.iter().enumerate() {
            let book = i as u8 + 1;
            for spelling in std::iter::once(&entry.nom).chain(&entry.abreviations) {
                let key = exact_key(spelling);
                if key.is_empty() {
                    continue;
                }
                match books.exact.insert(key, book) {
                    Some(other) if other != book => {
                        return Err(Error::Bible(format!(
                            "book list: '{spelling}' is used for both {} and {}",
                            file.livre[usize::from(other) - 1].nom,
                            entry.nom
                        )));
                    }
                    _ => {}
                }
                books.add_folded(normalize(spelling), Some(book));
            }
            books
                .citations
                .push(entry.citation.clone().unwrap_or_else(|| entry.nom.clone()));
        }
        Ok(books)
    }

    /// Reads `path`, first writing the default list there if it doesn't
    /// exist yet (so there's always a file to edit).
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(path, DEFAULT_BOOKS)?;
        }
        Self::parse(&std::fs::read_to_string(path)?).map_err(|e| match e {
            Error::Bible(msg) => Error::Bible(format!("{}: {msg}", path.display())),
            other => other,
        })
    }

    /// Adds `other`'s spellings — where they don't already mean something
    /// here, so on a clash this list (the user's own) wins.
    pub fn merge(&mut self, other: &Books) {
        for (key, &book) in &other.exact {
            self.exact.entry(key.clone()).or_insert(book);
        }
        for (key, &book) in &other.folded {
            if !self.folded.contains_key(key) {
                self.folded.insert(key.clone(), book);
            }
        }
    }

    /// Records a folded spelling, turning it ambiguous (`None`) when it
    /// already stands for another book.
    fn add_folded(&mut self, key: String, book: Option<u8>) {
        if key.is_empty() {
            return;
        }
        let slot = self.folded.entry(key).or_insert(book);
        if *slot != book {
            *slot = None;
        }
    }

    fn number(&self, name: &str) -> Option<u8> {
        self.exact
            .get(&exact_key(name))
            .copied()
            .or_else(|| self.folded.get(&normalize(name)).copied().flatten())
    }

    /// `Jean 3:16`, `Matthieu 24:3-5`, `Psaume 83:18`.
    pub fn citation(&self, r: &BibleRef) -> String {
        cite(&self.citations[usize::from(r.book) - 1], r)
    }

    /// The book named by a regex match's optional `1`–`3` prefix plus word,
    /// provided the word is capitalized.
    fn book_of(&self, prefix: Option<regex::Match>, name: &str) -> Option<u8> {
        let prefix = prefix.map_or("", |m| m.as_str().trim());
        name.chars()
            .next()
            .is_some_and(char::is_uppercase)
            .then(|| self.number(&format!("{prefix}{name}")))
            .flatten()
    }

    /// Whether `s` begins with a reference to a real book (`1 Jean 4:8 …`).
    fn starts_ref(&self, s: &str) -> bool {
        head_regex().captures(s).is_some_and(|c| {
            c.get(0).expect("group 0").start() == 0 && self.book_of(c.get(1), &c[2]).is_some()
        })
    }

    /// Every reference on `line`. The book must start with a capital letter
    /// (`Le 19:18` is Leviticus, `le 8:15` is a time of day), and must not
    /// be glued to a preceding word. A `, n` list continues the same chapter
    /// unless what follows is itself a new reference (`Lv 19:18, 1 Jean 4:8`).
    pub fn find_refs(&self, line: &str) -> Vec<BibleRef> {
        let mut refs = Vec::new();
        let mut pos = 0;
        while let Some(caps) = head_regex().captures_at(line, pos) {
            let whole = caps.get(0).expect("group 0");
            pos = whole.end();
            let starts_word = line[..whole.start()]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric() && c != '>');
            let book = self.book_of(caps.get(1), &caps[2]);
            let (Some(book), true, Ok(chapter)) = (book, starts_word, caps[3].parse::<u16>())
            else {
                continue;
            };
            refs.extend(range(
                book,
                chapter,
                &caps[4],
                caps.get(5).map(|m| m.as_str()),
            ));
            while let Some(item) = list_item_regex().captures(&line[pos..]) {
                let end = pos + item.get(0).expect("group 0").end();
                // `, 1 Jean 4:8` — that "1" is the start of the next
                // reference, not verse 1 of this chapter.
                let item_start = pos + item.get(1).expect("verse group").start();
                if self.starts_ref(&line[item_start..]) {
                    break;
                }
                refs.extend(range(
                    book,
                    chapter,
                    &item[1],
                    item.get(2).map(|m| m.as_str()),
                ));
                pos = end;
            }
        }
        refs
    }

    /// Recomputes every generated quote in a note body: drops the existing
    /// ones, then re-adds one under each reference `lookup` can resolve.
    /// Lines that are themselves quotes are never scanned for references.
    pub fn expand_body(
        &self,
        body: &str,
        lookup: &mut dyn FnMut(&BibleRef) -> Option<String>,
    ) -> String {
        let lines = self.expand_lines(numbered(body), lookup);
        lines
            .into_iter()
            .map(|(line, _)| line)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `expand_body` over `(line, origin)` pairs — `origin` being the line
    /// of the note each output line comes from (a quote's origin is its
    /// reference's line), so a rendered row can be traced back to the note.
    pub fn expand_lines(
        &self,
        lines: Vec<(String, usize)>,
        lookup: &mut dyn FnMut(&BibleRef) -> Option<String>,
    ) -> Vec<(String, usize)> {
        let mut out = Vec::with_capacity(lines.len());
        for (line, origin) in lines {
            if is_generated(&line) {
                continue;
            }
            let quotes: Vec<String> = if line.trim_start().starts_with('>') {
                Vec::new()
            } else {
                self.find_refs(&line)
                    .iter()
                    .filter_map(&mut *lookup)
                    .collect()
            };
            out.push((line, origin));
            out.extend(quotes.into_iter().map(|q| (q, origin)));
        }
        out
    }

    /// Puts each generated quote's study notes right under it, as a
    /// collapsible `<details>` block (PREVIEW renders and folds those).
    pub fn annotate_study_notes(
        &self,
        lines: Vec<(String, usize)>,
        notes: &mut dyn FnMut(&BibleRef) -> Option<StudyNotes>,
        lang: &str,
        citation: &dyn Fn(&BibleRef) -> String,
    ) -> Vec<(String, usize)> {
        let mut out = Vec::with_capacity(lines.len());
        for (line, origin) in lines {
            let block = BibleRef::from_quote(&line)
                .filter(|_| is_generated(&line))
                .and_then(|r| Some((r, notes(&r)?)))
                .filter(|(_, n)| !n.notes.is_empty())
                .map(|(r, n)| study_block(&citation(&r), &n, lang));
            out.push((line, origin));
            out.extend(block.into_iter().flatten().map(|l| (l, origin)));
        }
        out
    }

    /// `expand_body` over a whole file, leaving its YAML frontmatter
    /// untouched (a `title: Jean 3:16` must not grow a quote inside it).
    pub fn expand_document(
        &self,
        contents: &str,
        lookup: &mut dyn FnMut(&BibleRef) -> Option<String>,
    ) -> String {
        let split = contents
            .strip_prefix("---\n")
            .and_then(|rest| rest.find("\n---\n").map(|i| 4 + i + 5));
        match split {
            Some(at) => format!(
                "{}{}",
                &contents[..at],
                self.expand_body(&contents[at..], lookup)
            ),
            None => self.expand_body(contents, lookup),
        }
    }
}

/// The collapsible block of `notes` under a quote. `lang` is the Bible's
/// language: notes in any other one (the English fallback) say so.
fn study_block(citation: &str, notes: &StudyNotes, lang: &str) -> Vec<String> {
    let title = match notes.lang.as_str() {
        l if l == lang => "Notes d’étude".to_string(),
        "E" => "Notes d’étude (en anglais)".to_string(),
        other => format!("Notes d’étude ({other})"),
    };
    let mut block = vec![
        "<details>".to_string(),
        format!(
            "<summary>{title} — {citation} ({})</summary>",
            notes.notes.len()
        ),
    ];
    block.extend(
        notes
            .notes
            .iter()
            .map(|n| format!("- **v. {}** {}", n.verse, n.text)),
    );
    block.push("</details>".to_string());
    block
}

/// `{name} {chapter}:{verses}` — `Jean 3:16`, `John 24:3-5`.
fn cite(name: &str, r: &BibleRef) -> String {
    if r.first == r.last {
        format!("{name} {}:{}", r.chapter, r.first)
    } else {
        format!("{name} {}:{}-{}", r.chapter, r.first, r.last)
    }
}

/// `body`'s lines paired with their own line numbers — the starting point
/// for `Books::expand_lines`/`annotate_study_notes`.
pub fn numbered(body: &str) -> Vec<(String, usize)> {
    body.split('\n')
        .enumerate()
        .map(|(i, l)| (l.to_string(), i))
        .collect()
}

fn head_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"([1-3] ?)?(\p{L}+)\.? ?(\d+) ?: ?(\d+)(?: ?[-–] ?(\d+))?")
            .expect("valid regex")
    })
}

fn list_item_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^ ?, ?(\d+)(?: ?[-–] ?(\d+))?").expect("valid regex"))
}

fn range(book: u8, chapter: u16, first: &str, last: Option<&str>) -> Option<BibleRef> {
    let first: u16 = first.parse().ok()?;
    let last: u16 = last.map_or(Some(first), |l| l.parse().ok())?;
    (first >= 1 && last >= first).then_some(BibleRef {
        book,
        chapter,
        first,
        last,
    })
}

/// A quote line this module generated (and so may freely regenerate).
pub fn is_generated(line: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^> .* — \[[^\]]+\]\(https://www\.jw\.org/[^)]*\)$").expect("valid regex")
    })
    .is_match(line)
}

// ---------------------------------------------------------------- local EPUB

/// All verses of the EPUB, keyed by `(book, chapter, verse)`, and the book
/// names it prints (index = book number - 1).
pub struct LocalBible {
    verses: HashMap<(u8, u16, u16), String>,
    names: Vec<String>,
}

impl LocalBible {
    pub fn load(epub: &Path) -> Result<Self> {
        let err = |e: &dyn std::fmt::Display| Error::Bible(format!("{}: {e}", epub.display()));
        let file = std::fs::File::open(epub)?;
        let mut zip = zip::ZipArchive::new(file).map_err(|e| err(&e))?;
        let mut read = |name: &str| -> Result<String> {
            let mut s = String::new();
            zip.by_name(name)
                .map_err(|e| err(&e))?
                .read_to_string(&mut s)?;
            Ok(s)
        };
        let container = read("META-INF/container.xml")?;
        let opf_path = Regex::new(r#"full-path="([^"]+)""#)
            .expect("valid regex")
            .captures(&container)
            .map(|c| c[1].to_string())
            .ok_or_else(|| err(&"no rootfile in container.xml"))?;
        let base = opf_path
            .rsplit_once('/')
            .map_or(String::new(), |(d, _)| format!("{d}/"));
        let opf = read(&opf_path)?;

        let item = Regex::new(r"<item\b[^>]*>").expect("valid regex");
        let id = Regex::new(r#"\bid="([^"]+)""#).expect("valid regex");
        let href = Regex::new(r#"\bhref="([^"]+)""#).expect("valid regex");
        let attr = |re: &Regex, tag: &str| re.captures(tag).map(|c| c[1].to_string());
        let manifest: HashMap<String, String> = item
            .find_iter(&opf)
            .filter_map(|m| Some((attr(&id, m.as_str())?, attr(&href, m.as_str())?)))
            .collect();
        let spine: Vec<String> = Regex::new(r#"<itemref\b[^>]*idref="([^"]+)""#)
            .expect("valid regex")
            .captures_iter(&opf)
            .filter_map(|c| manifest.get(&c[1]).cloned())
            .collect();

        let marker =
            Regex::new(r#"<span id="chapter(\d+)_verse(\d+)"></span>"#).expect("valid regex");
        let book_name = Regex::new(r#"w_biblebookname"><a[^>]*>([^<]+)</a>"#).expect("valid regex");
        let mut verses = HashMap::new();
        let mut names = Vec::new();
        let mut book = 0u8;
        for href in spine {
            let page = read(&format!("{base}{href}"))?;
            let Some(body_at) = page.find("<body") else {
                continue;
            };
            let body = &page[body_at..];
            if !marker.is_match(body) {
                continue;
            }
            // Each book starts on a non-split page holding its 1:1 marker;
            // later chapters live on `-splitN` pages of the same book.
            if body.contains(r#"id="chapter1_verse1""#) && !href.contains("-split") {
                book += 1;
                let name = book_name.captures(body).map(|c| unescape(c[1].trim()));
                names.push(name.unwrap_or_default());
            }
            // A page is one chapter, so a verse never continues onto the
            // next page — whatever precedes the first marker (navigation,
            // book title, a psalm's superscription) is never verse text.
            let mut marks = marker.captures_iter(body).peekable();
            while let Some(m) = marks.next() {
                let start = m.get(0).expect("group 0").end();
                let end = marks
                    .peek()
                    .map_or(body.len(), |n| n.get(0).expect("group 0").start());
                let (Ok(ch), Ok(v)) = (m[1].parse(), m[2].parse()) else {
                    continue;
                };
                let text = clean_verse(&body[start..end]);
                if text.chars().any(char::is_alphabetic) {
                    verses.insert((book, ch, v), text);
                }
            }
        }
        if book != 66 {
            return Err(err(&format!("expected 66 books, found {book}")));
        }
        Ok(Self { verses, names })
    }

    pub fn passage(&self, r: &BibleRef) -> Option<Passage> {
        let verses: Vec<(u16, String)> = (r.first..=r.last)
            .filter_map(|v| Some((v, self.verses.get(&(r.book, r.chapter, v))?.clone())))
            .collect();
        let book = self
            .names
            .get(usize::from(r.book) - 1)
            .filter(|n| !n.is_empty())
            .cloned();
        (!verses.is_empty()).then_some(Passage { verses, book })
    }
}

/// The NWT's own "Names and Order of the Books" table (`nwt` p. 40 — the
/// same page id in every language): each book's name and official
/// abbreviation, in canonical order.
pub fn book_table(epub: &Path) -> Result<Vec<(String, String)>> {
    let err = |e: &dyn std::fmt::Display| Error::Bible(format!("{}: {e}", epub.display()));
    let mut zip = zip::ZipArchive::new(std::fs::File::open(epub)?).map_err(|e| err(&e))?;
    let mut page = String::new();
    zip.by_name("OEBPS/1001061103.xhtml")
        .map_err(|e| err(&e))?
        .read_to_string(&mut page)?;
    let row = Regex::new(r"(?s)<tr>(.*?)</tr>").expect("valid regex");
    let cell = Regex::new(r"(?s)<td>(.*?)</td>").expect("valid regex");
    let tag = Regex::new(r"<[^>]+>").expect("valid regex");
    let text = |html: &str| unescape(tag.replace_all(html, "").trim()).replace('\u{200b}', "");
    let rows: Vec<(String, String)> = row
        .captures_iter(&page)
        .filter_map(|r| {
            let cells: Vec<String> = cell.captures_iter(&r[1]).map(|c| text(&c[1])).collect();
            (cells.len() >= 3 && !cells[0].is_empty()).then(|| (cells[0].clone(), cells[2].clone()))
        })
        .collect();
    if rows.len() != 66 {
        return Err(err(&format!(
            "book table: expected 66 rows, found {}",
            rows.len()
        )));
    }
    Ok(rows)
}

/// A `bible-livres-{lang}.toml` for `rows` (from `book_table`).
pub fn books_toml(lang: &str, rows: &[(String, String)]) -> String {
    let quoted = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    let mut out = format!(
        "# Livres de la Bible en langue jw.org « {lang} », générés depuis la page\n\
         # « Noms et ordre des livres » de la Traduction du monde nouveau dans\n\
         # cette langue. Même format que bible-livres.toml ; modifiable. Ces\n\
         # abréviations s'ajoutent à celles de bible-livres.toml (qui l'emporte\n\
         # en cas de conflit), et `nom` sert aux citations dans cette langue.\n"
    );
    for (name, abbr) in rows {
        out.push_str(&format!("\n[[livre]]\nnom = {}\n", quoted(name)));
        let abbrs: Vec<String> = abbr
            .split([',', ';'])
            .map(str::trim)
            .filter(|a| !a.is_empty() && normalize(a) != normalize(name))
            .map(quoted)
            .collect();
        out.push_str(&format!("abreviations = [{}]\n", abbrs.join(", ")));
    }
    out
}

/// Where the book list of jw.org language `lang` lives, next to the main
/// one (`bible-livres.toml`): `None` for the main list's own language.
pub fn lang_books_path(main: &Path, lang: &str) -> Option<PathBuf> {
    (lang != BOOKS_LANG).then(|| main.with_file_name(format!("bible-livres-{lang}.toml")))
}

fn clean_verse(html: &str) -> String {
    static RES: OnceLock<[Regex; 5]> = OnceLock::new();
    let [footer, noteref, numbers, skipped, tag] = RES.get_or_init(|| {
        [
            // Footnotes/end-of-page material after the last verse.
            Regex::new(r#"(?s)(<aside|<div class="(groupFootnote|footnote)|<footer).*"#).unwrap(),
            Regex::new(r#"(?s)<a epub:type="noteref"[^>]*>.*?</a>"#).unwrap(),
            Regex::new(r#"(?s)<span class="w_ch">.*?</span>|<sup>.*?</sup>"#).unwrap(),
            // Psalm superscriptions (`sw`), Psalm 119 letter headings
            // (`ss`), and any heading.
            Regex::new(r#"(?s)<p[^>]*class="[^"]*\b(sw|ss)\b[^"]*".*?</p>|<h\d.*?</h\d>"#).unwrap(),
            Regex::new(r"<[^>]+>").unwrap(),
        ]
    });
    let s = footer.replace(html, "");
    let s = noteref.replace_all(&s, "");
    let s = numbers.replace_all(&s, "");
    let s = skipped.replace_all(&s, " ");
    let s = s.replace("</p>", " </p>").replace("<br", " <br");
    let s = tag.replace_all(&s, "");
    let s = unescape(&s).replace('\u{200b}', "");
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';').filter(|&e| e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            e if e.starts_with("#x") => u32::from_str_radix(&e[2..], 16)
                .ok()
                .and_then(char::from_u32),
            e if e.starts_with('#') => e[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

// ------------------------------------------------------------------- network

fn curl(url: &str) -> Result<String> {
    let out = std::process::Command::new("curl")
        .args(["-sSfL", "--max-time", "15", "-A", "Mozilla/5.0", url])
        .output()
        .map_err(|e| Error::Bible(format!("curl: {e}")))?;
    if !out.status.success() {
        return Err(Error::Bible(format!(
            "curl {url}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    String::from_utf8(out.stdout).map_err(|e| Error::Bible(e.to_string()))
}

/// The base of jw.org's Bible JSON endpoint for `edition` in `lang` —
/// localized (`/fr/bibliothèque/bible/bible-d-etude/livres`,
/// `/en/library/bible/study-bible/books`…), so it's found by following the
/// language-neutral share link once and remembered for the session.
fn api_base(edition: &str, lang: &str) -> Result<String> {
    static BASES: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let bases = BASES.get_or_init(Mutex::default);
    let key = format!("{edition}/{lang}");
    if let Some(base) = bases.lock().ok().and_then(|b| b.get(&key).cloned()) {
        return Ok(base);
    }
    let genesis = BibleRef {
        book: 1,
        chapter: 1,
        first: 1,
        last: 1,
    };
    let out = std::process::Command::new("curl")
        .args([
            "-sSL",
            "--max-time",
            "15",
            "-A",
            "Mozilla/5.0",
            "-o",
            "/dev/null",
        ])
        .args(["-w", "%{url_effective}", &genesis.link(edition, lang)])
        .output()
        .map_err(|e| Error::Bible(format!("curl: {e}")))?;
    let landed = String::from_utf8_lossy(&out.stdout);
    // `…/livres/genese/1/#v1001001` → `…/livres`
    let path = landed.split('#').next().unwrap_or("").trim_end_matches('/');
    let base = path
        .strip_suffix("/1")
        .and_then(|p| p.rsplit_once('/'))
        .map(|(base, _book)| base.to_string())
        .ok_or_else(|| Error::Bible(format!("no {edition} Bible in language {lang} on jw.org")))?;
    if let Ok(mut b) = bases.lock() {
        b.insert(key, base.clone());
    }
    Ok(base)
}

/// Fetches `r` from jw.org's Bible JSON endpoint (`edition` = `nwtsty`/`nwt`)
/// in jw.org language `lang`.
pub fn fetch_online(r: &BibleRef, edition: &str, lang: &str) -> Result<Passage> {
    let url = format!(
        "{}/json/data/{}-{}",
        api_base(edition, lang)?,
        r.verse_id(r.first),
        r.verse_id(r.last)
    );
    let json: serde_json::Value =
        serde_json::from_str(&curl(&url)?).map_err(|e| Error::Bible(e.to_string()))?;
    let range = json["ranges"]
        .as_object()
        .and_then(|ranges| ranges.values().next())
        .ok_or_else(|| Error::Bible(format!("{}: no verses in response", r.key())))?;
    let verses = range["verses"]
        .as_array()
        .ok_or_else(|| Error::Bible(format!("{}: no verses in response", r.key())))?;
    // `John 3:16` → `John`
    let book = range["citation"].as_str().map(|c| {
        let c = unescape(c).replace(['\u{200b}', '\u{a0}'], " ");
        c.trim_end_matches(|ch: char| {
            ch.is_ascii_digit() || matches!(ch, ':' | '-' | '–' | ' ' | ',')
        })
        .trim()
        .to_string()
    });
    static RES: OnceLock<[Regex; 4]> = OnceLock::new();
    let [numbers, breaks, xref, tag] = RES.get_or_init(|| {
        [
            Regex::new(r#"(?s)<sup.*?</sup>|<span class="chapterNum">.*?</span>"#).unwrap(),
            Regex::new(r#"<span class="(newblock|parabreak)"></span>"#).unwrap(),
            Regex::new(r"(?s)<xref.*?</xref>").unwrap(),
            Regex::new(r"<[^>]+>").unwrap(),
        ]
    });
    let passage: Vec<(u16, String)> = verses
        .iter()
        .filter_map(|v| {
            let n = u16::try_from(v["verseNumber"].as_u64()?).ok()?;
            let html = v["content"].as_str()?;
            let s = numbers.replace_all(html, "");
            let s = breaks.replace_all(&s, " ");
            let s = xref.replace_all(&s, "");
            let s = tag.replace_all(&s, "");
            let s = unescape(&s).replace('\u{200b}', "");
            Some((n, s.split_whitespace().collect::<Vec<_>>().join(" ")))
        })
        .collect();
    if passage.is_empty() {
        return Err(Error::Bible(format!("{}: empty passage", r.key())));
    }
    Ok(Passage {
        verses: passage,
        book: book.filter(|b| !b.is_empty()),
    })
}

/// One study note of the NWT study edition, attached to a verse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StudyNote {
    pub verse: u16,
    /// Markdown: the note's headword in bold, Greek/Hebrew terms in italics.
    pub text: String,
}

/// The study notes found for one reference, with where and when they were
/// found — what `study-notes.json` stores per reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StudyNotes {
    /// jw.org language of the notes: the Bible's own, or `E` when English
    /// filled in for it — the English study edition covers more books than
    /// most others (Matthew–James vs. Matthew–Colossians in French, 2026).
    pub lang: String,
    /// Unix time of the lookup.
    pub checked: u64,
    /// Empty when neither language has notes on these verses.
    pub notes: Vec<StudyNote>,
}

impl StudyNotes {
    /// Anything short of notes in `lang` (the Bible's language) is asked
    /// for again once `NOTES_RECHECK_SECS` have passed.
    pub fn needs_recheck(&self, now: u64, lang: &str) -> bool {
        (self.lang != lang || self.notes.is_empty())
            && now.saturating_sub(self.checked) >= NOTES_RECHECK_SECS
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `r`'s study notes in `lang`, or in English when `lang` has none (yet).
pub fn fetch_study_notes(r: &BibleRef, lang: &str) -> Result<StudyNotes> {
    let checked = unix_now();
    // A language without a study edition at all just has no notes.
    let own = fetch_study_notes_in(r, lang).unwrap_or_default();
    if !own.is_empty() {
        return Ok(StudyNotes {
            lang: lang.to_string(),
            checked,
            notes: own,
        });
    }
    if lang != "E" {
        if let Ok(english) = fetch_study_notes_in(r, "E") {
            if !english.is_empty() {
                return Ok(StudyNotes {
                    lang: "E".into(),
                    checked,
                    notes: english,
                });
            }
        }
    }
    Ok(StudyNotes {
        lang: lang.to_string(),
        checked,
        notes: Vec::new(),
    })
}

/// The study notes on the verses of `r`, from the study edition on jw.org
/// in `lang` (they exist in no openly downloadable format). Many verses
/// have none.
fn fetch_study_notes_in(r: &BibleRef, lang: &str) -> Result<Vec<StudyNote>> {
    let url = format!(
        "{}/json/data/{}-{}",
        api_base("nwtsty", lang)?,
        r.verse_id(r.first),
        r.verse_id(r.last)
    );
    let json: serde_json::Value =
        serde_json::from_str(&curl(&url)?).map_err(|e| Error::Bible(e.to_string()))?;
    let range = json["ranges"]
        .as_object()
        .and_then(|ranges| ranges.values().next())
        .ok_or_else(|| Error::Bible(format!("{}: no data in response", r.key())))?;
    let first = r.verse_id(r.first);
    let last = r.verse_id(r.last);
    let mut notes = Vec::new();
    for c in range["commentaries"].as_array().into_iter().flatten() {
        let (Some(source), Some(html)) = (c["source"].as_str(), c["content"].as_str()) else {
            continue;
        };
        // Same-width ids, so string order is verse order.
        if source.len() != first.len() || source < first.as_str() || source > last.as_str() {
            continue;
        }
        let Ok(verse) = source[source.len() - 3..].parse() else {
            continue;
        };
        // One entry can hold several notes, one `<p>` per headword
        // (`aimé :`, `le monde :` …) — keep them apart.
        notes.extend(
            html.split("</p>")
                .map(html_to_markdown)
                .filter(|text| !text.is_empty())
                .map(|text| StudyNote { verse, text }),
        );
    }
    Ok(notes)
}

/// Study-note HTML to one line of Markdown: `<strong>` → `**`, `<em>` →
/// `*`, links reduced to their text, everything else dropped.
fn html_to_markdown(html: &str) -> String {
    static RES: OnceLock<[Regex; 3]> = OnceLock::new();
    let [strong, em, tag] = RES.get_or_init(|| {
        [
            Regex::new(r"(?s)<strong>(.*?)</strong>").unwrap(),
            Regex::new(r"(?s)<em>(.*?)</em>").unwrap(),
            Regex::new(r"<[^>]+>").unwrap(),
        ]
    });
    let s = strong.replace_all(html, "**$1**");
    let s = em.replace_all(&s, "*$1*");
    let s = s.replace("</p>", " ");
    let s = tag.replace_all(&s, "");
    let s = unescape(&s).replace(['\u{200b}'], "");
    // ASCII-only, so French non-breaking spaces (`juge :`, `« mot »`) stay.
    s.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
}

/// Downloads the official NWT EPUB in jw.org language `lang` to `dest`
/// (resolving the current file URL through jw.org's publication-media API
/// first).
pub fn download_epub(dest: &Path, lang: &str) -> Result<()> {
    let json: serde_json::Value = serde_json::from_str(&curl(&format!("{EPUB_URL}{lang}"))?)
        .map_err(|e| Error::Bible(e.to_string()))?;
    let url = json["files"][lang]["EPUB"][0]["file"]["url"]
        .as_str()
        .ok_or_else(|| Error::Bible(format!("no EPUB of the NWT in language {lang} on jw.org")))?;
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = dest.with_extension("epub.part");
    let status = std::process::Command::new("curl")
        .args(["-sSfL", "--max-time", "600", "-o"])
        .arg(&tmp)
        .arg(url)
        .status()
        .map_err(|e| Error::Bible(format!("curl: {e}")))?;
    if !status.success() {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::Bible(format!("download of {url} failed")));
    }
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

// --------------------------------------------------------------------- facade

/// Where verses come from: the local EPUB first (falling back to jw.org for
/// anything missing), or jw.org only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Local,
    Online,
}

/// Resolves references to passages with an in-memory cache, persisted to
/// `cache_file` for online lookups. Thread-safe: the TUI shares one behind
/// an `Arc` and resolves misses on background threads.
pub struct Bible {
    pub source: Source,
    pub edition: String,
    /// jw.org language code of the text (`F`, `E`…) — see `jw_lang`.
    pub lang: String,
    /// The spellings recognized in notes: the user's list, plus this
    /// language's official ones when its list was already available.
    pub books: Books,
    /// This language's own book list (`bible-livres-{lang}.toml`, generated
    /// from its EPUB) — used for citations. `None` for the user list's
    /// own language.
    lang_books_file: Option<PathBuf>,
    lang_books: OnceLock<Option<Books>>,
    epub: PathBuf,
    cache_file: PathBuf,
    local: OnceLock<Option<LocalBible>>,
    cache: Mutex<HashMap<String, Option<Passage>>>,
    /// Study notes per reference key, persisted next to `cache_file`.
    /// `None` = the lookup failed this session (retried next session).
    notes: Mutex<HashMap<String, Option<StudyNotes>>>,
}

impl Bible {
    /// `dir` holds the per-language caches (`verses-F.json`,
    /// `study-notes-F.json`); `epub` is this language's EPUB, `books_path`
    /// the user's book list (`bible-livres.toml`).
    pub fn new(
        source: Source,
        edition: String,
        lang: String,
        books: Books,
        books_path: &Path,
        epub: PathBuf,
        dir: &Path,
    ) -> Self {
        let cache_file = dir.join(format!("verses-{lang}.json"));
        let cache = std::fs::read_to_string(&cache_file)
            .ok()
            .and_then(|s| serde_json::from_str::<HashMap<String, Passage>>(&s).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| (k, Some(v)))
            .collect();
        let notes = std::fs::read_to_string(dir.join(format!("{NOTES_FILE}-{lang}.json")))
            .ok()
            .and_then(|s| serde_json::from_str::<HashMap<String, StudyNotes>>(&s).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| (k, Some(v)))
            .collect();
        let lang_books_file = lang_books_path(books_path, &lang);
        let mut bible = Self {
            source,
            edition,
            lang,
            books,
            lang_books_file,
            lang_books: OnceLock::new(),
            epub,
            cache_file,
            local: OnceLock::new(),
            cache: Mutex::new(cache),
            notes: Mutex::new(notes),
        };
        // Cheap when the list or the EPUB is already on disk (one small
        // page); otherwise it's built after the EPUB's first download.
        let ready =
            bible.lang_books_file.as_ref().is_some_and(|f| f.exists()) || bible.epub.exists();
        if ready {
            if let Some(own) = bible.lang_books().cloned() {
                bible.books.merge(&own);
            }
        }
        bible
    }

    /// This language's book list, read from `bible-livres-{lang}.toml` or,
    /// the first time, generated there from the EPUB's own table (blocks
    /// on disk I/O — the worker calls it after a download).
    pub fn lang_books(&self) -> Option<&Books> {
        self.lang_books
            .get_or_init(|| {
                let file = self.lang_books_file.as_ref()?;
                if !file.exists() {
                    let rows = book_table(&self.epub).ok()?;
                    std::fs::write(file, books_toml(&self.lang, &rows)).ok()?;
                }
                Books::load(file).ok()
            })
            .as_ref()
    }

    /// Gets this language ready up front (call off the UI thread): with a
    /// local source, its EPUB is downloaded/loaded and its own book list
    /// built from it — rather than waiting for the first lookup that
    /// misses the cache.
    pub fn prepare(&self) {
        if self.source == Source::Local {
            self.local.get_or_init(|| self.load_local().ok());
        }
        self.lang_books();
    }

    /// The spellings to recognize once this language's own list has become
    /// available after `new` (the EPUB's first download): the user's list
    /// merged with it. `None` while it isn't — or when it was merged
    /// already in `new`, or there's none to merge.
    pub fn books_with_own(&self) -> Option<Books> {
        let own = self.lang_books.get()?.as_ref()?;
        let mut books = self.books.clone();
        books.merge(own);
        Some(books)
    }

    /// How `r` is cited in this Bible's language (no I/O once known).
    pub fn citation(&self, r: &BibleRef) -> String {
        match self.lang_books.get().and_then(Option::as_ref) {
            Some(own) => own.citation(r),
            None => self.books.citation(r),
        }
    }

    /// Study notes for `r` if already known — never blocks. `Some(None)`
    /// means the lookup failed this session.
    pub fn cached_notes(&self, r: &BibleRef) -> Option<Option<StudyNotes>> {
        self.notes.lock().ok()?.get(&r.key()).cloned()
    }

    /// Whether `r`'s cached notes should be looked up again (see
    /// `StudyNotes::needs_recheck`).
    pub fn notes_need_recheck(&self, r: &BibleRef) -> bool {
        matches!(self.cached_notes(r), Some(Some(n)) if n.needs_recheck(unix_now(), &self.lang))
    }

    /// Fetches `r`'s study notes from jw.org (call off the UI thread) and
    /// caches them — on disk too, so a verse seen once stays available
    /// offline. A due recheck that fails (offline) keeps what was cached.
    pub fn resolve_notes(&self, r: &BibleRef) -> Option<StudyNotes> {
        let cached = self.cached_notes(r);
        match &cached {
            Some(Some(n)) if !n.needs_recheck(unix_now(), &self.lang) => return cached.flatten(),
            Some(None) => return None,
            _ => {}
        }
        let fetched = fetch_study_notes(r, &self.lang).ok().or(cached.flatten());
        if let Ok(mut notes) = self.notes.lock() {
            notes.insert(r.key(), fetched.clone());
            if fetched.is_some() {
                let found: HashMap<&String, &StudyNotes> = notes
                    .iter()
                    .filter_map(|(k, v)| Some((k, v.as_ref()?)))
                    .collect();
                if let Ok(json) = serde_json::to_string(&found) {
                    let path = self
                        .cache_file
                        .with_file_name(format!("{NOTES_FILE}-{}.json", self.lang));
                    let _ = std::fs::write(path, json);
                }
            }
        }
        fetched
    }

    pub fn epub_path(&self) -> &Path {
        &self.epub
    }

    /// Already-known result for `r`, without any I/O: `Some(Some(_))`
    /// resolved, `Some(None)` known-unresolvable, `None` not looked up yet.
    pub fn cached(&self, r: &BibleRef) -> Option<Option<Passage>> {
        self.cache.lock().ok()?.get(&r.key()).cloned()
    }

    /// Looks `r` up (EPUB parse and/or network — call off the UI thread)
    /// and caches the outcome.
    pub fn resolve(&self, r: &BibleRef) -> Option<Passage> {
        if let Some(hit) = self.cached(r) {
            return hit;
        }
        let local = match self.source {
            Source::Local => {
                let local = self.local.get_or_init(|| self.load_local().ok()).as_ref();
                // The EPUB is surely on disk now: build this language's
                // book list from it if that hasn't happened yet.
                self.lang_books();
                local.and_then(|b| b.passage(r))
            }
            Source::Online => None,
        };
        let from_network = local.is_none();
        let passage = local.or_else(|| fetch_online(r, &self.edition, &self.lang).ok());
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(r.key(), passage.clone());
            if from_network && passage.is_some() {
                self.persist(&cache);
            }
        }
        passage
    }

    /// The EPUB, downloaded from jw.org first if it isn't there yet (the
    /// same fetch-on-first-use `publish`'s `pretty-pdf` gets).
    pub fn load_local(&self) -> Result<LocalBible> {
        if !self.epub.exists() {
            download_epub(&self.epub, &self.lang)?;
        }
        LocalBible::load(&self.epub)
    }

    /// The generated quote line for `r`, cited in this Bible's language:
    /// its book list, else the name the source printed, else the user's
    /// list.
    pub fn quote_line(&self, r: &BibleRef, passage: &Passage) -> String {
        let own = self.lang_books.get().and_then(Option::as_ref);
        let citation = match (own, &passage.book) {
            (Some(own), _) => own.citation(r),
            (None, Some(name)) if self.lang != BOOKS_LANG => cite(name, r),
            _ => self.books.citation(r),
        };
        passage.quote_line(&citation, &r.link(&self.edition, &self.lang))
    }

    /// The quote line for `r` if already cached — never blocks.
    pub fn cached_quote(&self, r: &BibleRef) -> Option<String> {
        self.cached(r).flatten().map(|p| self.quote_line(r, &p))
    }

    fn persist(&self, cache: &HashMap<String, Option<Passage>>) {
        let found: HashMap<&String, &Passage> = cache
            .iter()
            .filter_map(|(k, v)| Some((k, v.as_ref()?)))
            .collect();
        if let Some(dir) = self.cache_file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string(&found) {
            let _ = std::fs::write(&self.cache_file, json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find_refs(line: &str) -> Vec<BibleRef> {
        Books::default().find_refs(line)
    }

    /// A generated quote line for `r`, as the French Bible would write it.
    fn quote(r: &BibleRef, text: &str) -> String {
        Passage::new(vec![(r.first, text.into())])
            .quote_line(&Books::default().citation(r), &r.link("nwtsty", "F"))
    }

    fn r(book: u8, chapter: u16, first: u16, last: u16) -> BibleRef {
        BibleRef {
            book,
            chapter,
            first,
            last,
        }
    }

    #[test]
    fn finds_abbreviations_and_full_names() {
        assert_eq!(find_refs("Jn3:29"), vec![r(43, 3, 29, 29)]);
        assert_eq!(
            find_refs("Ré21:2 et Ré 22:17"),
            vec![r(66, 21, 2, 2), r(66, 22, 17, 17)]
        );
        assert_eq!(find_refs("Voir 1 Jean 4:8"), vec![r(62, 4, 8, 8)]);
        assert_eq!(find_refs("Jean 3 :16"), vec![r(43, 3, 16, 16)]);
        assert_eq!(find_refs("Mt 24:3-5"), vec![r(40, 24, 3, 5)]);
        assert_eq!(find_refs("Psaume 83:18"), vec![r(19, 83, 18, 18)]);
        assert_eq!(find_refs("Éz 18:4"), vec![r(26, 18, 4, 4)]);
        // Abbreviations from other French Bibles than the NWT.
        assert_eq!(find_refs("Rv21:3"), vec![r(66, 21, 3, 3)]);
        assert_eq!(find_refs("Ap 21:4"), vec![r(66, 21, 4, 4)]);
        assert_eq!(find_refs("Héb 11:1"), vec![r(58, 11, 1, 1)]);
    }

    #[test]
    fn verse_lists_stop_before_a_new_reference() {
        assert_eq!(
            find_refs("1 Jean 4:8, 16"),
            vec![r(62, 4, 8, 8), r(62, 4, 16, 16)]
        );
        assert_eq!(
            find_refs("Jn 3:16, 17 et Ps 83:18"),
            vec![r(43, 3, 16, 16), r(43, 3, 17, 17), r(19, 83, 18, 18)]
        );
        assert_eq!(
            find_refs("Le 19:18, 1 Jean 4:8"),
            vec![r(3, 19, 18, 18), r(62, 4, 8, 8)]
        );
    }

    #[test]
    fn ignores_times_and_lowercase_words() {
        assert!(find_refs("rdv le 8:15").is_empty());
        assert!(find_refs("1jn 4:8").is_empty());
        assert!(find_refs("Midi 12:30").is_empty());
        assert!(find_refs("vers 14:00").is_empty());
    }

    #[test]
    fn citation_and_link() {
        assert_eq!(
            Books::default().citation(&r(19, 83, 18, 18)),
            "Psaume 83:18"
        );
        assert_eq!(
            Books::default().citation(&r(40, 24, 3, 5)),
            "Matthieu 24:3-5"
        );
        assert_eq!(
            r(43, 3, 29, 29).link("nwtsty", "F"),
            "https://www.jw.org/finder?wtlocale=F&pub=nwtsty&bible=43003029"
        );
        assert_eq!(
            r(40, 24, 3, 5).link("nwt", "F"),
            "https://www.jw.org/finder?wtlocale=F&pub=nwt&bible=40024003-40024005"
        );
    }

    #[test]
    fn expand_regenerates_only_its_own_quotes() {
        let mut lookup = |r: &BibleRef| {
            Some(format!(
                "> v{} — [{}](https://www.jw.org/x)",
                r.first,
                Books::default().citation(r)
            ))
        };
        let body = "Jn 3:16\n> old — [Jean 3:29](https://www.jw.org/y)\n> perso\nsuite";
        let out = Books::default().expand_body(body, &mut lookup);
        assert_eq!(
            out,
            "Jn 3:16\n> v16 — [Jean 3:16](https://www.jw.org/x)\n> perso\nsuite"
        );
        // Idempotent.
        assert_eq!(Books::default().expand_body(&out, &mut lookup), out);
        // Deleting the reference drops its quote.
        let gone = out.replace("Jn 3:16", "plus rien");
        assert_eq!(
            Books::default().expand_body(&gone, &mut lookup),
            "plus rien\n> perso\nsuite"
        );
    }

    #[test]
    fn expand_document_skips_frontmatter() {
        let mut lookup = |_: &BibleRef| Some("> q — [X](https://www.jw.org/)".to_string());
        let doc = "---\ntitle: Jean 3:16\n---\nJn 3:16\n";
        assert_eq!(
            Books::default().expand_document(doc, &mut lookup),
            "---\ntitle: Jean 3:16\n---\nJn 3:16\n> q — [X](https://www.jw.org/)\n"
        );
    }

    #[test]
    fn unescapes_entities() {
        assert_eq!(
            unescape("a&nbsp;b &amp; c&#x2019;d &#8217; &bogus;"),
            "a\u{a0}b & c\u{2019}d \u{2019} &bogus;"
        );
    }

    #[test]
    fn cleans_epub_verse_markup() {
        let html = r##" <strong><sup>8</sup></strong> Je suivrai tes prescriptions.</p>
<p class="p2718 sz">Ne m’abandonne jamais<a epub:type="noteref" href="#f">*</a> complètement !</p>
<p id="p2719" class="p2719 ss">ב [<em>Beth</em>]</p>"##;
        assert_eq!(
            clean_verse(html),
            "Je suivrai tes prescriptions. Ne m’abandonne jamais complètement !"
        );
    }

    #[test]
    fn book_list_rejects_ambiguous_or_incomplete_files() {
        let twice = DEFAULT_BOOKS.replacen(
            r#"abreviations = ["Ex", "Exo"]"#,
            r#"abreviations = ["Ex", "Gn"]"#,
            1,
        );
        let err = Books::parse(&twice).unwrap_err().to_string();
        assert!(
            err.contains("'Gn' is used for both Genèse and Exode"),
            "{err}"
        );
        let short = DEFAULT_BOOKS.replacen(
            "[[livre]]\nnom = \"Jude\"",
            "[[ignored]]\nnom = \"Jude\"",
            1,
        );
        assert!(Books::parse(&short)
            .unwrap_err()
            .to_string()
            .contains("expected 66"));
    }

    #[test]
    fn book_list_edits_change_matching_and_citations() {
        let custom = DEFAULT_BOOKS.replacen(
            r#"nom = "Jean"
abreviations = ["Jn"]"#,
            r#"nom = "Jean"
citation = "Jn"
abreviations = ["Jn", "Joh"]"#,
            1,
        );
        let books = Books::parse(&custom).unwrap();
        assert_eq!(books.find_refs("Joh 3:16"), vec![r(43, 3, 16, 16)]);
        assert_eq!(books.citation(&r(43, 3, 16, 16)), "Jn 3:16");
    }

    #[test]
    fn reads_the_reference_back_from_a_quote_line() {
        for r in [r(43, 3, 16, 16), r(40, 24, 3, 5), r(1, 1, 1, 1)] {
            let line = quote(&r, "x");
            assert_eq!(BibleRef::from_quote(&line), Some(r), "{line}");
        }
        assert_eq!(BibleRef::from_quote("> perso"), None);
    }

    #[test]
    fn study_notes_go_under_their_quote_and_keep_its_origin() {
        let books = Books::default();
        let r316 = r(43, 3, 16, 16);
        let quote = quote(&r316, "Car Dieu…");
        let lines = vec![
            ("Jn 3:16".to_string(), 0),
            (quote.clone(), 0),
            ("suite".to_string(), 1),
        ];
        let out = books.annotate_study_notes(
            lines,
            &mut |r| {
                (*r == r316).then(|| StudyNotes {
                    lang: "F".into(),
                    checked: 0,
                    notes: vec![StudyNote {
                        verse: 16,
                        text: "**aimé :** agapaô".into(),
                    }],
                })
            },
            "F",
            &|r| Books::default().citation(r),
        );
        let text: Vec<&str> = out.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            text,
            [
                "Jn 3:16",
                quote.as_str(),
                "<details>",
                "<summary>Notes d’étude — Jean 3:16 (1)</summary>",
                "- **v. 16** **aimé :** agapaô",
                "</details>",
                "suite"
            ]
        );
        assert_eq!(
            out.iter().map(|(_, o)| *o).collect::<Vec<_>>(),
            [0, 0, 0, 0, 0, 0, 1]
        );
    }

    #[test]
    fn study_note_html_becomes_markdown() {
        let html = "<p class=\"s5\"><strong>juge\u{a0}:</strong> Ou «\u{a0}condamne\u{a0}» (<a class='b' href='/x'>Jean 3:16\u{a0};</a> <em>agapaô</em>).</p>\r\n";
        assert_eq!(
            html_to_markdown(html),
            "**juge\u{a0}:** Ou «\u{a0}condamne\u{a0}» (Jean 3:16\u{a0}; *agapaô*)."
        );
    }

    #[test]
    fn english_notes_are_labelled_and_anything_but_french_is_rechecked() {
        let books = Books::default();
        let r = r(52, 1, 3, 3);
        let quote = quote(&r, "x");
        let english = StudyNotes {
            lang: "E".into(),
            checked: 1_000,
            notes: vec![StudyNote {
                verse: 3,
                text: "**faithful work:** …".into(),
            }],
        };
        let out = books.annotate_study_notes(
            vec![(quote, 0)],
            &mut |_| Some(english.clone()),
            "F",
            &|r| Books::default().citation(r),
        );
        assert_eq!(
            out[2].0,
            "<summary>Notes d’étude (en anglais) — 1 Thessaloniciens 1:3 (1)</summary>"
        );

        let week = NOTES_RECHECK_SECS;
        assert!(!english.needs_recheck(1_000 + week - 1, "F"));
        assert!(english.needs_recheck(1_000 + week, "F"));
        let none = StudyNotes {
            lang: "F".into(),
            checked: 1_000,
            notes: vec![],
        };
        assert!(none.needs_recheck(1_000 + week, "F"));
        let french = StudyNotes {
            lang: "F".into(),
            ..english
        };
        assert!(
            !french.needs_recheck(u64::MAX, "F"),
            "French notes are final"
        );
    }

    #[test]
    fn language_codes_and_localized_links() {
        assert_eq!(jw_lang("fr"), "F");
        assert_eq!(jw_lang("EN"), "E");
        assert_eq!(jw_lang(" es "), "S");
        assert_eq!(
            jw_lang("tpo"),
            "TPO",
            "unknown codes pass through as jw.org codes"
        );
        assert_eq!(
            r(43, 3, 16, 16).link("nwtsty", "E"),
            "https://www.jw.org/finder?wtlocale=E&pub=nwtsty&bible=43003016"
        );
        // The link's locale doesn't change which reference it is.
        let line = Passage::new(vec![(16, "For God…".into())])
            .quote_line("John 3:16", &r(43, 3, 16, 16).link("nwtsty", "E"));
        assert_eq!(BibleRef::from_quote(&line), Some(r(43, 3, 16, 16)));
        assert!(is_generated(&line));
    }

    #[test]
    fn generated_language_list_parses_and_merges_under_the_users() {
        let mut rows: Vec<(String, String)> = Books::default()
            .citations
            .iter()
            .map(|n| (n.clone(), String::new()))
            .collect();
        rows[0] = ("Genesis".into(), "Ge".into());
        rows[42] = ("John".into(), "Joh".into());
        rows[2] = ("Leviticus".into(), "Le".into());
        rows[38] = ("Malachi".into(), "Mal".into());
        let english = Books::parse(&books_toml("E", &rows)).unwrap();
        assert_eq!(english.citation(&r(43, 3, 16, 16)), "John 3:16");
        let mut merged = Books::default();
        merged.merge(&english);
        assert_eq!(merged.find_refs("John 3:16"), vec![r(43, 3, 16, 16)]);
        assert_eq!(merged.find_refs("Joh 3:16"), vec![r(43, 3, 16, 16)]);
        // Still the user's own names in their language.
        assert_eq!(merged.citation(&r(43, 3, 16, 16)), "Jean 3:16");
        assert_eq!(merged.find_refs("Jn 3:16"), vec![r(43, 3, 16, 16)]);
    }

    #[test]
    fn accents_tell_spellings_apart_only_when_folding_would_clash() {
        let mut rows: Vec<(String, String)> = Books::default()
            .citations
            .iter()
            .map(|n| (n.clone(), String::new()))
            .collect();
        rows[10] = ("1. Könige".into(), "1Kö".into());
        rows[45] = ("1. Korinther".into(), "1Ko".into());
        let german = Books::parse(&books_toml("X", &rows)).unwrap();
        assert_eq!(german.find_refs("1Kö 8:27"), vec![r(11, 8, 27, 27)]);
        assert_eq!(german.find_refs("1Ko 13:4"), vec![r(46, 13, 4, 4)]);
        // French still ignores accents where nothing clashes.
        assert_eq!(find_refs("Re 21:3"), vec![r(66, 21, 3, 3)]);
        assert_eq!(find_refs("Rev 21:3"), vec![r(66, 21, 3, 3)]);
    }
}
