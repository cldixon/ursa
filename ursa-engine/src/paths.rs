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

use datafusion::datasource::listing::ListingTableUrl;
use datafusion::error::{DataFusionError, Result};
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
/// expands them), or a URL whose path may contain a glob.
pub fn table_url(location: &str) -> Result<ListingTableUrl> {
    let location = expand_tilde(location);
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
}
