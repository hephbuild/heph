//! `<ref>`: which recording a session command reads (spec, Design "Opening a
//! recording: `<ref>`", D8), resolved once, in this order:
//!
//! 1. `https://…` is a URL, downloaded (capped) to a temp file and sniffed;
//!    `http://` is refused.
//! 2. Anything with a `/` or a `.` is a path. Ids are Crockford base32 and
//!    pids are digits, so neither ever has one: the rule needs no filesystem
//!    probe, and a file in the cwd named like an id never changes what is
//!    opened.
//! 3. The rest is an id, a live pid, or a unique prefix or suffix of an id.
//!
//! A path or URL opens a *foreign* recording (spec I7), read only from
//! itself, even when it came from this machine.

use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;

use super::{ErrorKind, ViewOpts, kinded, require_known, resolve};
use crate::tui::session::hephsession::ForeignError;
use crate::tui::session::recording::Recording;
use crate::tui::session::{SessionEntry, SessionSource};

/// What a `<ref>` names, before anything is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefKind<'a> {
    Url(&'a str),
    Path(&'a Path),
    Id(&'a str),
}

/// Classify `arg` (D8). Refuses `http://`: a recording is only downloaded
/// over https.
pub(crate) fn classify(arg: &str) -> anyhow::Result<RefKind<'_>> {
    let scheme = |s: &str| {
        arg.get(..s.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(s))
    };
    if scheme("https://") {
        return Ok(RefKind::Url(arg));
    }
    if scheme("http://") {
        return Err(kinded(
            ErrorKind::Fetch,
            anyhow::anyhow!(
                "refusing {}: a recording is only downloaded over https://",
                redact_url(arg)
            ),
        ));
    }
    if arg.contains(['/', '.']) {
        return Ok(RefKind::Path(Path::new(arg)));
    }
    Ok(RefKind::Id(arg))
}

/// `url` without the credentials it may carry, for messages: they never
/// reach error text.
pub(crate) fn redact_url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut u) => {
            if !u.username().is_empty() || u.password().is_some() {
                // Only fails for a URL that cannot have credentials.
                let _ok = u.set_username("");
                let _ok = u.set_password(None);
            }
            u.to_string()
        }
        Err(_) => "the URL".to_owned(),
    }
}

/// Downloads a `.hephsession` given by URL: `hfetch::Fetcher` in production,
/// a stub in tests.
pub(crate) trait Download: Send + Sync {
    /// GET `url` into a new temp file in `dir`.
    fn get_to_temp(&self, url: &str, dir: &Path) -> anyhow::Result<tempfile::NamedTempFile>;
}

impl Download for hfetch::Fetcher {
    fn get_to_temp(&self, url: &str, dir: &Path) -> anyhow::Result<tempfile::NamedTempFile> {
        hfetch::Fetcher::get_to_temp(self, url, dir)
    }
}

/// How a foreign `<ref>` is opened: where temp files go, and the download.
/// Owned, because the opening runs on a blocking thread.
pub(crate) struct Opener {
    /// Downloads and decompressed copies: the sessions root's `.tmp/`.
    pub tmp_dir: Arc<Path>,
    pub download: Arc<dyn Download>,
}

/// A resolved `<ref>`.
#[derive(Debug)]
pub(crate) enum Resolved {
    /// A session in this user's sessions directory, from `sources[index]`.
    /// Its format is one this heph reads.
    Local { index: usize, entry: SessionEntry },
    /// A recording opened by path or URL.
    Foreign(Recording),
}

/// Resolve `arg` against `sources` (an id) or open it (a path or URL).
pub(crate) async fn open_ref(
    sources: &[&dyn SessionSource],
    arg: &str,
    opts: ViewOpts,
    opener: &Opener,
) -> anyhow::Result<Resolved> {
    let arg = arg.trim();
    // A download, a 4 GiB decompression and the SQLite checks are blocking
    // work: on the blocking pool, never on a runtime worker.
    let tmp_dir = Arc::clone(&opener.tmp_dir);
    match classify(arg)? {
        RefKind::Id(id) => {
            let (index, entry) = resolve(sources, id, opts).await?;
            require_known(&entry)?;
            Ok(Resolved::Local { index, entry })
        }
        RefKind::Path(path) => {
            let path = path.to_owned();
            hcore::blocking::run(move || {
                ensure_tmp(&tmp_dir)?;
                Recording::open_foreign(&path, &tmp_dir)
                    .map(Resolved::Foreign)
                    .map_err(|e| foreign_error(e, &path.display().to_string()))
            })
            .await
        }
        RefKind::Url(url) => {
            let origin = redact_url(url);
            if url::Url::parse(url)
                .is_ok_and(|u| !u.username().is_empty() || u.password().is_some())
            {
                return Err(kinded(
                    ErrorKind::Fetch,
                    anyhow::anyhow!(
                        "refusing {origin}: credentials in a URL are not supported; download it \
                         (`gh run download`, say) and open the file"
                    ),
                ));
            }
            let (url, download) = (url.to_owned(), Arc::clone(&opener.download));
            hcore::blocking::run(move || {
                ensure_tmp(&tmp_dir)?;
                let file = download.get_to_temp(&url, &tmp_dir).map_err(|e| {
                    kinded(ErrorKind::Fetch, e.context(format!("downloading {origin}")))
                })?;
                Recording::open_download(file, origin.clone(), &tmp_dir)
                    .map(Resolved::Foreign)
                    .map_err(|e| foreign_error(e, &origin))
            })
            .await
        }
    }
}

fn ensure_tmp(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating {}", dir.display()))
        .map_err(|e| kinded(ErrorKind::Io, e))
}

/// A foreign recording that did not open, with its `error.kind`. A format
/// too new keeps exit 125 like every other session-command failure; its
/// kind tells it apart.
fn foreign_error(e: ForeignError, origin: &str) -> anyhow::Error {
    let kind = match &e {
        ForeignError::NotASession(_) => ErrorKind::NotASession,
        ForeignError::TooNew { .. } => ErrorKind::FormatTooNew,
        ForeignError::Io(_) => ErrorKind::Io,
    };
    kinded(kind, anyhow::anyhow!("{origin}: {e}"))
}
