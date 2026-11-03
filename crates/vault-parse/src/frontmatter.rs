/// Split YAML frontmatter. Returns (yaml, body_offset).
pub fn split(src: &str) -> (Option<&str>, usize) {
    let rest = if let Some(r) = src.strip_prefix("---\n") {
        r
    } else if let Some(r) = src.strip_prefix("---\r\n") {
        r
    } else {
        return (None, 0);
    };
    let start = src.len() - rest.len();
    let mut pos = 0;
    for line in rest.split_inclusive('\n') {
        let t = line.trim_end_matches(['\n', '\r']);
        if t == "---" || t == "..." {
            return (Some(&rest[..pos]), start + pos + line.len());
        }
        pos += line.len();
    }
    (None, 0)
}

pub fn parse_yaml(yaml: &str) -> Option<serde_json::Value> {
    if yaml.trim().is_empty() {
        return Some(serde_json::Value::Object(Default::default()));
    }
    let v: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml).ok()?;
    serde_json::to_value(v).ok().filter(|v| v.is_object())
}

/// A frontmatter value that may be a list, or a comma/space separated string.
pub fn string_list(v: Option<&serde_json::Value>, split_spaces: bool) -> Vec<String> {
    match v {
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|x| match x {
                serde_json::Value::String(s) => Some(s.trim().to_string()),
                serde_json::Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
            .filter(|s| !s.is_empty())
            .collect(),
        Some(serde_json::Value::String(s)) => s
            .split(|c: char| c == ',' || (split_spaces && c.is_whitespace()))
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}
