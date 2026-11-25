//! Keep Steam's original document, including comments, ordering, and unrelated settings.
//! The VDF parser supplies borrowed keys; only the selected field is spliced back.
use anyhow::{Context, Result, bail, ensure};
use keyvalues_parser::Value;
use std::ops::Range;

pub(super) struct Field {
    pub value: Option<String>,
    span: Range<usize>,
    value_start: usize,
    insertion: Option<(usize, Vec<String>)>,
}

fn quoted(s: &str) -> String {
    format!(
        "\"{}\"",
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\r', "\\r")
            .replace('\n', "\\n")
            .replace('\t', "\\t")
    )
}

fn skip_space(s: &[u8], mut at: usize) -> usize {
    loop {
        while s.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        if s[at..].starts_with(b"//") {
            at += s[at..]
                .iter()
                .position(|b| *b == b'\n')
                .unwrap_or(s.len() - at);
        } else {
            return at;
        }
    }
}

fn token_end(s: &[u8], mut at: usize) -> Result<usize> {
    if s.get(at) != Some(&b'"') {
        while s
            .get(at)
            .is_some_and(|v| !v.is_ascii_whitespace() && !b"{}\0".contains(v))
        {
            at += 1;
        }
        return Ok(at);
    }
    at += 1;
    while let Some(&v) = s.get(at) {
        at += 1;
        if v == b'"' {
            return Ok(at);
        }
        if v == b'\\' {
            at += 1;
        }
    }
    bail!("unterminated Steam configuration string")
}

fn key_span(input: &str, key: &str) -> Result<Range<usize>> {
    let start = (key.as_ptr() as usize)
        .checked_sub(input.as_ptr() as usize)
        .context("Steam configuration key offset")?;
    let end = start + key.len();
    ensure!(end <= input.len(), "invalid Steam configuration key offset");
    if start != 0 && input.as_bytes()[start - 1] == b'"' && input.as_bytes().get(end) == Some(&b'"')
    {
        Ok(start - 1..end + 1)
    } else {
        Ok(start..end)
    }
}

pub(super) fn field(input: &str, id: u32) -> Result<Field> {
    let id = id.to_string();
    let path = ["Software", "Valve", "Steam", "apps", &id, "LaunchOptions"];
    let root = keyvalues_parser::parse(input.trim_start_matches('\u{feff}'))
        .context("read Steam localconfig.vdf")?
        .into_vdf();
    ensure!(
        root.key.eq_ignore_ascii_case("UserLocalConfigStore"),
        "unexpected Steam local configuration root"
    );
    let mut key = &root.key;
    let mut node = &root.value;
    for (index, part) in path.iter().enumerate() {
        let obj = node
            .get_obj()
            .context("invalid Steam configuration section")?;
        let mut keys = obj.iter().filter(|(k, _)| k.eq_ignore_ascii_case(part));
        let Some((next_key, values)) = keys.next() else {
            let open = skip_space(input.as_bytes(), key_span(input, key)?.end);
            ensure!(
                input.as_bytes().get(open) == Some(&b'{'),
                "invalid Steam configuration section"
            );
            return Ok(Field {
                value: None,
                span: open + 1..open + 1,
                value_start: open + 1,
                insertion: Some((index, path[index..].iter().map(|x| x.to_string()).collect())),
            });
        };
        ensure!(
            keys.next().is_none() && values.len() == 1,
            "ambiguous Steam configuration field {part}"
        );
        key = next_key;
        node = &values[0];
    }
    let Value::Str(value) = node else {
        bail!("Steam launch options must be text");
    };
    let start = key_span(input, key)?;
    let at = skip_space(input.as_bytes(), start.end);
    Ok(Field {
        value: Some(value.to_string()),
        span: start.start..token_end(input.as_bytes(), at)?,
        value_start: at,
        insertion: None,
    })
}

impl Field {
    pub fn replace(&self, input: &str, value: Option<&str>) -> String {
        if self.value.as_deref() == value {
            return input.to_owned();
        }
        let replacement = match (value, &self.insertion) {
            (None, _) => String::new(),
            (Some(value), None) => quoted(value),
            (Some(value), Some((level, path))) => {
                let eol = if input.contains("\r\n") { "\r\n" } else { "\n" };
                let mut out = String::new();
                for (i, key) in path.iter().enumerate() {
                    let indent = "\t".repeat(level + i + 1);
                    out.push_str(&format!("{eol}{indent}{}", quoted(key)));
                    if i + 1 == path.len() {
                        out.push_str(&format!("\t\t{}", quoted(value)));
                    } else {
                        out.push_str(&format!("{eol}{indent}{{"));
                    }
                }
                for i in (0..path.len() - 1).rev() {
                    out.push_str(&format!("{eol}{}}}", "\t".repeat(level + i + 1)));
                }
                out.push_str(eol);
                out
            }
        };
        let mut out = input.to_owned();
        let start = if value.is_some() {
            self.value_start
        } else {
            self.span.start
        };
        out.replace_range(start..self.span.end, &replacement);
        out
    }
}
