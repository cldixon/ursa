//! The text of a value, as the table, CSV and TSV write it.
//!
//! A scalar is Arrow's text for it. A list, struct or map is written as
//! Arrow writes it (`[1, 2]`, `{a: 1}`, `{k: v}`), with `NULL` for a null
//! inside it, but a string inside it is quoted as DuckDB quotes one, so the
//! text says where each element starts and ends:
//!
//! - a string is written in single quotes when it is empty, is `NULL` in any
//!   case, starts or ends with whitespace, or holds any of
//!   `' " \ , : = [ ] { } ( )`; otherwise it is written as it is;
//! - inside the quotes, a `'` or `\` is escaped with a `\`.
//!
//! So `['a, b', 'c']` is `['a, b', c]`, `['a', 'b', 'c']` is `[a, b, c]`,
//! and `['x]', NULL, 'NULL']` is `['x]', NULL, 'NULL']`. A struct's field
//! names and a map's keys follow the same rule. JSON is the lossless format
//! for nested values: this text is for reading.

use std::fmt::Write;

use arrow::array::{Array, AsArray, StructArray};
use arrow::datatypes::{DataType, Field};
use arrow::error::ArrowError;
use arrow::util::display::{
    ArrayFormatter, ArrayFormatterFactory, DisplayIndex, FormatOptions, FormatResult,
};

/// A formatter for `array` that writes nested values as the module's docs
/// say. A null at the top is the caller's to write; `options` say how the
/// scalars inside are written, except that a null inside is always `NULL`.
pub fn formatter<'a>(
    array: &'a dyn Array,
    options: &FormatOptions<'a>,
) -> Result<ArrayFormatter<'a>, ArrowError> {
    if !is_nested(array.data_type()) {
        return ArrayFormatter::try_new(array, options);
    }
    let options = options
        .clone()
        .with_null("NULL")
        .with_formatter_factory(Some(&NestedText));
    inner(array, &options, None)
}

/// Whether values of type `t` hold other values.
fn is_nested(t: &DataType) -> bool {
    match t {
        DataType::List(_)
        | DataType::LargeList(_)
        | DataType::ListView(_)
        | DataType::LargeListView(_)
        | DataType::FixedSizeList(..)
        | DataType::Struct(_)
        | DataType::Map(..)
        | DataType::Union(..) => true,
        DataType::Dictionary(_, values) => is_nested(values),
        DataType::RunEndEncoded(_, values) => is_nested(values.data_type()),
        _ => false,
    }
}

/// Arrow asks this for the formatter of each array inside a nested one: a
/// string array is quoted, a struct array quotes its field names, a
/// dictionary looks its values up in a formatter made here (Arrow's would not
/// ask), and any other array is Arrow's (which asks again for the arrays
/// inside it).
#[derive(Debug)]
struct NestedText;

impl ArrayFormatterFactory for NestedText {
    fn create_array_formatter<'a>(
        &self,
        array: &'a dyn Array,
        options: &FormatOptions<'a>,
        _field: Option<&'a Field>,
    ) -> Result<Option<ArrayFormatter<'a>>, ArrowError> {
        Ok(match array.data_type() {
            DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => Some(ArrayFormatter::new(
                Box::new(Strings {
                    array,
                    null: options.null(),
                }),
                options.safe(),
            )),
            DataType::Struct(_) => Some(struct_formatter(array.as_struct(), options)?),
            DataType::Dictionary(..) => {
                let dictionary = array.as_any_dictionary();
                Some(ArrayFormatter::new(
                    Box::new(Dictionary {
                        array,
                        keys: dictionary.normalized_keys(),
                        values: inner(dictionary.values().as_ref(), options, None)?,
                        null: options.null(),
                    }),
                    options.safe(),
                ))
            }
            _ => None,
        })
    }
}

/// The formatter of an array inside a nested one.
fn inner<'a>(
    array: &'a dyn Array,
    options: &FormatOptions<'a>,
    field: Option<&'a Field>,
) -> Result<ArrayFormatter<'a>, ArrowError> {
    match NestedText.create_array_formatter(array, options, field)? {
        Some(formatter) => Ok(formatter),
        None => ArrayFormatter::try_new(array, options),
    }
}

/// Strings inside a nested value, quoted where they need it.
struct Strings<'a> {
    array: &'a dyn Array,
    null: &'a str,
}

impl DisplayIndex for Strings<'_> {
    fn write(&self, idx: usize, f: &mut dyn Write) -> FormatResult {
        if self.array.is_null(idx) {
            f.write_str(self.null)?;
            return Ok(());
        }
        let value = match self.array.data_type() {
            DataType::Utf8 => self.array.as_string::<i32>().value(idx),
            DataType::LargeUtf8 => self.array.as_string::<i64>().value(idx),
            _ => self.array.as_string_view().value(idx),
        };
        write_string(value, f)?;
        Ok(())
    }
}

/// A dictionary inside a nested value: its values' text.
struct Dictionary<'a> {
    array: &'a dyn Array,
    keys: Vec<usize>,
    values: ArrayFormatter<'a>,
    null: &'a str,
}

impl DisplayIndex for Dictionary<'_> {
    fn write(&self, idx: usize, f: &mut dyn Write) -> FormatResult {
        if self.array.is_null(idx) {
            f.write_str(self.null)?;
        } else {
            write!(f, "{}", self.values.value(self.keys[idx]))?;
        }
        Ok(())
    }
}

fn struct_formatter<'a>(
    array: &'a StructArray,
    options: &FormatOptions<'a>,
) -> Result<ArrayFormatter<'a>, ArrowError> {
    let fields = array
        .columns()
        .iter()
        .zip(array.fields())
        .map(|(column, field)| {
            Ok((
                field.name().as_str(),
                inner(column.as_ref(), options, Some(field))?,
            ))
        })
        .collect::<Result<_, ArrowError>>()?;
    Ok(ArrayFormatter::new(
        Box::new(Struct {
            array,
            fields,
            null: options.null(),
        }),
        options.safe(),
    ))
}

/// A struct, `{name: value, …}`, its names quoted where they need it.
struct Struct<'a> {
    array: &'a StructArray,
    fields: Vec<(&'a str, ArrayFormatter<'a>)>,
    null: &'a str,
}

impl DisplayIndex for Struct<'_> {
    fn write(&self, idx: usize, f: &mut dyn Write) -> FormatResult {
        if self.array.is_null(idx) {
            f.write_str(self.null)?;
            return Ok(());
        }
        f.write_char('{')?;
        for (i, (name, value)) in self.fields.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write_string(name, f)?;
            write!(f, ": {}", value.value(idx))?;
        }
        f.write_char('}')?;
        Ok(())
    }
}

/// Write `value`, a string inside a nested value, quoted if it needs it.
fn write_string(value: &str, f: &mut dyn Write) -> std::fmt::Result {
    if !needs_quotes(value) {
        return f.write_str(value);
    }
    f.write_char('\'')?;
    for c in value.chars() {
        if c == '\'' || c == '\\' {
            f.write_char('\\')?;
        }
        f.write_char(c)?;
    }
    f.write_char('\'')
}

/// Whether a string inside a nested value would be mistaken for something
/// else written bare: see the module's docs.
fn needs_quotes(value: &str) -> bool {
    value.is_empty()
        || value.eq_ignore_ascii_case("null")
        || value.starts_with(char::is_whitespace)
        || value.ends_with(char::is_whitespace)
        || value.chars().any(|c| {
            matches!(
                c,
                '\'' | '"' | '\\' | ',' | ':' | '=' | '[' | ']' | '{' | '}' | '(' | ')'
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use arrow::array::{
        ArrayRef, Int64Array, LargeStringArray, ListArray, MapBuilder, StringArray, StringBuilder,
        StringDictionaryBuilder, StringViewArray,
    };
    use arrow::datatypes::{Int32Type, Int64Type};

    fn text(array: &dyn Array) -> Vec<String> {
        let options = FormatOptions::default();
        let f = formatter(array, &options).unwrap();
        (0..array.len()).map(|i| f.value(i).to_string()).collect()
    }

    fn list(values: ArrayRef, offsets: &[i32]) -> ListArray {
        let field = Arc::new(Field::new_list_field(values.data_type().clone(), true));
        ListArray::new(
            field,
            arrow::buffer::OffsetBuffer::new(offsets.to_vec().into()),
            values,
            None,
        )
    }

    fn strings(values: &[Option<&str>]) -> ArrayRef {
        Arc::new(StringArray::from(values.to_vec()))
    }

    #[test]
    fn lists_that_differ_print_differently() {
        let l = list(
            strings(&[Some("a, b"), Some("c"), Some("a"), Some("b"), Some("c")]),
            &[0, 2, 5],
        );
        assert_eq!(text(&l), ["['a, b', c]", "[a, b, c]"]);
    }

    #[test]
    fn a_null_element_is_not_the_string_null() {
        let l = list(
            strings(&[Some("x]"), None, Some("NULL"), Some("null"), Some("")]),
            &[0, 5],
        );
        assert_eq!(text(&l), ["['x]', NULL, 'NULL', 'null', '']"]);
    }

    #[test]
    fn quotes_and_backslashes_are_escaped() {
        let l = list(
            strings(&[
                Some("it's"),
                Some(r"a\b"),
                Some("say \"hi\""),
                Some("plain"),
            ]),
            &[0, 4],
        );
        assert_eq!(text(&l), [r#"['it\'s', 'a\\b', 'say "hi"', plain]"#]);
    }

    #[test]
    fn whitespace_at_the_ends_is_quoted_inside_is_not() {
        let l = list(strings(&[Some(" a"), Some("a "), Some("a b")]), &[0, 3]);
        assert_eq!(text(&l), ["[' a', 'a ', a b]"]);
    }

    #[test]
    fn struct_fields_and_names_are_quoted() {
        let s = StructArray::from(vec![
            (
                Arc::new(Field::new("k", DataType::Utf8, true)),
                strings(&[Some("v, w"), None]),
            ),
            (
                Arc::new(Field::new("a b", DataType::Utf8, true)),
                strings(&[Some("x"), Some("y")]),
            ),
            (
                Arc::new(Field::new("k:", DataType::Int64, true)),
                Arc::new(Int64Array::from(vec![1, 2])) as ArrayRef,
            ),
        ]);
        assert_eq!(
            text(&s),
            ["{k: 'v, w', a b: x, 'k:': 1}", "{k: NULL, a b: y, 'k:': 2}"]
        );
    }

    #[test]
    fn a_struct_in_a_list() {
        let s = StructArray::from(vec![(
            Arc::new(Field::new("id", DataType::Utf8, true)),
            strings(&[Some("https://x.org/a,b"), Some("b")]),
        )]);
        let l = list(Arc::new(s), &[0, 2]);
        assert_eq!(text(&l), ["[{id: 'https://x.org/a,b'}, {id: b}]"]);
    }

    #[test]
    fn map_keys_and_values_are_quoted() {
        let mut b = MapBuilder::new(None, StringBuilder::new(), StringBuilder::new());
        b.keys().append_value("a, b");
        b.values().append_value("x");
        b.keys().append_value("c");
        b.values().append_null();
        b.keys().append_value("d");
        b.values().append_value("it's");
        b.append(true).unwrap();
        let m = b.finish();
        assert_eq!(text(&m), [r"{'a, b': x, c: NULL, d: 'it\'s'}"]);
    }

    #[test]
    fn every_string_type_is_quoted() {
        let large: ArrayRef = Arc::new(LargeStringArray::from(vec!["a,b"]));
        let view: ArrayRef = Arc::new(StringViewArray::from(vec!["a,b"]));
        let mut dict = StringDictionaryBuilder::<Int32Type>::new();
        dict.append_value("a,b");
        let dict: ArrayRef = Arc::new(dict.finish());
        for values in [large, view, dict] {
            assert_eq!(text(&list(values, &[0, 1])), ["['a,b']"]);
        }
    }

    #[test]
    fn a_string_at_the_top_is_not_quoted() {
        assert_eq!(text(strings(&[Some("a, b")]).as_ref()), ["a, b"]);
        let mut dict = StringDictionaryBuilder::<Int32Type>::new();
        dict.append_value("a, b");
        assert_eq!(text(&dict.finish()), ["a, b"]);
    }

    #[test]
    fn numbers_inside_are_arrows() {
        let l = ListArray::from_iter_primitive::<Int64Type, _, _>(vec![Some(vec![
            Some(1),
            None,
            Some(3),
        ])]);
        assert_eq!(text(&l), ["[1, NULL, 3]"]);
    }
}
