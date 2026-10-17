use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use crate::errors::Result;

pub mod cache;
pub mod ffmpeg;
pub mod fs;
pub mod parallel;
pub mod path;

pub fn parse_sync_list(source_dir: &Path, path: &Path) -> Result<HashSet<PathBuf>> {
    let contents = std::fs::read_to_string(path)?;

    let splits = contents
        .split(&['\n', '\r'])
        .map(str::trim)
        .filter(|x| !x.is_empty() && !x.starts_with('#'))
        .map(|x| source_dir.join(x))
        .collect::<HashSet<_>>();

    Ok(splits)
}

#[cfg(test)]
mod tests {
    use super::parse_sync_list;
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn unique_temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("tsync-test-{name}-{nanos}"))
    }

    #[test]
    fn parse_sync_list_ignores_comments_and_blank_lines() {
        let source = unique_temp_path("source");
        let sync_list_file = unique_temp_path("sync-list.txt");

        fs::create_dir_all(&source).expect("source dir should be created");
        fs::write(
            &sync_list_file,
            "# comment\nAlbum A\r\n\nAlbum B\n   # another comment\nAlbum C\n",
        )
        .expect("sync list should be written");

        let parsed = parse_sync_list(&source, &sync_list_file).expect("sync list should parse");

        assert_eq!(parsed.len(), 3);
        assert!(parsed.contains(&source.join("Album A")));
        assert!(parsed.contains(&source.join("Album B")));
        assert!(parsed.contains(&source.join("Album C")));

        let _ = fs::remove_file(sync_list_file);
        let _ = fs::remove_dir_all(source);
    }
}
