//! `SHOW ursa.<setting>` and `SHOW ALL` with Ursa's settings.
//!
//! Ursa's settings are a DataFusion config extension with the prefix `ursa`
//! (`ursa_engine::options`): `SET ursa.index_storage_limit = '8GiB'` works,
//! but DataFusion 55's `SHOW` accepts only its own settings, and
//! `information_schema.df_settings` lists an extension's settings without the
//! prefix (`index_storage_limit`). So `SHOW` statements that name Ursa's
//! settings are answered here, from the same `df_settings` rows DataFusion's
//! `SHOW` reads, with the same columns: `name` and `value`, and
//! `description` after `VERBOSE`. A setting is named as `SET` names it,
//! `ursa.index_storage_limit`.
//!
//! - `SHOW ursa.<setting> [VERBOSE]`: one row, or an error naming the
//!   settings there are.
//! - `SHOW ALL [VERBOSE]`: DataFusion's rows with Ursa's named as `SET` names
//!   them, ordered by name.
//!
//! Every other `SHOW` is DataFusion's.

use datafusion::config::ExtensionOptions;
use datafusion::dataframe::DataFrame;
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::SessionContext;
use datafusion::sql::sqlparser::ast::{Ident, ObjectName};
use ursa_engine::options::UrsaOptions;

/// The prefix of Ursa's settings.
const PREFIX: &str = "ursa";

/// What a `SHOW <variable>` statement asks for, if it is about Ursa's
/// settings.
#[derive(Debug, PartialEq, Eq)]
enum Show {
    /// `SHOW ursa.<name>`: the name after the prefix.
    One { name: String, verbose: bool },
    /// `SHOW ALL`.
    All { verbose: bool },
}

impl Show {
    /// The request in `SHOW <variable>`, or `None` for one DataFusion
    /// answers itself.
    fn of(variable: &[Ident]) -> Option<Show> {
        let mut parts: Vec<String> = variable.iter().map(normalize).collect();
        let verbose = parts.len() > 1 && parts.last().is_some_and(|p| p == "verbose");
        if verbose {
            parts.pop();
        }
        match parts.as_slice() {
            [all] if all == "all" => Some(Show::All { verbose }),
            [prefix, rest @ ..] if prefix == PREFIX && !rest.is_empty() => Some(Show::One {
                name: rest.join("."),
                verbose,
            }),
            _ => None,
        }
    }
}

/// An identifier as DataFusion normalizes it: unquoted ones in lower case.
fn normalize(ident: &Ident) -> String {
    match ident.quote_style {
        Some(_) => ident.value.clone(),
        None => ident.value.to_ascii_lowercase(),
    }
}

/// Answer `SHOW <variable>` if it is about Ursa's settings (see the module
/// docs); `None` leaves it to DataFusion.
pub(crate) async fn show(ctx: &SessionContext, variable: &[Ident]) -> Option<Result<DataFrame>> {
    let request = Show::of(variable)?;
    Some(answer(ctx, request).await)
}

async fn answer(ctx: &SessionContext, request: Show) -> Result<DataFrame> {
    let names = setting_names(ctx);
    // `df_settings` names Ursa's settings without the prefix; a later
    // DataFusion may name them with it, so both are matched.
    let as_set = format!(
        "CASE WHEN name IN ({}) THEN '{PREFIX}.' || name ELSE name END",
        quoted(&names)
    );
    let query = match request {
        Show::One { name, verbose } => {
            if !names.contains(&name) {
                return Err(unknown_setting(&name, &names));
            }
            let full = format!("{PREFIX}.{name}");
            format!(
                "SELECT '{full}' AS name, value{} FROM information_schema.df_settings \
                 WHERE name IN ('{name}', '{full}')",
                if verbose { ", description" } else { "" }
            )
        }
        Show::All { verbose } => format!(
            "SELECT {as_set} AS name, value{} FROM information_schema.df_settings \
             ORDER BY name",
            if verbose { ", description" } else { "" }
        ),
    };
    ctx.sql(&query).await
}

/// Answer `RESET ursa.<setting>`, which DataFusion 55 refuses for an
/// extension's settings, by setting its default; `None` leaves any other
/// `RESET` to DataFusion.
pub(crate) fn reset(ctx: &SessionContext, variable: &ObjectName) -> Option<Result<DataFrame>> {
    let parts: Vec<String> = variable
        .0
        .iter()
        .filter_map(|p| p.as_ident().map(normalize))
        .collect();
    let [prefix, rest @ ..] = parts.as_slice() else {
        return None;
    };
    if prefix != PREFIX || rest.is_empty() {
        return None;
    }
    let name = rest.join(".");
    let names = setting_names(ctx);
    if !names.contains(&name) {
        return Some(Err(unknown_setting(&name, &names)));
    }
    let default = UrsaOptions::default()
        .entries()
        .into_iter()
        .find(|e| e.key == name)
        .and_then(|e| e.value)
        .unwrap_or_default();
    let set = ctx
        .state_ref()
        .write()
        .config_mut()
        .options_mut()
        .set(&format!("{PREFIX}.{name}"), &default);
    Some(set.and_then(|()| crate::graph_ddl::empty(ctx)))
}

/// The names of Ursa's settings, without the prefix, in the session's order.
fn setting_names(ctx: &SessionContext) -> Vec<String> {
    let state = ctx.state();
    let options = state.config_options();
    let entries = match options.extensions.get::<UrsaOptions>() {
        Some(o) => o.entries(),
        None => UrsaOptions::default().entries(),
    };
    entries.into_iter().map(|e| e.key).collect()
}

/// `'a', 'b'`, for an `IN` list (names are identifiers, without quotes).
fn quoted(names: &[String]) -> String {
    names
        .iter()
        .map(|n| format!("'{}'", n.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ")
}

/// "'ursa.index_storage_limt' is not a setting; did you mean ...?".
fn unknown_setting(name: &str, names: &[String]) -> DataFusionError {
    let full: Vec<String> = names.iter().map(|n| format!("{PREFIX}.{n}")).collect();
    let hint = match ursa_engine::graph::closest_name(name, names.iter().map(String::as_str)) {
        Some(close) => format!("; did you mean '{PREFIX}.{close}'?"),
        None => format!(". Ursa's settings are: {}", full.join(", ")),
    };
    DataFusionError::Plan(format!("'{PREFIX}.{name}' is not an Ursa setting{hint}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idents(words: &[&str]) -> Vec<Ident> {
        words.iter().map(|w| Ident::new(*w)).collect()
    }

    #[test]
    fn which_show_statements_are_ursas() {
        assert_eq!(
            Show::of(&idents(&["ursa", "index_storage_limit"])),
            Some(Show::One {
                name: "index_storage_limit".into(),
                verbose: false
            })
        );
        assert_eq!(
            Show::of(&idents(&["URSA", "Index_Storage_Limit", "VERBOSE"])),
            Some(Show::One {
                name: "index_storage_limit".into(),
                verbose: true
            })
        );
        assert_eq!(
            Show::of(&idents(&["all"])),
            Some(Show::All { verbose: false })
        );
        assert_eq!(
            Show::of(&idents(&["ALL", "verbose"])),
            Some(Show::All { verbose: true })
        );
        for other in [
            &["datafusion", "execution", "batch_size"][..],
            &["timezone"],
            &["ursa"],
            &["verbose"],
        ] {
            assert_eq!(Show::of(&idents(other)), None, "{other:?}");
        }
    }
}
