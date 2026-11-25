//! Lossless edits to the binary KeyValues file used for non-Steam shortcuts.
use anyhow::{Context, Result, bail, ensure};
use std::{collections::BTreeSet, ops::Range};

pub const EMPTY: &[u8] = b"\0shortcuts\0\x08\x08";
#[derive(Debug)]
pub struct Node<'a> {
    key: &'a [u8],
    kind: u8,
    start: usize,
    value: Range<usize>,
    children: Vec<Node<'a>>,
}

fn zero(input: &[u8], at: usize) -> Result<usize> {
    Ok(at
        + input
            .get(at..)
            .context("invalid shortcut offset")?
            .iter()
            .position(|b| *b == 0)
            .context("unterminated shortcut field")?)
}
fn children<'a>(input: &'a [u8], at: &mut usize, depth: u8) -> Result<Vec<Node<'a>>> {
    ensure!(depth < 32, "Steam shortcut nesting is too deep");
    let mut nodes = Vec::new();
    loop {
        let start = *at;
        let kind = *input.get(*at).context("truncated Steam shortcuts")?;
        *at += 1;
        if kind == 8 {
            return Ok(nodes);
        }
        let end = zero(input, *at)?;
        let key = &input[*at..end];
        *at = end + 1;
        let body = *at;
        let mut nested = Vec::new();
        match kind {
            0 => nested = children(input, at, depth + 1)?,
            1 => *at = zero(input, *at)? + 1,
            2 | 3 | 4 | 6 => *at += 4,
            7 | 10 => *at += 8,
            _ => bail!(
                "unsupported Steam shortcut field type {kind}; the shortcuts file was left untouched"
            ),
        }
        ensure!(*at <= input.len(), "truncated Steam shortcut field");
        nodes.push(Node {
            key,
            kind,
            start,
            value: body..*at,
            children: nested,
        });
    }
}

impl<'a> Node<'a> {
    fn named(&self, key: &str) -> Result<Option<&Node<'a>>> {
        let mut matches = self
            .children
            .iter()
            .filter(|n| n.key.eq_ignore_ascii_case(key.as_bytes()));
        let first = matches.next();
        ensure!(
            matches.next().is_none(),
            "duplicate {key} in Steam shortcut"
        );
        Ok(first)
    }
    pub fn string<'b>(&self, input: &'b [u8], key: &str) -> Result<Option<&'b str>> {
        self.named(key)?
            .map(|node| {
                ensure!(node.kind == 1, "invalid {key} value in Steam shortcut");
                std::str::from_utf8(&input[node.value.start..node.value.end - 1])
                    .with_context(|| format!("non-Unicode Steam shortcut {key}"))
            })
            .transpose()
    }
    pub fn has_customizations(&self, input: &[u8]) -> Result<bool> {
        for field in &self.children {
            let key = std::str::from_utf8(field.key)?.to_ascii_lowercase();
            let customized = match key.as_str() {
                "appid" | "appname" | "exe" | "startdir" | "launchoptions" | "lastplaytime" => {
                    false
                }
                "allowdesktopconfig" | "allowoverlay" => {
                    field.kind != 2 || input[field.value.clone()] != 1u32.to_le_bytes()
                }
                "ishidden" | "openvr" | "devkit" | "devkitoverrideappid" => {
                    field.kind != 2 || input[field.value.clone()] != 0u32.to_le_bytes()
                }
                "icon" | "shortcutpath" | "devkitgameid" | "flatpakappid" => {
                    field.kind != 1 || field.value.len() != 1
                }
                "tags" => field.kind != 0 || !field.children.is_empty(),
                _ => true,
            };
            if customized {
                return Ok(true);
            }
        }
        Ok(false)
    }
    pub fn id(&self, input: &[u8]) -> Result<u32> {
        let node = self
            .named("appid")?
            .context("Steam shortcut missing app ID")?;
        ensure!(node.kind == 2, "invalid Steam shortcut app ID");
        Ok(u32::from_le_bytes(input[node.value.clone()].try_into()?))
    }
    pub fn edit(&self, input: &[u8], fields: &[(&str, Option<&str>)]) -> Result<Vec<u8>> {
        let mut patches = Vec::new();
        for (key, value) in fields {
            if self.string(input, key)? == *value {
                continue;
            }
            let old = self.named(key)?;
            let (span, bytes) = match (old, value) {
                (Some(n), Some(s)) => (n.value.clone(), cstring(s)?),
                (Some(n), None) => (n.start..n.value.end, Vec::new()),
                (None, Some(s)) => {
                    let mut record = Vec::new();
                    string(&mut record, key, s)?;
                    let pos = self.value.end - 1;
                    (pos..pos, record)
                }
                (None, None) => continue,
            };
            patches.push((span, bytes));
        }
        patches.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
        let mut bytes = input.to_vec();
        for (span, patch) in patches {
            bytes.splice(span, patch);
        }
        Ok(bytes)
    }
}
pub struct Shortcuts<'a> {
    pub rows: Vec<Node<'a>>,
    end: usize,
}
impl<'a> Shortcuts<'a> {
    pub fn parse(input: &'a [u8]) -> Result<Self> {
        let mut at = 0;
        let mut roots = children(input, &mut at, 0)?;
        ensure!(
            at == input.len() && roots.len() == 1,
            "invalid Steam shortcuts document"
        );
        let root = roots.pop().unwrap();
        ensure!(
            root.kind == 0 && root.key.eq_ignore_ascii_case(b"shortcuts"),
            "unexpected Steam shortcut root"
        );
        let mut indices = BTreeSet::new();
        let mut ids = BTreeSet::new();
        for row in &root.children {
            let index = std::str::from_utf8(row.key)?
                .parse::<u32>()
                .context("unexpected Steam shortcut index")?;
            ensure!(
                row.kind == 0 && indices.insert(index) && ids.insert(row.id(input)?),
                "ambiguous Steam shortcut ID or index"
            );
        }
        Ok(Self {
            rows: root.children,
            end: root.value.end - 1,
        })
    }
    pub fn by_id(&self, input: &[u8], id: u32) -> Option<&Node<'a>> {
        self.rows.iter().find(|r| r.id(input).ok() == Some(id))
    }
    pub fn add(
        &self,
        input: &[u8],
        id: u32,
        name: &str,
        target: &str,
        options: &str,
        directory: &str,
    ) -> Result<Vec<u8>> {
        ensure!(
            self.by_id(input, id).is_none(),
            "Steam shortcut ID already in use"
        );
        let next = self
            .rows
            .iter()
            .map(|n| std::str::from_utf8(n.key).unwrap().parse::<u32>().unwrap())
            .max()
            .map_or(Some(0), |max| max.checked_add(1))
            .context("too many Steam shortcuts")?;
        let mut item = vec![0];
        item.extend(cstring(&next.to_string())?);
        integer(&mut item, "appid", id);
        for (k, v) in [
            ("AppName", name),
            ("Exe", target),
            ("StartDir", directory),
            ("icon", ""),
            ("ShortcutPath", ""),
            ("LaunchOptions", options),
        ] {
            string(&mut item, k, v)?;
        }
        for (k, v) in [
            ("IsHidden", 0),
            ("AllowDesktopConfig", 1),
            ("AllowOverlay", 1),
            ("OpenVR", 0),
            ("Devkit", 0),
            ("LastPlayTime", 0),
        ] {
            integer(&mut item, k, v);
        }
        item.extend_from_slice(b"\0tags\0\x08\x08");
        let mut bytes = input.to_vec();
        bytes.splice(self.end..self.end, item);
        Ok(bytes)
    }
    pub fn remove(&self, input: &[u8], node: &Node<'a>) -> Vec<u8> {
        // Steam's array indices are positional. Its saved app IDs, including
        // those used by artwork and Proton, never depend on these indices.
        let mut result = Vec::with_capacity(input.len());
        let mut at = 0;
        let mut index = 0;
        for row in &self.rows {
            result.extend_from_slice(&input[at..row.start]);
            if row.start != node.start {
                result.push(0);
                result.extend_from_slice(index.to_string().as_bytes());
                result.push(0);
                result.extend_from_slice(&input[row.value.clone()]);
                index += 1;
            }
            at = row.value.end;
        }
        result.extend_from_slice(&input[at..]);
        result
    }
}
fn cstring(value: &str) -> Result<Vec<u8>> {
    ensure!(
        !value.contains('\0'),
        "Steam fields cannot contain a null byte"
    );
    let mut bytes = value.as_bytes().to_vec();
    bytes.push(0);
    Ok(bytes)
}
fn string(out: &mut Vec<u8>, key: &str, value: &str) -> Result<()> {
    out.push(1);
    out.extend(cstring(key)?);
    out.extend(cstring(value)?);
    Ok(())
}
fn integer(out: &mut Vec<u8>, key: &str, value: u32) {
    out.push(2);
    out.extend(key.as_bytes());
    out.push(0);
    out.extend(value.to_le_bytes());
}
