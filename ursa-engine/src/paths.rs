//! Turning a name in `FROM` into a file location.
//!
//! `SELECT * FROM 'edges.parquet'` names a file where a table would go. A name
//! that reads as a location (it has a scheme, a path separator, an extension or
//! a glob) is opened as a listing table; anything else is left to the catalog,
//! so a misspelt table name still reports "table not found".
//!
//! DataFusion's [`ListingTableUrl::parse`] expands a glob only in a scheme-less
//! local path. [`table_url`] also expands one in a URL (`s3://bucket/dir/*.parquet`,
//! `file:///data/*.csv`): the URL is split at the path segment where the glob
//! begins, the part before it becomes the listing prefix, and the rest filters
//! the listing.
//!
//! A relative path resolves against the session's [`Root`]: a directory
//! natively, a URL in the browser, or, natively with none, the working
//! directory. A session with a directory root also writes only under it
//! ([`write_target`], [`check_write`]).

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use datafusion::datasource::listing::ListingTableUrl;
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::{SessionConfig, SessionContext};
use glob::Pattern;
use url::Url;

/// The characters that start a glob, as DataFusion's local-path parser has them.
const GLOB_START_CHARS: [char; 3] = ['*', '?', '['];

/// Whether a table name reads as a file location rather than a table name.
pub fn looks_like_location(name: &str) -> bool {
    name.contains(['/', '\\', '.']) || name.contains(GLOB_START_CHARS) || name.starts_with('~')
}

/// `~` or `~/…` with the home directory put in for the `~`.
pub fn expand_tilde(location: &str) -> String {
    if location == "~" || location.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME").and_then(|h| h.into_string().ok()) {
            if !home.is_empty() {
                return format!("{home}{}", &location[1..]);
            }
        }
    }
    location.to_string()
}

/// The listing table URL for a location: a local path (with globs, as DataFusion
/// expands them), or a URL whose path may contain a glob. A relative path is
/// relative to the process's working directory; [`session_table_url`]
/// resolves it against a session's [`Root`].
pub fn table_url(location: &str) -> Result<ListingTableUrl> {
    table_url_under(None, location)
}

/// [`table_url`] in the session with `config`: a relative path resolves
/// against the session's [`Root`], if it has one.
pub fn session_table_url(config: &SessionConfig, location: &str) -> Result<ListingTableUrl> {
    table_url_under(root(config).as_ref(), location)
}

fn table_url_under(root: Option<&Root>, location: &str) -> Result<ListingTableUrl> {
    let location = resolve_under(root, location)?;
    if let Some((base, pattern)) = split_url_glob(&location) {
        let url = Url::parse(base)
            .map_err(|e| DataFusionError::Plan(format!("'{location}' is not a valid URL: {e}")))?;
        let glob = Pattern::new(pattern).map_err(|e| {
            DataFusionError::Plan(format!(
                "'{pattern}' in '{location}' is not a valid glob: {e}"
            ))
        })?;
        return ListingTableUrl::try_new(url, Some(glob));
    }
    ListingTableUrl::parse(&location)
        .map_err(|e| DataFusionError::Plan(format!("'{location}' is not a valid path or URL: {e}")))
}

/// For a URL with a scheme, split it at the start of the path segment that holds
/// the first glob character: `s3://b/data/*.parquet` gives `("s3://b/data/",
/// "*.parquet")`. `None` for a local path (DataFusion expands its globs itself),
/// a URL without a glob, and `http(s)` URLs, where `?` starts a query and a
/// server cannot be listed anyway.
pub fn split_url_glob(location: &str) -> Option<(&str, &str)> {
    let scheme_end = location.find("://")?;
    let scheme = &location[..scheme_end];
    // A one-letter "scheme" is a Windows drive letter.
    if scheme.len() < 2
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        || matches!(scheme, "http" | "https")
    {
        return None;
    }
    let authority_start = scheme_end + 3;
    let path_start = authority_start + location[authority_start..].find('/')?;
    let path = &location[path_start..];
    let glob_at = path.find(GLOB_START_CHARS)?;
    // `path` starts with '/', so there is always a separator before the glob.
    let split = path_start + path[..glob_at].rfind('/')? + 1;
    Some((&location[..split], &location[split..]))
}

/// Where a session's relative paths resolve: its root.
///
/// The root travels with the session, as an extension of its
/// [`SessionConfig`] ([`set_root`], [`with_root`]), so two sessions in one
/// process can each have their own.
///
/// - **No root:** natively, a relative path is relative to the process's
///   working directory, as the CLI has always read it. The browser has no
///   working directory, so there a relative path is an error.
/// - **[`Root::Dir`]:** a directory, natively (`ursa serve DIR`). `'edges.csv'`
///   is `DIR/edges.csv`. Reads are not confined: an absolute path, `~/…` or a
///   URL reads what it names, as anywhere else in the native engine. Writes
///   are: `COPY … TO` and `.output` write only under the root
///   ([`write_target`]), and `INSERT INTO` a table of files only into files
///   under it ([`check_write`]).
/// - **[`Root::Url`]:** a URL, such as the browser shell's `https://…/data/`.
///   A relative path, or an absolute one (`/data/x.csv`), is joined to it as
///   a browser joins a link. It confines no write: a browser writes only the
///   session's own files (`crate::session_files`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Root {
    /// A directory. A relative one is made absolute (against the working
    /// directory) when it is set.
    Dir(PathBuf),
    /// A URL. One that names a directory ends in `/`, as in a browser.
    Url(Url),
}

/// The session's root, as its config holds it.
#[derive(Debug)]
struct SessionRoot(Option<Root>);

/// `config` with `root` as its session's root (`None`: none).
pub fn with_root(config: SessionConfig, root: Option<Root>) -> SessionConfig {
    config.with_extension(Arc::new(SessionRoot(root.map(absolute))))
}

/// Set (or clear) the root of the session `ctx`. It applies from the next
/// statement on.
pub fn set_root(ctx: &SessionContext, root: Option<Root>) {
    let state = ctx.state_ref();
    let mut state = state.write();
    state
        .config_mut()
        .set_extension(Arc::new(SessionRoot(root.map(absolute))));
}

/// The root of the session with `config`, if it has one.
pub fn root(config: &SessionConfig) -> Option<Root> {
    config
        .get_extension::<SessionRoot>()
        .and_then(|root| root.0.clone())
}

/// The directory a native session's relative local paths are relative to:
/// its root, if that is a directory, else the working directory. `None` if
/// the working directory cannot be read (or the root is a URL).
pub fn base_dir(config: &SessionConfig) -> Option<PathBuf> {
    match root(config) {
        Some(Root::Dir(dir)) => Some(dir),
        Some(Root::Url(_)) => None,
        None => std::env::current_dir().ok(),
    }
}

fn absolute(root: Root) -> Root {
    match root {
        Root::Dir(dir) if dir.is_relative() => Root::Dir(std::path::absolute(&dir).unwrap_or(dir)),
        root => root,
    }
}

/// Whether `location` is a URL: it has a scheme of two letters or more (a
/// one-letter one is a Windows drive).
fn has_scheme(location: &str) -> bool {
    matches!(Url::parse(location), Ok(url) if url.scheme().len() > 1)
}

/// `location` (a path, glob or URL, as `FROM` names it) resolved in the
/// session with `config`: `~` expanded, then a relative path joined to the
/// session's [`Root`]. With no root, a relative path is left as it is (the
/// working directory's), except in the browser, which has none.
pub fn resolve(config: &SessionConfig, location: &str) -> Result<String> {
    resolve_under(root(config).as_ref(), location)
}

fn resolve_under(root: Option<&Root>, location: &str) -> Result<String> {
    let location = expand_tilde(location);
    if has_scheme(&location) {
        return Ok(location);
    }
    match root {
        Some(Root::Url(base)) => base.join(&location).map(String::from).map_err(|e| {
            DataFusionError::Plan(format!("'{location}' is not a valid path or URL: {e}"))
        }),
        Some(Root::Dir(_)) if Path::new(&location).is_absolute() => Ok(location),
        Some(Root::Dir(dir)) => dir
            .join(&location)
            .into_os_string()
            .into_string()
            .map_err(|_| {
                DataFusionError::Plan(format!(
                    "'{location}' cannot be read: the session's root, {}, is not valid UTF-8",
                    dir.display()
                ))
            }),
        None if cfg!(target_arch = "wasm32") => Err(DataFusionError::Plan(format!(
            "'{location}' is a relative path, and the browser has no current directory; \
             use an http:// or https:// URL"
        ))),
        None => Ok(location),
    }
}

/// Where a write to `target` (`COPY … TO 'target'`, `.output target`) goes in
/// the session with `config`: `Ok(None)` to write it as named, in a session
/// with no directory [`Root`]; in a session with one, the absolute path under
/// the root that `target` names, or why it may not be written (a reason
/// that does not repeat `target`, for the caller to say what was refused).
///
/// A rooted session writes only under its root. `target` must be a relative
/// path with no `..` (not absolute, not `~/…`, not a URL), and no part of it
/// that exists may be a symbolic link that leads outside the root. The check
/// is made before the write, not during it: it keeps a client of the session
/// (a page, an agent) inside the root, not another process that swaps a
/// directory for a link in between.
pub fn write_target(config: &SessionConfig, target: &str) -> Result<Option<String>> {
    match root(config) {
        Some(Root::Dir(dir)) => under_root(&dir, target).map(Some),
        _ => Ok(None),
    }
}

fn under_root(dir: &Path, target: &str) -> Result<String> {
    let outside = |why: &str| {
        DataFusionError::Plan(format!(
            "it is outside this session's root, {} ({why}); a session with a root writes \
             only under it, to a relative path such as 'out/result.parquet'",
            dir.display()
        ))
    };
    if has_scheme(target) {
        return Err(outside("it is a URL"));
    }
    if target.starts_with('~') {
        return Err(outside("it is in the home directory"));
    }
    let mut parts = Vec::new();
    for component in Path::new(target).components() {
        match component {
            Component::Normal(part) => parts.push(part),
            Component::CurDir => {}
            Component::ParentDir => return Err(outside("it has '..' in it")),
            Component::RootDir | Component::Prefix(_) => {
                return Err(outside("it is an absolute path"))
            }
        }
    }
    if parts.is_empty() {
        return Err(outside("it names the root itself"));
    }
    let root = std::fs::canonicalize(dir).map_err(|e| {
        DataFusionError::Plan(format!(
            "the session's root, {}, cannot be read: {e}",
            dir.display()
        ))
    })?;
    let mut path = root.clone();
    path.extend(&parts);
    match resolve_links(&path) {
        Ok(real) if real.starts_with(&root) => {}
        Ok(_) => return Err(outside("a symbolic link in it leads outside the root")),
        Err(e) => {
            return Err(DataFusionError::Plan(format!(
                "a symbolic link in it cannot be followed: {e}"
            )))
        }
    }
    let mut written = path.into_os_string().into_string().map_err(|_| {
        DataFusionError::Plan(format!(
            "the session's root, {}, is not valid UTF-8",
            dir.display()
        ))
    })?;
    // `COPY … TO 'out/'` writes a directory of files.
    if target.ends_with(['/', std::path::MAIN_SEPARATOR]) {
        written.push(std::path::MAIN_SEPARATOR);
    }
    Ok(written)
}

/// Refuse a write to the files at `url` (an `INSERT INTO` a table of files)
/// in a session with a directory [`Root`], unless they are under it: a local
/// path whose links stay inside the root. Any write is allowed in a session
/// without one.
pub fn check_write(config: &SessionConfig, url: &Url) -> Result<()> {
    let Some(Root::Dir(dir)) = root(config) else {
        return Ok(());
    };
    let refused = || {
        DataFusionError::Plan(format!(
            "cannot write to {url}: it is outside this session's root, {}, and a session \
             with a root writes only under it",
            dir.display()
        ))
    };
    let path = local_path(url).ok_or_else(refused)?;
    let root = std::fs::canonicalize(&dir).map_err(|_| refused())?;
    match resolve_links(&path) {
        Ok(real) if real.starts_with(&root) => Ok(()),
        _ => Err(refused()),
    }
}

/// The local path a `file://` URL names.
#[cfg(not(target_arch = "wasm32"))]
fn local_path(url: &Url) -> Option<PathBuf> {
    (url.scheme() == "file").then(|| url.to_file_path().ok())?
}

/// The browser has no local paths.
#[cfg(target_arch = "wasm32")]
fn local_path(_: &Url) -> Option<PathBuf> {
    None
}

/// `path` (absolute, without `..`) with its symbolic links resolved as far as
/// it exists, and the rest, which does not exist yet, as it is. An error for
/// a link that cannot be followed (one that leads nowhere, or a loop): a
/// write would create its target, wherever it is.
fn resolve_links(path: &Path) -> std::io::Result<PathBuf> {
    match std::fs::canonicalize(path) {
        Ok(real) => Ok(real),
        Err(e) => {
            // It is there (a link), but cannot be followed.
            if std::fs::symlink_metadata(path).is_ok() {
                return Err(e);
            }
            match (path.parent(), path.file_name()) {
                (Some(parent), Some(name)) => Ok(resolve_links(parent)?.join(name)),
                _ => Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations_and_table_names() {
        assert!(looks_like_location("edges.csv"));
        assert!(looks_like_location("data/edges"));
        assert!(looks_like_location("s3://bucket/x"));
        assert!(looks_like_location("*.parquet"));
        assert!(looks_like_location("~/edges"));
        assert!(!looks_like_location("edges"));
        assert!(!looks_like_location("my_table"));
    }

    #[test]
    fn splits_url_globs() {
        assert_eq!(
            split_url_glob("s3://bucket/data/*.parquet"),
            Some(("s3://bucket/data/", "*.parquet"))
        );
        assert_eq!(
            split_url_glob("gs://bucket/a/b/part-?.csv"),
            Some(("gs://bucket/a/b/", "part-?.csv"))
        );
        assert_eq!(
            split_url_glob("s3://bucket/year=*/x.parquet"),
            Some(("s3://bucket/", "year=*/x.parquet"))
        );
        assert_eq!(
            split_url_glob("file:///tmp/x/[ab].csv"),
            Some(("file:///tmp/x/", "[ab].csv"))
        );
        assert_eq!(split_url_glob("s3://bucket/data/x.parquet"), None);
        assert_eq!(split_url_glob("https://host/x.csv?y=1"), None);
        assert_eq!(split_url_glob("data/*.csv"), None);
        assert_eq!(split_url_glob("C://data/*.csv"), None);
    }

    #[test]
    fn remote_glob_url_lists_its_prefix() {
        let url = table_url("s3://bucket/data/*.parquet").unwrap();
        assert_eq!(url.scheme(), "s3");
        assert_eq!(url.prefix().as_ref(), "data");
        assert!(url.is_collection());
        assert_eq!(url.get_glob().as_ref().unwrap().as_str(), "*.parquet");
    }

    fn rooted(dir: &Path) -> SessionConfig {
        with_root(SessionConfig::new(), Some(Root::Dir(dir.to_path_buf())))
    }

    #[cfg(unix)]
    #[test]
    fn a_relative_path_resolves_against_the_root() {
        let config = rooted(Path::new("/srv/data"));
        assert_eq!(
            resolve(&config, "edges.csv").unwrap(),
            "/srv/data/edges.csv"
        );
        assert_eq!(
            resolve(&config, "./sub/*.csv").unwrap(),
            "/srv/data/./sub/*.csv"
        );
        assert_eq!(
            resolve(&config, "/abs/edges.csv").unwrap(),
            "/abs/edges.csv"
        );
        assert_eq!(
            resolve(&config, "s3://bucket/x.parquet").unwrap(),
            "s3://bucket/x.parquet"
        );
        let url = with_root(
            SessionConfig::new(),
            Some(Root::Url(Url::parse("https://example.com/data/").unwrap())),
        );
        assert_eq!(
            resolve(&url, "karate_edges.csv").unwrap(),
            "https://example.com/data/karate_edges.csv"
        );
        assert_eq!(
            resolve(&url, "/other/x.csv").unwrap(),
            "https://example.com/other/x.csv"
        );
        // No root: as written, the working directory's (natively).
        assert_eq!(
            resolve(&SessionConfig::new(), "edges.csv").unwrap(),
            "edges.csv"
        );
        let table = session_table_url(&config, "edges.csv").unwrap();
        assert_eq!(table.get_url().as_str(), "file:///srv/data/edges.csv");
    }

    #[test]
    fn a_relative_root_is_made_absolute() {
        let config = with_root(SessionConfig::new(), Some(Root::Dir("data".into())));
        let Some(Root::Dir(dir)) = root(&config) else {
            panic!("a directory root")
        };
        assert_eq!(dir, std::env::current_dir().unwrap().join("data"));
    }

    #[test]
    fn writes_stay_under_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root_dir = std::fs::canonicalize(dir.path()).unwrap();
        let config = rooted(dir.path());
        let target = |t: &str| write_target(&config, t);
        assert_eq!(
            target("out.parquet").unwrap().unwrap(),
            root_dir.join("out.parquet").to_str().unwrap()
        );
        assert_eq!(
            target("./new/dir/out.csv").unwrap().unwrap(),
            root_dir.join("new/dir/out.csv").to_str().unwrap()
        );
        assert!(target("parts/").unwrap().unwrap().ends_with("parts/"));
        for (refused, why) in [
            ("../out.csv", "'..'"),
            ("a/../../out.csv", "'..'"),
            ("/tmp/out.csv", "absolute"),
            ("~/out.csv", "home directory"),
            ("file:///tmp/out.csv", "URL"),
            ("s3://bucket/out.csv", "URL"),
            ("memory://uploads/out.csv", "URL"),
            (".", "the root itself"),
        ] {
            let e = target(refused).unwrap_err().to_string();
            assert!(
                e.contains("outside this session's root") && e.contains(why),
                "{refused}: {e}"
            );
        }
        // No root: as written.
        assert_eq!(
            write_target(&SessionConfig::new(), "/tmp/x.csv").unwrap(),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_out_of_the_root_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root_dir = dir.path().join("root");
        std::fs::create_dir(&root_dir).unwrap();
        std::fs::create_dir(root_dir.join("inside")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root_dir.join("away")).unwrap();
        std::os::unix::fs::symlink(root_dir.join("inside"), root_dir.join("here")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("x.csv"), root_dir.join("dangling.csv"))
            .unwrap();
        std::os::unix::fs::symlink(outside.path().join("y.csv"), root_dir.join("inside/y.csv"))
            .unwrap();
        let config = rooted(&root_dir);
        for refused in ["away/x.csv", "away/new/x.csv", "here/y.csv"] {
            let e = write_target(&config, refused).unwrap_err().to_string();
            assert!(e.contains("symbolic link"), "{refused}: {e}");
        }
        // A link that leads nowhere would be created wherever it points.
        let e = write_target(&config, "dangling.csv")
            .unwrap_err()
            .to_string();
        assert!(e.contains("cannot be followed"), "{e}");
        // A link that stays inside the root is fine.
        assert!(write_target(&config, "here/z.csv").unwrap().is_some());
        // A root that is itself a link is followed.
        let linked = dir.path().join("linked");
        std::os::unix::fs::symlink(&root_dir, &linked).unwrap();
        assert!(write_target(&rooted(&linked), "inside/z.csv")
            .unwrap()
            .is_some());

        let inside = Url::from_file_path(root_dir.join("inside/t.csv")).unwrap();
        check_write(&config, &inside).unwrap();
        let away = Url::from_file_path(root_dir.join("away/t.csv")).unwrap();
        assert!(check_write(&config, &away).is_err());
        let s3 = Url::parse("s3://bucket/t.csv").unwrap();
        assert!(check_write(&config, &s3).is_err());
        check_write(&SessionConfig::new(), &s3).unwrap();
    }
}
