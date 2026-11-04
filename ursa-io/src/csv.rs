//! CSV options, type inference and value errors (#216, #221, #222).
//!
//! A CSV file is read through DataFusion's CSV reader with [`CsvOptions`]; a
//! `null_value` is applied by reading each file with the same arrow-csv reader
//! ([`read_csv`]), a column DataFusion infers as text only because of a float
//! spelling its inferer does not know reads as float64 ([`float_columns`]), and
//! a value that does not parse as its column's type is located in its file
//! ([`CsvValueError`]).

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use arrow::array::RecordBatch;
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::error::ArrowError;
use datafusion::datasource::listing::ListingTableUrl;
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::{CsvReadOptions, DataFrame, SessionContext};
use futures::StreamExt;
use object_store::ObjectStore;

use crate::scan::{arrow_err, ScanSource, SCAN_BATCH_ROWS};
use crate::store::scan_state;

/// How a scan parses a CSV file (#216). The defaults are how a scan always read
/// one: comma-separated, with a header row, `"` as the quote, no escape or
/// comment character, and only an empty field read as null. A Parquet file
/// ignores them.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CsvOptions {
    pub delimiter: u8,
    pub has_header: bool,
    pub quote: u8,
    pub escape: Option<u8>,
    pub comment: Option<u8>,
    /// A field that reads as null, as an empty field does (see [`read_csv`]).
    pub null_value: Option<String>,
}

impl Default for CsvOptions {
    fn default() -> Self {
        CsvOptions {
            delimiter: b',',
            has_header: true,
            quote: b'"',
            escape: None,
            comment: None,
            null_value: None,
        }
    }
}

impl CsvOptions {
    pub fn is_default(&self) -> bool {
        *self == CsvOptions::default()
    }

    /// DataFusion's read options for these: the null pattern matches an empty
    /// field or the null value, literally.
    pub(crate) fn read_options(&self) -> CsvReadOptions<'static> {
        let mut options = CsvReadOptions::new()
            .has_header(self.has_header)
            .delimiter(self.delimiter)
            .quote(self.quote);
        options.escape = self.escape;
        options.comment = self.comment;
        options.null_regex = self
            .null_value
            .as_ref()
            .map(|v| format!("^(?:{})?$", regex::escape(v)));
        options
    }
}

/// `ctx.read_csv(path)` with `csv`'s options.
///
/// DataFusion (54 and 55) applies a null pattern when it infers a CSV file's schema but
/// not when it reads the rows (its `CsvSource` never hands the pattern to the
/// arrow-csv reader), so a column with a `null_value` in it would infer as a
/// number and then fail to parse. With a `null_value`, the schema is inferred
/// as DataFusion infers it, with the pattern, and each file is read by the same
/// arrow-csv reader DataFusion uses, with the same options and that schema,
/// plus the pattern. Every value parses, or fails to, exactly as it does
/// without a `null_value`; the only difference is which fields read as null.
/// The files are read whole, one partition each, in path order, as a glob
/// lists them. A dry run reads no rows.
///
/// On both paths, a column DataFusion infers as text only because of a float
/// spelling its inferer does not know reads as float64 ([`float_columns`],
/// #221).
pub(crate) async fn read_csv(
    ctx: &SessionContext,
    path: &str,
    csv: &CsvOptions,
) -> Result<DataFrame> {
    use datafusion::datasource::MemTable;
    use object_store::ObjectStoreExt;
    let options = csv.read_options();
    let (inferred, widened) = infer_csv(ctx, path, csv).await?;
    let Some(pattern) = options.null_regex.clone() else {
        return match widened {
            None => Ok(inferred),
            Some(schema) => ctx.read_csv(path, options.schema(&schema)).await,
        };
    };
    let schema: SchemaRef = match widened {
        None => Arc::new(inferred.schema().as_arrow().clone()),
        Some(schema) => Arc::new(schema),
    };
    let mut partitions: Vec<Vec<RecordBatch>> = vec![];
    if !crate::dry::active() {
        let null =
            regex::Regex::new(&pattern).map_err(|e| DataFusionError::External(Box::new(e)))?;
        let url = ListingTableUrl::parse(path)?;
        let (store, files) = csv_files(ctx, &url, options.file_extension).await?;
        for file in files {
            let bytes = store
                .get(&file.location)
                .await
                .map_err(|e| DataFusionError::ObjectStore(Box::new(e)))?
                .bytes()
                .await
                .map_err(|e| DataFusionError::ObjectStore(Box::new(e)))?;
            let batches = csv_reader(schema.clone(), csv, Some(null.clone()))
                .build(std::io::Cursor::new(bytes))
                .map_err(arrow_err)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(arrow_err)?;
            partitions.push(batches);
        }
    }
    if partitions.is_empty() {
        partitions.push(vec![]);
    }
    ctx.read_table(Arc::new(MemTable::try_new(schema, partitions)?))
}

/// The CSV `path` as DataFusion infers it with `csv`'s options, and the
/// schema with #221's float columns ([`float_columns`]) when one is retyped.
async fn infer_csv(
    ctx: &SessionContext,
    path: &str,
    csv: &CsvOptions,
) -> Result<(DataFrame, Option<Schema>)> {
    let options = csv.read_options();
    let inferred = ctx.read_csv(path, csv.read_options()).await?;
    let widened = float_columns(ctx, path, &options, inferred.schema().as_arrow()).await?;
    Ok((inferred, widened))
}

/// The schema a scan reads the CSV `path` with ([`infer_csv`]).
async fn csv_schema(ctx: &SessionContext, path: &str, csv: &CsvOptions) -> Result<SchemaRef> {
    let (inferred, widened) = infer_csv(ctx, path, csv).await?;
    Ok(Arc::new(
        widened.unwrap_or_else(|| inferred.schema().as_arrow().clone()),
    ))
}

/// The reader DataFusion's `CsvSource::builder` makes for a CSV file with
/// `schema` and `csv`'s options, plus the null pattern `null`.
fn csv_reader(
    schema: SchemaRef,
    csv: &CsvOptions,
    null: Option<regex::Regex>,
) -> arrow::csv::ReaderBuilder {
    let mut reader = arrow::csv::ReaderBuilder::new(schema)
        .with_delimiter(csv.delimiter)
        .with_batch_size(SCAN_BATCH_ROWS)
        .with_header(csv.has_header)
        .with_quote(csv.quote)
        .with_truncated_rows(false);
    if let Some(null) = null {
        reader = reader.with_null_regex(null);
    }
    if let Some(escape) = csv.escape {
        reader = reader.with_escape(escape);
    }
    if let Some(comment) = csv.comment {
        reader = reader.with_comment(comment);
    }
    reader
}

/// The files `url` names with the extension `ext`, in path order, as a glob
/// lists them, and their store.
async fn csv_files(
    ctx: &SessionContext,
    url: &ListingTableUrl,
    ext: &str,
) -> Result<(Arc<dyn ObjectStore>, Vec<object_store::ObjectMeta>)> {
    use futures::TryStreamExt;
    let state = ctx.state();
    let store = state.runtime_env().object_store(url.object_store())?;
    let mut files = url
        .list_all_files(&state, store.as_ref(), ext)
        .await?
        .try_collect::<Vec<_>>()
        .await?;
    files.sort_by(|a, b| a.location.cmp(&b.location));
    Ok((store, files))
}

/// The arrow-csv error under `e` for a value that does not parse as its
/// column's type, if `e` is one.
fn csv_parse_error(e: &DataFusionError) -> Option<&arrow::error::ArrowError> {
    use arrow::error::ArrowError;
    fn external<'a>(x: &'a (dyn std::error::Error + 'static)) -> Option<&'a ArrowError> {
        if let Some(e) = x.downcast_ref::<DataFusionError>() {
            csv_parse_error(e)
        } else {
            x.downcast_ref::<ArrowError>()
                .filter(|a| csv_parse_fields(a).is_some())
        }
    }
    match e {
        DataFusionError::Context(_, inner) | DataFusionError::Diagnostic(_, inner) => {
            csv_parse_error(inner)
        }
        DataFusionError::Shared(inner) => csv_parse_error(inner),
        DataFusionError::ArrowError(a, _) => match a.as_ref() {
            ArrowError::ExternalError(x) => external(x.as_ref()),
            a => csv_parse_fields(a).map(|_| a),
        },
        DataFusionError::External(x) => external(x.as_ref()),
        _ => None,
    }
}

/// `e` as a [`CsvValueError`] when it is arrow-csv's error for a value that
/// does not parse as its column's type; otherwise `e` as it is.
///
/// DataFusion may read a large file in byte ranges, in parallel, and arrow-csv
/// counts lines from the start of a range. So the scan reads the failing column
/// again, file by file in path order, with one reader per file, and reports
/// the first value that does not parse, its line in its file, and the file. The
/// message is the same on any core count, with or without a `null_value`.
pub(crate) fn csv_value_error(e: DataFusionError, source: ScanSource<'_>) -> DataFusionError {
    let Some(fields) = csv_parse_error(&e).and_then(csv_parse_fields) else {
        return e;
    };
    match crate::runtime::block_on(locate_csv_value(source, fields.column)) {
        Ok(Ok(Some(found))) => DataFusionError::External(Box::new(found)),
        _ => e,
    }
}

/// The first value of column `column` in `source`'s CSV files that does not
/// parse as the column's type (see [`csv_value_error`]), or `None` if every
/// value parses.
async fn locate_csv_value(source: ScanSource<'_>, column: usize) -> Result<Option<CsvValueError>> {
    use object_store::ObjectStoreExt;
    let options = source.csv.read_options();
    let null = match &options.null_regex {
        Some(pattern) => {
            Some(regex::Regex::new(pattern).map_err(|e| DataFusionError::External(Box::new(e)))?)
        }
        None => None,
    };
    for path in source.paths {
        let ctx = SessionContext::new_with_state(scan_state(path, source.storage_options)?);
        let schema = csv_schema(&ctx, path, source.csv).await?;
        let Some(field) = schema.fields().get(column).cloned() else {
            return Ok(None);
        };
        let url = ListingTableUrl::parse(path)?;
        let named = source.paths.len() > 1 || url.get_glob().is_some() || url.is_collection();
        let (store, files) = csv_files(&ctx, &url, options.file_extension).await?;
        for file in files {
            let mut decoder = csv_reader(schema.clone(), source.csv, null.clone())
                .with_projection(vec![column])
                .build_decoder();
            let mut chunks = store
                .get(&file.location)
                .await
                .map_err(|e| DataFusionError::ObjectStore(Box::new(e)))?
                .into_stream();
            let mut buf = None;
            let failed = loop {
                let bytes = match buf.take() {
                    Some(bytes) => bytes,
                    None => match chunks.next().await {
                        Some(bytes) => {
                            bytes.map_err(|e| DataFusionError::ObjectStore(Box::new(e)))?
                        }
                        None => Default::default(),
                    },
                };
                let decoded = decoder.decode(&bytes).map_err(arrow_err)?;
                if decoded < bytes.len() {
                    buf = Some(bytes.slice(decoded..));
                }
                // A batch is full, or the file has ended.
                if decoded == 0 || decoder.capacity() == 0 {
                    match decoder.flush() {
                        Ok(Some(_)) => {}
                        Ok(None) => break None,
                        Err(a) => break Some(a),
                    }
                }
            };
            let Some(a) = failed else { continue };
            let Some(at) = csv_parse_fields(&a) else {
                return Err(arrow_err(a));
            };
            let file = named.then(|| match url.scheme() {
                "file" => format!("/{}", file.location),
                _ => format!("{}{}", url.object_store().as_str(), file.location),
            });
            return Ok(Some(CsvValueError {
                column: field.name().clone(),
                data_type: field.data_type().clone(),
                inferred_from: options.schema_infer_max_records,
                value: at.value,
                line: at.line + 1,
                file,
                source: a,
            }));
        }
    }
    Ok(None)
}

/// How arrow-csv's inferer reads one CSV value: as text, an integer, or a float.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Spelling {
    Text,
    Int,
    /// A float arrow-csv's inferer knows: `1.5`, `-1e3`, `nan`, `-inf`.
    Float,
    /// A float pyarrow reads that arrow-csv's inferer reads as text: a leading
    /// `+` (`+5`), `Inf`, `-nan`, `Infinity` and the other cases of the names.
    PyarrowFloat,
}

/// The spelling of one non-null CSV value. The first three cases are arrow-csv
/// 58's `InferredDataType::update`, so a value is `Int` or `Float` exactly
/// when arrow-csv would read it so; a value it reads as a date, a time or a
/// boolean is `Text` here.
fn spelling(s: &str) -> Spelling {
    static INT: OnceLock<regex::Regex> = OnceLock::new();
    static FLOAT: OnceLock<regex::Regex> = OnceLock::new();
    static PYARROW: OnceLock<regex::Regex> = OnceLock::new();
    let int = INT.get_or_init(|| regex::Regex::new(r"^-?(\d+)$").expect("valid regex"));
    let float = FLOAT.get_or_init(|| {
        regex::Regex::new(r"^-?((\d*\.\d+|\d+\.\d*)([eE][-+]?\d+)?|\d+([eE][-+]?\d+))$")
            .expect("valid regex")
    });
    // pyarrow's float grammar: a sign, digits with an optional point (or a
    // point and digits), an optional exponent; or a sign and nan, inf or
    // infinity in any case.
    let pyarrow = PYARROW.get_or_init(|| {
        regex::Regex::new(
            r"^[+-]?(?:(?:[0-9]+\.?[0-9]*|\.[0-9]+)(?:[eE][+-]?[0-9]+)?|(?i:nan|inf|infinity))$",
        )
        .expect("valid regex")
    });
    if s.starts_with('"') {
        Spelling::Text
    } else if int.is_match(s) {
        // arrow-csv reads an integer past int64 as text.
        if s.len() >= 19 && s.parse::<i64>().is_err() {
            Spelling::Text
        } else {
            Spelling::Int
        }
    } else if float.is_match(s) || matches!(s, "NaN" | "nan" | "inf" | "-inf") {
        Spelling::Float
    } else if pyarrow.is_match(s) {
        Spelling::PyarrowFloat
    } else {
        Spelling::Text
    }
}

/// `inferred` (the schema DataFusion infers for the CSV `path`) with each
/// `string` column that is text only because of a pyarrow float spelling
/// retyped `float64`, or `None` when no column is (#221).
///
/// arrow-csv's inferer reads `+5`, `Inf` or `-nan` as text, so one such value
/// makes the whole column `string`, where pyarrow reads `double`. This reads
/// the values DataFusion's inference read (the same files in the same order,
/// cut into the same chunks, each parsed as arrow-csv parses it, up to the same
/// record count) and retypes a column when every non-null value is a number
/// and one or more is a [`Spelling::PyarrowFloat`]. A column that is text for
/// any other reason, or not text, keeps its type. The float64 reader
/// (lexical-core) parses each pyarrow spelling to the bits pyarrow gives.
async fn float_columns(
    ctx: &SessionContext,
    path: &str,
    options: &CsvReadOptions<'_>,
    inferred: &Schema,
) -> Result<Option<Schema>> {
    use datafusion::datasource::file_format::csv::CsvFormat;
    use futures::TryStreamExt;
    use object_store::ObjectStoreExt;
    // Per text column: (every value a number, a pyarrow spelling seen).
    let mut columns: HashMap<String, (bool, bool)> = inferred
        .fields()
        .iter()
        .filter(|f| f.data_type() == &DataType::Utf8)
        .map(|f| (f.name().clone(), (true, false)))
        .collect();
    if columns.is_empty() {
        return Ok(None);
    }
    let null = options
        .null_regex
        .as_deref()
        .map(regex::Regex::new)
        .transpose()
        .map_err(|e| DataFusionError::External(Box::new(e)))?;
    let is_null = |s: &str| match &null {
        Some(r) => r.is_match(s),
        None => s.is_empty(),
    };
    let csv_err = |e: ::csv::Error| DataFusionError::External(Box::new(e));
    // As `ListingOptions::infer_schema` and `CsvFormat::infer_schema` read.
    let state = ctx.state();
    let url = ListingTableUrl::parse(path)?;
    let store = state.runtime_env().object_store(url.object_store())?;
    let files: Vec<_> = url
        .list_all_files(&state, store.as_ref(), options.file_extension)
        .await?
        .try_filter(|f| futures::future::ready(f.size > 0))
        .try_collect()
        .await?;
    let mut remaining = options.schema_infer_max_records;
    'files: for file in files {
        let bytes = store
            .get(&file.location)
            .await
            .map_err(|e| DataFusionError::ObjectStore(Box::new(e)))?
            .into_stream()
            .map_err(|e| DataFusionError::ObjectStore(Box::new(e)))
            .boxed();
        let mut chunks = CsvFormat::default().read_to_delimited_chunks_from_stream(bytes);
        let mut names: Vec<String> = vec![];
        let mut first = true;
        while let Some(chunk) = chunks.next().await.transpose()? {
            // The reader arrow-csv's `Format::infer_schema` builds.
            let header = first && options.has_header;
            let mut reader = ::csv::ReaderBuilder::new();
            reader
                .has_headers(header)
                .flexible(false)
                .delimiter(options.delimiter)
                .escape(options.escape)
                .quote(options.quote);
            if let Some(comment) = options.comment {
                reader.comment(Some(comment));
            }
            let mut reader = reader.from_reader(&chunk[..]);
            let headers = reader.headers().map_err(csv_err)?;
            if first {
                names = if header {
                    headers.iter().map(str::to_string).collect()
                } else {
                    (1..=headers.len()).map(|i| format!("column_{i}")).collect()
                };
                first = false;
            }
            let mut record = ::csv::StringRecord::new();
            let mut read = 0;
            while read < remaining && reader.read_record(&mut record).map_err(csv_err)? {
                read += 1;
                for (name, value) in names.iter().zip(record.iter()) {
                    let Some((numbers, pyarrow)) = columns.get_mut(name) else {
                        continue;
                    };
                    if is_null(value) {
                        continue;
                    }
                    match spelling(value) {
                        Spelling::Text => *numbers = false,
                        Spelling::PyarrowFloat => *pyarrow = true,
                        Spelling::Int | Spelling::Float => {}
                    }
                }
            }
            remaining -= read;
            if remaining == 0 {
                break 'files;
            }
        }
    }
    if !columns
        .values()
        .any(|&(numbers, pyarrow)| numbers && pyarrow)
    {
        return Ok(None);
    }
    let fields: Vec<Field> = inferred
        .fields()
        .iter()
        .map(|f| match columns.get(f.name()) {
            Some(&(true, true)) => f.as_ref().clone().with_data_type(DataType::Float64),
            _ => f.as_ref().clone(),
        })
        .collect();
    Ok(Some(Schema::new_with_metadata(
        fields,
        inferred.metadata().clone(),
    )))
}

/// A CSV value that does not parse as its column's inferred type (#222), as the
/// scan located it, carried in a `DataFusionError::External`. Its `Display` is
/// the engine's text; the host words it for its users (ursa-plan's
/// `engine_message`).
#[derive(Debug)]
pub struct CsvValueError {
    /// The column's name.
    pub column: String,
    /// The column's inferred type.
    pub data_type: DataType,
    /// How many rows the types are inferred from.
    pub inferred_from: usize,
    /// The value, when the engine's error has it.
    pub value: Option<String>,
    /// The line of the file, from 1. The header is line 1.
    pub line: usize,
    /// The file, when the scan's paths can name more than one.
    pub file: Option<String>,
    /// The engine's error.
    pub source: ArrowError,
}

impl std::fmt::Display for CsvValueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(f)
    }
}

impl std::error::Error for CsvValueError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// What arrow-csv's error for a value that does not parse as its column's type
/// says: [`csv_parse_fields`].
#[derive(Debug, PartialEq, Eq)]
pub struct CsvParseFields {
    /// The column's index in the file.
    pub column: usize,
    /// arrow-csv's line: the record's index from 0, the header included.
    pub line: usize,
    /// The value, when the error has it.
    pub value: Option<String>,
}

/// The fields of arrow-csv's error for a value that does not parse as its
/// column's type, or `None` for any other error. Arrow carries them as text
/// only (`ArrowError::ParseError`), in one of two fixed forms: one for a
/// boolean or a number or a date, and one for a timestamp.
pub fn csv_parse_fields(a: &ArrowError) -> Option<CsvParseFields> {
    let ArrowError::ParseError(m) = a else {
        return None;
    };
    // `Error while parsing value '{value}' as type '{type}' for column {i} at
    // line {n}. Row data: '{row}'`. The value may hold any text, so each
    // `' as type '` is tried until the rest has this form.
    if let Some(rest) = m.strip_prefix("Error while parsing value '") {
        for (i, _) in rest.match_indices("' as type '") {
            // The type's name has no quote in it.
            let Some((ty, tail)) = rest[i + "' as type '".len()..].split_once("' for column ")
            else {
                continue;
            };
            if ty.contains('\'') {
                continue;
            }
            let Some((column, tail)) = number_then(tail, " at line ") else {
                continue;
            };
            let Some((line, _)) = number_then(tail, ". Row data: '") else {
                continue;
            };
            return Some(CsvParseFields {
                column,
                line,
                value: Some(rest[..i].to_string()),
            });
        }
        return None;
    }
    // `Error parsing column {i} at line {n}: {cause}`, where the cause names
    // the value unless the value's time zone or range was the problem.
    let rest = m.strip_prefix("Error parsing column ")?;
    let (column, rest) = number_then(rest, " at line ")?;
    let (line, cause) = number_then(rest, ": ")?;
    let value = cause
        .strip_prefix("Parser error: Error parsing timestamp from '")
        .and_then(|v| v.rsplit_once("': "))
        .map(|(v, _)| v.to_string());
    Some(CsvParseFields {
        column,
        line,
        value,
    })
}

/// The number `s` starts with, and the text after the `sep` that follows it.
fn number_then<'a>(s: &'a str, sep: &str) -> Option<(usize, &'a str)> {
    let digits = s.bytes().take_while(u8::is_ascii_digit).count();
    let rest = s[digits..].strip_prefix(sep)?;
    Some((s[..digits].parse().ok()?, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pyarrow_float_spelling_is_a_float() {
        use Spelling::*;
        for (s, want) in [
            ("1", Int),
            ("-0", Int),
            ("9223372036854775808", Text),
            ("1.5", Float),
            ("1e400", Float),
            ("nan", Float),
            ("-inf", Float),
            ("+5", PyarrowFloat),
            ("+.5", PyarrowFloat),
            ("Inf", PyarrowFloat),
            ("-nan", PyarrowFloat),
            ("+Infinity", PyarrowFloat),
            ("+9223372036854775808", PyarrowFloat),
            ("true", Text),
            ("2020-01-01", Text),
            ("1e", Text),
            ("++5", Text),
            (" 5", Text),
            ("inff", Text),
            ("\"5", Text),
            ("\u{0665}", Int), // arrow-csv's `\d` is Unicode
            ("+\u{0665}", Text),
        ] {
            assert!(spelling(s) == want, "{s}");
        }
    }

    #[test]
    fn reads_the_fields_of_a_csv_parse_error() {
        let parse = |m: &str| csv_parse_fields(&ArrowError::ParseError(m.into()));
        assert_eq!(
            parse(
                "Error while parsing value 'yes' as type 'Boolean' for column 2 at line 1001. \
                 Row data: '[2,3,yes]'"
            ),
            Some(CsvParseFields {
                column: 2,
                line: 1001,
                value: Some("yes".into())
            })
        );
        // A value may hold the text that follows it.
        assert_eq!(
            parse(
                "Error while parsing value 'a' as type 'x' for' as type 'Int64' for column 0 at \
                 line 3. Row data: '[a' as type 'x' for]'"
            )
            .and_then(|f| f.value),
            Some("a' as type 'x' for".into())
        );
        assert_eq!(
            parse(
                "Error parsing column 1 at line 7: Parser error: Error parsing timestamp from \
                 'soon': timestamp must contain at least 10 characters"
            ),
            Some(CsvParseFields {
                column: 1,
                line: 7,
                value: Some("soon".into())
            })
        );
        assert_eq!(
            parse("Error parsing column 1 at line 7: Parser error: Invalid timezone \"X\"")
                .map(|f| f.value),
            Some(None)
        );
        assert_eq!(parse("encountered unterminated string"), None);
        assert_eq!(csv_parse_fields(&ArrowError::DivideByZero), None);
    }
}
