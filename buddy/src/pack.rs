//! Bounded transport frames for small objects. Individual chunk identities stay unchanged.
use crate::model::{MAX_CHUNK, digest, hash_ok};
use anyhow::{Context, Result, ensure};
pub const LIMIT: usize = 8 * 1024 * 1024;
pub const COUNT: usize = 256;
const HEADER: usize = 68;
pub fn append(out: &mut Vec<u8>, hash: &str, data: &[u8]) -> Result<()> {
    ensure!(
        hash_ok(hash)
            && !data.is_empty()
            && data.len() <= MAX_CHUNK
            && out.len() + HEADER + data.len() <= LIMIT,
        "invalid chunk pack"
    );
    out.extend_from_slice(hash.as_bytes());
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(data);
    Ok(())
}
pub fn decode(mut input: &[u8]) -> Result<Vec<(&str, &[u8])>> {
    ensure!(
        !input.is_empty() && input.len() <= LIMIT,
        "invalid pack size"
    );
    let mut entries = vec![];
    while !input.is_empty() {
        ensure!(
            entries.len() < COUNT && input.len() >= HEADER,
            "invalid pack header"
        );
        let hash = std::str::from_utf8(&input[..64])?;
        let len = u32::from_be_bytes(input[64..HEADER].try_into()?) as usize;
        ensure!(len > 0 && len <= MAX_CHUNK, "invalid packed chunk length");
        let data = input.get(HEADER..HEADER + len).context("truncated pack")?;
        ensure!(digest(data) == hash, "packed chunk digest mismatch");
        entries.push((hash, data));
        input = &input[HEADER + len..];
    }
    Ok(entries)
}
pub fn groups(chunks: &[(String, u32)]) -> Vec<&[(String, u32)]> {
    let mut groups = vec![];
    let (mut start, mut bytes) = (0, 0);
    for (i, (_, len)) in chunks.iter().enumerate() {
        let size = *len as usize + HEADER;
        if i - start == COUNT || bytes + size > LIMIT {
            groups.push(&chunks[start..i]);
            start = i;
            bytes = 0;
        }
        bytes += size;
    }
    if start < chunks.len() {
        groups.push(&chunks[start..]);
    }
    groups
}
