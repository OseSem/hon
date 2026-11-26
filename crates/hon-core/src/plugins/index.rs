//! A repo's `index.json`: the sources and libs it offers, signed with the repo's minisign key.
//! The index holds every file's SHA-256, so the one signature covers every file.

use std::collections::HashSet;

use anyhow::{Context, anyhow, bail};
use minisign_verify::{PublicKey, Signature};
use serde::{Deserialize, Serialize};

use crate::plugins::runtime::display_text;
use crate::plugins::source::valid_id;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Index {
    pub name: String,
    pub api: u32,
    pub sources: Vec<IndexSource>,
    #[serde(default)]
    pub libs: Vec<IndexLib>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct IndexSource {
    pub id: String,
    pub name: String,
    pub version: u32,
    pub lang: String,
    pub nsfw: bool,
    pub api: u32,
    pub file: String,
    pub sha256: String,
    /// The `file` of each entry in [`Index::libs`] this source `require`s.
    #[serde(default)]
    pub libs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct IndexLib {
    pub file: String,
    pub sha256: String,
}

/// Checks `index_bytes` against the repo's signature and key, then checks every field hon
/// relies on. Names come back cleaned for the screen.
pub fn verify(index_bytes: &[u8], minisig: &str, pubkey_b64: &str) -> anyhow::Result<Index> {
    if minisig.trim().is_empty() {
        bail!("repo signature check failed: the repo has no index.json.minisig");
    }
    check_signature(index_bytes, minisig, pubkey_b64)
        .map_err(|e| anyhow!("repo signature check failed: {e}"))?;
    let mut index: Index =
        serde_json::from_slice(index_bytes).context("repo index.json is not valid")?;
    check_fields(&mut index)?;
    Ok(index)
}

fn check_signature(
    index_bytes: &[u8],
    minisig: &str,
    pubkey_b64: &str,
) -> Result<(), minisign_verify::Error> {
    let key = PublicKey::from_base64(pubkey_b64.trim())?;
    let signature = Signature::decode(minisig)?;
    key.verify(index_bytes, &signature, false)
}

/// The base64 key line of a minisign `key.pub` (the line after the untrusted comment).
pub fn pubkey_from_file(key_pub: &str) -> anyhow::Result<String> {
    let line = key_pub
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("untrusted comment:"))
        .context("key.pub has no key line")?;
    PublicKey::from_base64(line)
        .map_err(|e| anyhow!("key.pub is not a minisign public key: {e}"))?;
    Ok(line.to_string())
}

fn check_fields(index: &mut Index) -> anyhow::Result<()> {
    index.name = display_text(&index.name);
    if index.name.is_empty() {
        bail!("repo index field `name` must not be empty");
    }

    let mut lib_files = HashSet::new();
    for (i, lib) in index.libs.iter().enumerate() {
        if lib_name(&lib.file).is_none() {
            bail!(
                "repo index field `libs[{i}].file` must be `lib/<name>.lua`, with <name> made of \
                 lowercase letters, digits and `_`"
            );
        }
        check_sha256(&lib.sha256, || format!("libs[{i}].sha256"))?;
        if !lib_files.insert(lib.file.as_str()) {
            bail!("repo index field `libs[{i}].file` repeats an earlier lib");
        }
    }

    let mut ids = HashSet::new();
    for (i, source) in index.sources.iter_mut().enumerate() {
        if !valid_id(&source.id) {
            bail!(
                "repo index field `sources[{i}].id` must be 1 to 64 lowercase letters, digits, \
                 `-` or `_`, starting with a letter or digit"
            );
        }
        if !ids.insert(source.id.clone()) {
            bail!(
                "repo index field `sources[{i}].id` repeats the id `{}`",
                source.id
            );
        }
        source.name = display_text(&source.name);
        if source.name.is_empty() {
            bail!("repo index field `sources[{i}].name` must not be empty");
        }
        if source.lang.chars().any(char::is_control) {
            bail!("repo index field `sources[{i}].lang` must not contain control characters");
        }
        if !valid_source_file(&source.file) {
            bail!(
                "repo index field `sources[{i}].file` must be a relative path under `sources/` \
                 ending in `.lua`, with no `..`"
            );
        }
        check_sha256(&source.sha256, || format!("sources[{i}].sha256"))?;
        for (j, lib) in source.libs.iter().enumerate() {
            if !lib_files.contains(lib.as_str()) {
                bail!(
                    "repo index field `sources[{i}].libs[{j}]` names a lib that is not in `libs`"
                );
            }
        }
    }
    Ok(())
}

fn check_sha256(value: &str, field: impl FnOnce() -> String) -> anyhow::Result<()> {
    let hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
    if value.len() != 64 || !value.bytes().all(hex) {
        bail!(
            "repo index field `{}` must be 64 lowercase hex characters",
            field()
        );
    }
    Ok(())
}

/// `<name>` of `lib/<name>.lua` when [`valid_lib_name`] accepts it, so `require("<name>")`
/// finds it.
pub(crate) fn lib_name(file: &str) -> Option<&str> {
    file.strip_prefix("lib/")
        .and_then(|f| f.strip_suffix(".lua"))
        .filter(|name| valid_lib_name(name))
}

/// `^[a-z0-9_]+$`: a name `require` accepts, which is also a safe file name.
pub fn valid_lib_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn valid_source_file(file: &str) -> bool {
    file.strip_prefix("sources/")
        .and_then(|f| f.strip_suffix(".lua"))
        .is_some_and(|stem| !stem.is_empty())
        && safe_rel_path(file)
}

/// A relative `/`-separated path that stays inside the repo, whether read from disk or joined
/// onto a URL: no leading `/`, no empty, `.` or `..` segments, and none of `\ : % ? #` or
/// control characters (each can change what a URL join or a Windows path points at).
pub(crate) fn safe_rel_path(path: &str) -> bool {
    !path.is_empty()
        && !path
            .chars()
            .any(|c| c.is_control() || matches!(c, '\\' | ':' | '%' | '?' | '#'))
        && path
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use minisign::KeyPair;
    use serde_json::{Value, json};

    use super::*;

    fn sign(kp: &KeyPair, bytes: &[u8]) -> String {
        minisign::sign(Some(&kp.pk), &kp.sk, Cursor::new(bytes), None, None)
            .unwrap()
            .to_string()
    }

    fn sample() -> Value {
        json!({
            "name": "Example sources",
            "api": 1,
            "sources": [{
                "id": "delta",
                "name": "Delta",
                "version": 3,
                "lang": "en",
                "nsfw": false,
                "api": 1,
                "file": "sources/delta.lua",
                "sha256": "a".repeat(64),
                "libs": ["lib/madara.lua"],
            }],
            "libs": [{ "file": "lib/madara.lua", "sha256": "b".repeat(64) }],
        })
    }

    /// Signs `index` with a fresh key and runs `verify` on it.
    fn verify_signed(index: &Value) -> anyhow::Result<Index> {
        let kp = KeyPair::generate_unencrypted_keypair().unwrap();
        let bytes = serde_json::to_vec(index).unwrap();
        verify(&bytes, &sign(&kp, &bytes), &kp.pk.to_base64())
    }

    #[test]
    fn verify_accepts_good_signature() {
        let index = verify_signed(&sample()).unwrap();
        assert_eq!(index.name, "Example sources");
        assert_eq!(index.api, 1);
        assert_eq!(index.sources.len(), 1);
        let s = &index.sources[0];
        assert_eq!(
            (s.id.as_str(), s.name.as_str(), s.version, s.lang.as_str()),
            ("delta", "Delta", 3, "en")
        );
        assert_eq!(s.file, "sources/delta.lua");
        assert_eq!(s.libs, ["lib/madara.lua"]);
        assert_eq!(index.libs[0].file, "lib/madara.lua");
    }

    #[test]
    fn verify_rejects_tampered_index_and_wrong_key() {
        let kp = KeyPair::generate_unencrypted_keypair().unwrap();
        let bytes = serde_json::to_vec(&sample()).unwrap();
        let sig = sign(&kp, &bytes);

        let mut tampered = bytes.clone();
        let at = tampered.iter().position(|&b| b == b'D').unwrap();
        tampered[at] = b'X';
        let err = verify(&tampered, &sig, &kp.pk.to_base64()).unwrap_err();
        assert!(
            err.to_string().starts_with("repo signature check failed: "),
            "{err}"
        );

        let other = KeyPair::generate_unencrypted_keypair().unwrap();
        let err = verify(&bytes, &sig, &other.pk.to_base64()).unwrap_err();
        assert!(
            err.to_string().starts_with("repo signature check failed: "),
            "{err}"
        );

        let err = verify(&bytes, "", &kp.pk.to_base64()).unwrap_err();
        assert!(
            err.to_string().starts_with("repo signature check failed: "),
            "{err}"
        );
    }

    #[test]
    fn verify_rejects_escaping_paths() {
        for bad in ["../x.lua", "/etc/x", "sources/../../x.lua"] {
            let mut index = sample();
            index["sources"][0]["file"] = json!(bad);
            let err = verify_signed(&index).unwrap_err().to_string();
            assert!(err.contains("`sources[0].file`"), "{bad}: {err}");
        }
    }

    /// The field an error must name, and how to break the sample index there.
    type Case = (&'static str, fn(&mut Value));

    #[test]
    fn verify_checks_index_fields() {
        let cases: Vec<Case> = vec![
            ("sources[0].id", |i| i["sources"][0]["id"] = json!("Bad Id")),
            ("sources[0].id", |i| i["sources"][0]["id"] = json!("")),
            ("sources[1].id", |i| {
                let dup = i["sources"][0].clone();
                i["sources"].as_array_mut().unwrap().push(dup);
            }),
            ("sources[0].lang", |i| {
                i["sources"][0]["lang"] = json!("e\u{1b}n")
            }),
            ("sources[0].sha256", |i| {
                i["sources"][0]["sha256"] = json!("A".repeat(64))
            }),
            ("sources[0].sha256", |i| {
                i["sources"][0]["sha256"] = json!("abc")
            }),
            ("libs[0].sha256", |i| {
                i["libs"][0]["sha256"] = json!("g".repeat(64))
            }),
            ("sources[0].libs[0]", |i| {
                i["sources"][0]["libs"] = json!(["lib/other.lua"])
            }),
            ("libs[0].file", |i| {
                i["libs"][0]["file"] = json!("lib/Madara.lua");
                i["sources"][0]["libs"] = json!(["lib/Madara.lua"]);
            }),
            ("libs[0].file", |i| {
                i["libs"][0]["file"] = json!("lib/sub/madara.lua");
                i["sources"][0]["libs"] = json!(["lib/sub/madara.lua"]);
            }),
            ("libs[0].file", |i| {
                i["libs"][0]["file"] = json!("sources/madara.lua");
                i["sources"][0]["libs"] = json!(["sources/madara.lua"]);
            }),
            ("sources[0].file", |i| {
                i["sources"][0]["file"] = json!("lib/delta.lua")
            }),
            ("sources[0].file", |i| {
                i["sources"][0]["file"] = json!("sources/delta.txt")
            }),
            ("sources[0].file", |i| {
                i["sources"][0]["file"] = json!("sources/%2e%2e/delta.lua")
            }),
            ("sources[0].name", |i| {
                i["sources"][0]["name"] = json!("\n\t")
            }),
            ("name", |i| i["name"] = json!(" \u{7} ")),
            ("libs[1].file", |i| {
                let dup = i["libs"][0].clone();
                i["libs"].as_array_mut().unwrap().push(dup);
            }),
            ("sources[0].file", |i| {
                i["sources"][0]["file"] = json!("sources/.lua")
            }),
        ];
        for (field, mutate) in cases {
            let mut index = sample();
            mutate(&mut index);
            let err = verify_signed(&index).unwrap_err().to_string();
            assert!(err.contains(&format!("`{field}`")), "{field}: {err}");
        }
    }

    #[test]
    fn verify_rejects_signed_non_json() {
        let kp = KeyPair::generate_unencrypted_keypair().unwrap();
        let bytes = b"not json at all";
        let err = verify(bytes, &sign(&kp, bytes), &kp.pk.to_base64())
            .unwrap_err()
            .to_string();
        assert!(err.contains("index.json is not valid"), "{err}");
    }

    #[test]
    fn verify_cleans_names_and_accepts_nested_source_paths() {
        let mut index = sample();
        index["name"] = json!("My\u{1b}[31m repo");
        index["sources"][0]["name"] = json!("Kat\u{7}ana\n");
        index["sources"][0]["file"] = json!("sources/en/delta.lua");
        let index = verify_signed(&index).unwrap();
        assert_eq!(index.name, "My [31m repo");
        assert_eq!(index.sources[0].name, "Kat ana");
    }

    #[test]
    fn pubkey_from_file_returns_key_line() {
        let kp = KeyPair::generate_unencrypted_keypair().unwrap();
        let file = kp.pk.to_box().unwrap().to_string();
        assert!(file.starts_with("untrusted comment:"), "{file}");
        assert_eq!(pubkey_from_file(&file).unwrap(), kp.pk.to_base64());
        assert!(pubkey_from_file("untrusted comment: nothing\n").is_err());
        assert!(pubkey_from_file("untrusted comment: x\nnot-base64!\n").is_err());
    }
}
