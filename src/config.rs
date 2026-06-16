//! Loading and merging of the declarative TOML connection map.
//!
//!   * a top-level `include = ["a.toml", ...]` array pulls in other files,
//!     resolved relative to the including file, merged *first* so the including
//!     file's own keys win on scalar conflicts;
//!   * same-key arrays are concatenated with order-preserving dedup (so a port
//!     listed in several files complements rather than overrides);
//!   * include cycles on the current path are rejected, but diamonds are fine.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use toml::value::{Table, Value};

const INCLUDE_KEY: &str = "include";

/// Load a config file, resolving any `include` directives recursively.
pub fn load_config(path: &Path) -> Result<Table, String> {
    load_file(path, &HashSet::new())
}

fn load_file(path: &Path, chain: &HashSet<PathBuf>) -> Result<Table, String> {
    let abs = std::fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))?;

    if chain.contains(&abs) {
        return Err(format!("circular include detected at {}", abs.display()));
    }
    // A fresh set per branch: cycles on the current path are blocked, while
    // sibling/diamond includes are still allowed.
    let mut next_chain = chain.clone();
    next_chain.insert(abs.clone());

    let text = std::fs::read_to_string(&abs).map_err(|e| format!("{}: {e}", abs.display()))?;
    let mut raw: Table = toml::from_str(&text).map_err(|e| format!("{}: {e}", abs.display()))?;

    let includes = raw.remove(INCLUDE_KEY);
    let mut merged = Table::new();

    if let Some(inc_val) = includes {
        let arr = match inc_val {
            Value::Array(a) => a,
            _ => {
                return Err(format!(
                    "`{INCLUDE_KEY}` in {} must be an array",
                    abs.display()
                ))
            }
        };
        let base_dir = abs.parent().unwrap_or_else(|| Path::new("."));
        for inc in arr {
            let inc_str = inc.as_str().ok_or_else(|| {
                format!("`{INCLUDE_KEY}` in {} must be path strings", abs.display())
            })?;
            let inc_path = {
                let p = Path::new(inc_str);
                if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    base_dir.join(p)
                }
            };
            let child = load_file(&inc_path, &next_chain)?;
            merge_table(&mut merged, child);
        }
    }

    merge_table(&mut merged, raw);
    Ok(merged)
}

/// Merge `new` into `base` in place: recurse into tables, concat-dedup arrays,
/// scalar/type-mismatch replaces.
fn merge_table(base: &mut Table, new: Table) {
    for (key, val) in new {
        match base.get_mut(&key) {
            None => {
                base.insert(key, val);
            }
            Some(Value::Table(bt)) if val.is_table() => {
                if let Value::Table(nt) = val {
                    merge_table(bt, nt);
                }
            }
            Some(Value::Array(ba)) if val.is_array() => {
                if let Value::Array(na) = val {
                    for item in na {
                        if !ba.contains(&item) {
                            ba.push(item);
                        }
                    }
                }
            }
            Some(slot) => {
                *slot = val;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Pull a client/key edge list out of a loaded config, for assertions.
    fn edges(t: &Table, client: &str, key: &str) -> Vec<String> {
        t.get(client)
            .unwrap_or_else(|| panic!("missing client '{client}'"))
            .as_table()
            .unwrap_or_else(|| panic!("'{client}' is not a table"))
            .get(key)
            .unwrap_or_else(|| panic!("missing key '{client}:{key}'"))
            .as_array()
            .unwrap_or_else(|| panic!("'{client}:{key}' is not an array"))
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    }

    fn write(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn loads_a_single_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            dir.path(),
            "c.toml",
            r#"
            [synth]
            out_left = ["mixer:in_1", "rec:in_1"]
        "#,
        );
        let cfg = load_config(&p).unwrap();
        assert_eq!(edges(&cfg, "synth", "out_left"), ["mixer:in_1", "rec:in_1"]);
    }

    #[test]
    fn include_pulls_in_child_clients() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "child.toml",
            r#"
            [drums]
            out = ["mixer:in_9"]
        "#,
        );
        let base = write(
            dir.path(),
            "base.toml",
            r#"
            include = ["child.toml"]
            [synth]
            out = ["mixer:in_1"]
        "#,
        );
        let cfg = load_config(&base).unwrap();
        assert_eq!(edges(&cfg, "synth", "out"), ["mixer:in_1"]);
        assert_eq!(edges(&cfg, "drums", "out"), ["mixer:in_9"]);
        // The `include` directive itself must not survive as a client.
        assert!(cfg.get("include").is_none());
    }

    #[test]
    fn include_merges_keys_within_same_client() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "child.toml",
            r#"
            [synth]
            aux = ["fx:in_1"]
        "#,
        );
        let base = write(
            dir.path(),
            "base.toml",
            r#"
            include = ["child.toml"]
            [synth]
            main = ["mixer:in_1"]
        "#,
        );
        let cfg = load_config(&base).unwrap();
        assert_eq!(edges(&cfg, "synth", "aux"), ["fx:in_1"]);
        assert_eq!(edges(&cfg, "synth", "main"), ["mixer:in_1"]);
    }

    #[test]
    fn include_concats_arrays_with_dedup_includes_first() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "child.toml",
            r#"
            [synth]
            out = ["b", "c"]
        "#,
        );
        let base = write(
            dir.path(),
            "base.toml",
            r#"
            include = ["child.toml"]
            [synth]
            out = ["a", "b"]
        "#,
        );
        let cfg = load_config(&base).unwrap();
        // Included file is the base; the including file's items are appended,
        // with duplicates ("b") dropped.
        assert_eq!(edges(&cfg, "synth", "out"), ["b", "c", "a"]);
    }

    #[test]
    fn include_path_is_relative_to_including_file() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "sub/child.toml",
            r#"
            [drums]
            out = ["mixer:in_9"]
        "#,
        );
        let base = write(
            dir.path(),
            "base.toml",
            r#"
            include = ["sub/child.toml"]
        "#,
        );
        let cfg = load_config(&base).unwrap();
        assert_eq!(edges(&cfg, "drums", "out"), ["mixer:in_9"]);
    }

    #[test]
    fn circular_include_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.toml", "include = [\"b.toml\"]\n");
        write(dir.path(), "b.toml", "include = [\"a.toml\"]\n");
        let err = load_config(&dir.path().join("a.toml")).unwrap_err();
        assert!(err.contains("circular"), "unexpected error: {err}");
    }

    #[test]
    fn missing_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_config(&dir.path().join("nope.toml")).is_err());
    }

    #[test]
    fn missing_include_target_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let base = write(dir.path(), "base.toml", "include = [\"ghost.toml\"]\n");
        assert!(load_config(&base).is_err());
    }

    #[test]
    fn non_array_include_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let base = write(dir.path(), "base.toml", "include = \"child.toml\"\n");
        assert!(load_config(&base).is_err());
    }
}
