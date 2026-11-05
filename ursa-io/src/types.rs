//! Arrow types as a user reads them in a message.

use arrow::datatypes::{DataType, TimeUnit};

/// pyarrow's spelling of an Arrow type, as its messages print it.
pub fn pa_name(dt: &DataType) -> String {
    use DataType::*;
    let unit = |u: &TimeUnit| match u {
        TimeUnit::Second => "s",
        TimeUnit::Millisecond => "ms",
        TimeUnit::Microsecond => "us",
        TimeUnit::Nanosecond => "ns",
    };
    match dt {
        Null => "null".into(),
        Boolean => "bool".into(),
        Int8 | Int16 | Int32 | Int64 | UInt8 | UInt16 | UInt32 | UInt64 => {
            dt.to_string().to_lowercase()
        }
        Float16 => "halffloat".into(),
        Float32 => "float".into(),
        Float64 => "double".into(),
        Utf8 => "string".into(),
        LargeUtf8 => "large_string".into(),
        Utf8View => "string_view".into(),
        Binary => "binary".into(),
        LargeBinary => "large_binary".into(),
        Date32 => "date32[day]".into(),
        Date64 => "date64[ms]".into(),
        Timestamp(u, None) => format!("timestamp[{}]", unit(u)),
        Timestamp(u, Some(tz)) => format!("timestamp[{}, tz={tz}]", unit(u)),
        Duration(u) => format!("duration[{}]", unit(u)),
        Time32(u) => format!("time32[{}]", unit(u)),
        Time64(u) => format!("time64[{}]", unit(u)),
        Decimal128(p, s) => format!("decimal128({p}, {s})"),
        Decimal256(p, s) => format!("decimal256({p}, {s})"),
        List(f) | LargeList(f) => format!("list<item: {}>", pa_name(f.data_type())),
        Dictionary(k, v) => format!(
            "dictionary<values={}, indices={}, ordered=0>",
            pa_name(v),
            pa_name(k)
        ),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pyarrow_type_names() {
        assert_eq!(pa_name(&DataType::Int64), "int64");
        assert_eq!(pa_name(&DataType::Float64), "double");
        assert_eq!(
            pa_name(&DataType::Timestamp(
                TimeUnit::Nanosecond,
                Some("UTC".into())
            )),
            "timestamp[ns, tz=UTC]"
        );
        assert_eq!(pa_name(&DataType::Decimal128(5, 2)), "decimal128(5, 2)");
    }
}
