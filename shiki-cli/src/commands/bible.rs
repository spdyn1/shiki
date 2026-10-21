//! `shiki bible` — looks up Bible references the way `[bible]` does in the
//! TUI, printing the quote lines it would insert. Mostly for checking a
//! reference or the local EPUB from a terminal; `--download` fetches (or
//! refreshes) that EPUB explicitly.

use anyhow::{bail, Result};
use shiki_config::Config;
use shiki_core::bible::{self, Bible, Books, Source};

pub fn run(
    config: &Config,
    text: &str,
    online: bool,
    download: bool,
    language: Option<&str>,
) -> Result<()> {
    let dir = Config::default_bible_dir()?;
    let lang = bible::jw_lang(language.unwrap_or(&config.bible.language));
    let epub = bible::epub_path(&dir, &config.bible.epub_path, &lang);
    if download {
        bible::download_epub(&epub, &lang)?;
        println!("downloaded {}", epub.display());
        if text.is_empty() {
            return Ok(());
        }
    }
    let books_path = Config::default_bible_books_path()?;
    let books = Books::load(&books_path)?;
    let source = if online || config.bible.online_only() {
        Source::Online
    } else {
        Source::Local
    };
    let bible = Bible::new(
        source,
        config.bible.edition.clone(),
        lang,
        books,
        &books_path,
        epub,
        &dir,
    );
    // This language's own spellings count too (its list is built from the
    // EPUB, downloaded here if needed).
    bible.prepare();
    let books = bible
        .books_with_own()
        .unwrap_or_else(|| bible.books.clone());
    let refs = books.find_refs(text);
    if refs.is_empty() {
        bail!("no Bible reference found in '{text}' (e.g. \"Jn 3:16\", \"1 Jean 4:8, 16\")");
    }
    for r in refs {
        match bible.resolve(&r) {
            Some(passage) => println!("{}", bible.quote_line(&r, &passage)),
            None => eprintln!("{}: not found", bible.books.citation(&r)),
        }
    }
    Ok(())
}
