//! `sources.list` and `sources.attach` (`design/console.md` §4.3, §8): the
//! files a session can read, for the console's sources panel.
//!
//! A local directory is listed only under the session's directory root:
//! the path asked for, with its symbolic links resolved, must be inside it,
//! and so must each entry's (one whose link leads out is left out, as are
//! hidden files). A bucket prefix (`s3://`, `gs://`, `az://`) is listed with
//! `object_store`'s `list_with_delimiter`, through the session's object
//! store registry, which builds a store from the CLI's credential chain the
//! first time a bucket is named. A reply names objects by `scheme://bucket/
//! key` only: never a credential, a query string or a signed URL.

#[cfg(not(target_arch = "wasm32"))]
use std::path::Path;

use datafusion::execution::object_store::ObjectStoreUrl;
use datafusion::prelude::SessionContext;
use object_store::path::Path as ObjectPath;
use serde_json::{json, Value};
use url::Url;

use super::{Error, Root};

/// The bucket schemes `sources.list` lists.
const BUCKETS: &[&str] = &["s3", "s3a", "gs", "az", "abfs", "abfss"];

/// What a file holds, by its extension: `parquet`, `csv`, `json`, `arrow`
/// or `ursa`; `None` for anything else. A compression suffix (`.gz`, …) is
/// looked past.
pub(crate) fn format_of(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    let mut stem = lower.as_str();
    for suffix in [".gz", ".bz2", ".xz", ".zst"] {
        if let Some(s) = stem.strip_suffix(suffix) {
            stem = s;
            break;
        }
    }
    let ext = stem.rsplit_once('.').map(|(_, e)| e)?;
    Some(match ext {
        "parquet" => "parquet",
        "csv" | "tsv" => "csv",
        "json" | "jsonl" | "ndjson" => "json",
        "arrow" | "ipc" | "feather" => "arrow",
        "ursa" => "ursa",
        _ => return None,
    })
}

fn refused(message: impl Into<String>) -> Error {
    Error::protocol(message)
}

/// `sources.list {url}` in a session rooted at `root`: `{entries: [{name,
/// url, kind, format, bytes}]}`, directories first, then files, each by name.
pub(crate) async fn list(ctx: &SessionContext, root: &Root, url: &str) -> Result<Value, Error> {
    let parsed = Url::parse(url).ok().filter(|u| u.scheme().len() > 1);
    let entries = match parsed {
        Some(u) if BUCKETS.contains(&u.scheme()) => list_bucket(ctx, &u).await?,
        Some(u) if u.scheme() == "file" => list_local(root, &file_path(&u)?)?,
        Some(u) => {
            return Err(refused(format!(
                "cannot list {}: sources.list lists a directory under the session's root, \
                 or a bucket prefix (s3://, gs://, az://)",
                clean(&u)
            )))
        }
        None => list_local(root, url)?,
    };
    Ok(json!({ "entries": entries }))
}

#[cfg(not(target_arch = "wasm32"))]
fn file_path(url: &Url) -> Result<String, Error> {
    url.to_file_path()
        .ok()
        .and_then(|p| p.into_os_string().into_string().ok())
        .ok_or_else(|| refused(format!("{} is not a local path", clean(url))))
}

#[cfg(target_arch = "wasm32")]
fn file_path(url: &Url) -> Result<String, Error> {
    Err(refused(format!("{} is not a local path", clean(url))))
}

/// `url` as a reply may show it: scheme, host and path only.
fn clean(url: &Url) -> String {
    format!(
        "{}://{}{}",
        url.scheme(),
        url.host_str().unwrap_or(""),
        url.path()
    )
}

/// The entries of the local directory `path` (relative to the root, or
/// absolute), which must be under `root`.
#[cfg(not(target_arch = "wasm32"))]
fn list_local(root: &Root, path: &str) -> Result<Vec<Value>, Error> {
    let Root::Dir(dir) = root else {
        return Err(refused("this session has no directory to list"));
    };
    let root = std::fs::canonicalize(dir).map_err(|e| {
        Error::not_found(format!(
            "the session's root, {}, cannot be read: {e}",
            dir.display()
        ))
    })?;
    let expanded = ursa_engine::paths::expand_tilde(path);
    let asked = Path::new(&expanded);
    let joined = if asked.is_absolute() {
        asked.to_path_buf()
    } else {
        dir.join(asked)
    };
    let outside = || {
        refused(format!(
            "'{path}' is outside this session's root, {}: sources.list lists only under it",
            root.display()
        ))
    };
    let real = match std::fs::canonicalize(&joined) {
        Ok(real) => real,
        // Missing: say so only of a path under the root, so a listing
        // cannot probe what is outside it.
        Err(e) if lexically_under(&joined, dir) => {
            return Err(Error::not_found(format!("cannot list '{path}': {e}")))
        }
        Err(_) => return Err(outside()),
    };
    if !real.starts_with(&root) {
        return Err(outside());
    }
    if !real.is_dir() {
        return Err(refused(format!("'{path}' is not a directory")));
    }
    let read = std::fs::read_dir(&real)
        .map_err(|e| Error::not_found(format!("cannot list '{path}': {e}")))?;
    let mut entries = Vec::new();
    for entry in read.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let at = real.join(&name);
        // Through its links: left out if they lead outside, or nowhere.
        let Ok(target) = std::fs::canonicalize(&at) else {
            continue;
        };
        if !target.starts_with(&root) {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&target) else {
            continue;
        };
        let Ok(url) = Url::from_file_path(&at) else {
            continue;
        };
        let dir = meta.is_dir();
        let mut url = String::from(url);
        if dir {
            url.push('/');
        }
        entries.push(entry_json(&name, url, dir, (!dir).then_some(meta.len())));
    }
    sort(&mut entries);
    Ok(entries)
}

#[cfg(target_arch = "wasm32")]
fn list_local(_: &Root, _: &str) -> Result<Vec<Value>, Error> {
    Err(refused("this session has no directory to list"))
}

/// Whether `path`, with no link followed, is `dir` or under it.
#[cfg(not(target_arch = "wasm32"))]
fn lexically_under(path: &Path, dir: &Path) -> bool {
    use std::path::Component;
    let mut depth = 0usize;
    let Ok(rest) = path.strip_prefix(dir) else {
        return false;
    };
    for c in rest.components() {
        match c {
            Component::ParentDir if depth == 0 => return false,
            Component::ParentDir => depth -= 1,
            Component::Normal(_) => depth += 1,
            _ => {}
        }
    }
    true
}

fn entry_json(name: &str, url: String, dir: bool, bytes: Option<u64>) -> Value {
    json!({
        "name": name,
        "url": url,
        "kind": if dir { "dir" } else { "file" },
        "format": format_of(name),
        "bytes": bytes,
    })
}

fn sort(entries: &mut [Value]) {
    entries.sort_by(|a, b| {
        let key = |v: &Value| {
            (
                v["kind"] != "dir",
                v["name"].as_str().unwrap_or("").to_string(),
            )
        };
        key(a).cmp(&key(b))
    });
}

/// The objects and prefixes right under the bucket prefix `url`.
async fn list_bucket(ctx: &SessionContext, url: &Url) -> Result<Vec<Value>, Error> {
    let base = format!("{}://{}", url.scheme(), url.host_str().unwrap_or(""));
    let shown = clean(url);
    let failed = |e: String| Error {
        kind: "engine",
        message: format!("cannot list {shown}: {e}"),
    };
    let store_url = Url::parse(&format!("{base}/")).map_err(|e| failed(e.to_string()))?;
    let runtime = ctx.runtime_env();
    ursa_engine::stores::ensure_object_store(&runtime, &store_url)
        .await
        .map_err(|e| failed(ursa_engine::user_message(&e)))?;
    let store = runtime
        .object_store(
            &ObjectStoreUrl::parse(store_url.as_str())
                .map_err(|e| failed(ursa_engine::user_message(&e)))?,
        )
        .map_err(|e| failed(ursa_engine::user_message(&e)))?;
    let prefix = ObjectPath::from_url_path(url.path()).map_err(|e| failed(e.to_string()))?;
    let prefix = (!prefix.as_ref().is_empty()).then_some(prefix);
    let listed = store
        .list_with_delimiter(prefix.as_ref())
        .await
        .map_err(|e| failed(e.to_string()))?;
    let mut entries = Vec::new();
    for dir in &listed.common_prefixes {
        let Some(name) = dir.filename() else { continue };
        if name.starts_with('.') {
            continue;
        }
        entries.push(entry_json(name, format!("{base}/{dir}/"), true, None));
    }
    for object in &listed.objects {
        let Some(name) = object.location.filename() else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let url = format!("{base}/{}", object.location);
        entries.push(entry_json(name, url, false, Some(object.size)));
    }
    sort(&mut entries);
    Ok(entries)
}

/// `sources.attach {url, as?}`'s statement: `ATTACH 'path' [AS name]`, a
/// `file://` URL given as its path.
pub(crate) fn attach_sql(url: &str, alias: Option<&str>) -> Result<String, Error> {
    let path = match Url::parse(url) {
        Ok(u) if u.scheme() == "file" => file_path(&u)?,
        _ => url.to_string(),
    };
    let mut sql = format!("ATTACH '{}'", path.replace('\'', "''"));
    if let Some(alias) = alias {
        sql += &format!(" AS \"{}\"", alias.replace('"', "\"\""));
    }
    Ok(sql)
}
