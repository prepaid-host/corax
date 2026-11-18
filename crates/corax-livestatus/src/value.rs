//! Column types and cell values.

/// Livestatus column type as reported by the `columns` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Int,
    Float,
    String,
    List,
    Dict,
    Time,
    Blob,
    DictDouble,
}

impl ColumnType {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "int" => Self::Int,
            "float" => Self::Float,
            "string" => Self::String,
            "list" => Self::List,
            "dict" => Self::Dict,
            "time" => Self::Time,
            "blob" => Self::Blob,
            "dictdouble" => Self::DictDouble,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Int => "int",
            Self::Float => "float",
            Self::String => "string",
            Self::List => "list",
            Self::Dict => "dict",
            Self::Time => "time",
            Self::Blob => "blob",
            Self::DictDouble => "dictdouble",
        }
    }

    /// Type-correct "empty" value used for columns without data.
    pub fn default_value(self) -> Value {
        match self {
            Self::Int | Self::Time => Value::Int(0),
            Self::Float => Value::Float(0.0),
            Self::String => Value::Str(String::new()),
            Self::List => Value::List(Vec::new()),
            Self::Dict | Self::DictDouble => Value::Dict(Vec::new()),
            Self::Blob => Value::Blob(Vec::new()),
        }
    }
}

/// One cell of a result row.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Float(f64),
    Str(String),
    Blob(Vec<u8>),
    /// List; elements may themselves be lists ("sublists").
    List(Vec<Value>),
    /// Ordered dictionary.
    Dict(Vec<(String, Value)>),
}

impl Value {
    pub fn str(s: impl Into<String>) -> Self {
        Value::Str(s.into())
    }

    pub fn bool(b: bool) -> Self {
        Value::Int(b as i64)
    }

    pub fn str_list<I, S>(items: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Value::List(items.into_iter().map(|s| Value::Str(s.into())).collect())
    }

    pub fn str_dict<I, K, V>(items: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Value::Dict(items.into_iter().map(|(k, v)| (k.into(), Value::Str(v.into()))).collect())
    }

    /// Numeric view used by filters and stats aggregations.
    pub fn as_f64(&self) -> f64 {
        match self {
            Value::Int(i) => *i as f64,
            Value::Float(f) => *f,
            Value::Str(s) => s.trim().parse().unwrap_or(0.0),
            _ => 0.0,
        }
    }

    pub fn as_i64(&self) -> i64 {
        match self {
            Value::Int(i) => *i,
            Value::Float(f) => *f as i64,
            Value::Str(s) => s.trim().parse().unwrap_or(0),
            _ => 0,
        }
    }

    /// String view used by string filters and list element matching.
    pub fn as_text(&self) -> String {
        match self {
            Value::Null => String::new(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => format_double(*f),
            Value::Str(s) => s.clone(),
            Value::Blob(b) => String::from_utf8_lossy(b).into_owned(),
            // a sublist is matched by its first element (usually the name)
            Value::List(l) => l.first().map(Value::as_text).unwrap_or_default(),
            Value::Dict(_) => String::new(),
        }
    }
}

/// Format a double like C++ `std::ostream << double` with default precision (`%g`, 6 digits).
pub fn format_double(v: f64) -> String {
    if v.is_nan() {
        return "nan".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if v == 0.0 {
        return "0".into();
    }
    const PREC: i32 = 6;
    let exp = v.abs().log10().floor() as i32;
    // %g uses scientific notation if exp < -4 or exp >= precision
    let rounded_exp = {
        // rounding may bump the exponent (e.g. 999999.5 -> 1e+06)
        let s = format!("{:.*e}", (PREC - 1) as usize, v);
        s.split('e').nth(1).and_then(|e| e.parse::<i32>().ok()).unwrap_or(exp)
    };
    if !(-4..PREC).contains(&rounded_exp) {
        let s = format!("{:.*e}", (PREC - 1) as usize, v);
        let (mant, e) = s.split_once('e').unwrap_or((&s, "0"));
        let mant = trim_fraction(mant);
        let e: i32 = e.parse().unwrap_or(0);
        let sign = if e < 0 { '-' } else { '+' };
        format!("{mant}e{sign}{:02}", e.abs())
    } else {
        let decimals = (PREC - 1 - rounded_exp).max(0) as usize;
        trim_fraction(&format!("{v:.decimals$}")).to_string()
    }
}

fn trim_fraction(s: &str) -> &str {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_formatting_matches_ostream() {
        assert_eq!(format_double(1.0), "1");
        assert_eq!(format_double(0.018), "0.018");
        assert_eq!(format_double(0.0666667), "0.0666667");
        assert_eq!(format_double(0.06666666666), "0.0666667");
        assert_eq!(format_double(1.01649123), "1.01649");
        assert_eq!(format_double(1234567.0), "1.23457e+06");
        assert_eq!(format_double(150000.0), "150000");
        assert_eq!(format_double(0.00001), "1e-05");
        assert_eq!(format_double(-2.5), "-2.5");
        assert_eq!(format_double(999999.7), "1e+06");
    }

    #[test]
    fn defaults() {
        assert_eq!(ColumnType::List.default_value(), Value::List(vec![]));
        assert_eq!(ColumnType::Time.default_value(), Value::Int(0));
    }
}
