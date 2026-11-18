//! Checkmk encodes labels and tags as hex custom variables:
//!
//! * `__LABEL_<hex name> <hex value>` (World key: `_LABEL_<HEX>`)
//! * `__LABELSOURCE_<hex name> <hex source>`
//! * `__TAG_<hex group> <hex tag id>`
//!
//! Livestatus decodes them into the `labels`, `label_sources` and `tags` columns and
//! hides them from `custom_variables`.

/// Custom variables of an object, split by kind.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Attributes {
    pub custom: Vec<(String, String)>,
    pub labels: Vec<(String, String)>,
    pub label_sources: Vec<(String, String)>,
    pub tags: Vec<(String, String)>,
}

fn hex_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    if bytes.len() % 2 != 0 {
        return s.to_string();
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char).to_digit(16);
        let lo = (pair[1] as char).to_digit(16);
        match (hi, lo) {
            (Some(h), Some(l)) => out.push((h * 16 + l) as u8),
            _ => return s.to_string(),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn split(vars: &[(String, String)]) -> Attributes {
    let mut a = Attributes::default();
    for (k, v) in vars {
        if let Some(name) = k.strip_prefix("_LABELSOURCE_") {
            a.label_sources.push((hex_decode(name), hex_decode(v)));
        } else if let Some(name) = k.strip_prefix("_LABEL_") {
            a.labels.push((hex_decode(name), hex_decode(v)));
        } else if let Some(name) = k.strip_prefix("_TAG_") {
            a.tags.push((hex_decode(name), hex_decode(v)));
        } else {
            a.custom.push((k.clone(), v.clone()));
        }
    }
    a
}

/// Only the plain custom variables (cheap path for the common case).
pub fn custom_only(vars: &[(String, String)]) -> impl Iterator<Item = &(String, String)> {
    vars.iter().filter(|(k, _)| !(k.starts_with("_LABEL_") || k.starts_with("_LABELSOURCE_") || k.starts_with("_TAG_")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_checkmk_labels() {
        let vars = vec![
            ("_LABEL_636D6B2F6F735F66616D696C79".to_string(), "6C696E7578".to_string()),
            ("_LABELSOURCE_636D6B2F6F735F66616D696C79".to_string(), "646973636F7665726564".to_string()),
            ("_TAG_73697465".to_string(), "636D6B".to_string()),
            ("FILENAME".to_string(), "/wato/hosts.mk".to_string()),
        ];
        let a = split(&vars);
        assert_eq!(a.labels, vec![("cmk/os_family".into(), "linux".into())]);
        assert_eq!(a.label_sources, vec![("cmk/os_family".into(), "discovered".into())]);
        assert_eq!(a.tags, vec![("site".into(), "cmk".into())]);
        assert_eq!(a.custom, vec![("FILENAME".into(), "/wato/hosts.mk".into())]);
        assert_eq!(custom_only(&vars).count(), 1);
    }
}
