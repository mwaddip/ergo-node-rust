use std::path::Path;

pub fn rewrite_storage_db(config_path: &Path, new_db: &str) -> anyhow::Result<()> {
    let original = std::fs::read_to_string(config_path).map_err(|e| {
        anyhow::anyhow!(
            "config file missing or unreadable at {}: {e}",
            config_path.display()
        )
    })?;
    let mut doc: toml_edit::DocumentMut = original.parse()?;

    let storage = doc
        .get_mut("storage")
        .and_then(|s| s.as_table_mut())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "[storage] section not found in {}",
                config_path.display()
            )
        })?;

    let prev = storage
        .get("db")
        .and_then(|v| v.as_str())
        .unwrap_or("(unset)")
        .to_string();

    storage["db"] = toml_edit::value(new_db);

    // Set the comment on the key's prefix so it appears on its own line above
    // `db = "new"`. The existing parsed key prefix is typically "\n", so we
    // replace it with "\n# previous: {prev}\n" which produces:
    //
    //   # previous: old-value
    //   db = "new-value"
    if let Some(mut key_mut) = storage.key_mut("db") {
        key_mut
            .leaf_decor_mut()
            .set_prefix(format!("\n# previous: {prev}\n"));
    }

    std::fs::write(config_path, doc.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn rewrite_replaces_storage_db_and_preserves_others() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("indexer.toml");
        fs::write(
            &path,
            r#"
[node]
url = "http://127.0.0.1:9052"

[storage]
db = "sqlite:///old/path/index.db"

[api]
bind = "0.0.0.0:9054"
"#,
        )
        .unwrap();

        rewrite_storage_db(&path, "postgres://user@host:5432/db").unwrap();
        let s = fs::read_to_string(&path).unwrap();
        assert!(s.contains("db = \"postgres://user@host:5432/db\""));
        assert!(s.contains("[node]"));
        assert!(s.contains("[api]"));
        // Previous value preserved as a comment immediately above
        assert!(
            s.contains("# previous: sqlite:///old/path/index.db")
                || s.contains("# previous: \"sqlite:///old/path/index.db\"")
        );
    }

    #[test]
    fn rewrite_returns_warning_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.toml");
        let err = rewrite_storage_db(&path, "postgres://x@h/d").unwrap_err();
        assert!(
            err.to_string().contains("missing") || err.to_string().contains("not found"),
            "unexpected error: {err}"
        );
    }
}
