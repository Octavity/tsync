use std::{
    collections::HashSet,
    io::BufRead,
    path::{Path, PathBuf},
    process::Command,
};

use clap::ValueEnum;

use crate::errors::{Error, Result};

struct BackendADB;
struct BackendNone;

trait FSEmu {
    fn available() -> Result<bool>;
    fn build_file_list(source: &Path) -> Result<HashSet<PathBuf>>;
    fn cp(source: &Path, target: &Path) -> Result<()>;
}

#[derive(Debug, Clone, ValueEnum)]
pub enum FSBackend {
    /// Useful for android devices connected over tcpip or usb, and is recommended for all android-targeted syncs.
    Adb,

    /// Reserved for future FTP backend support.
    #[value(hide = true)]
    Ftp,

    /// Not recommended for syncing between devices, but can be useful for moving files around on the same device.
    None,
}

impl FSBackend {
    pub fn available(&self) -> Result<bool> {
        match self {
            FSBackend::Adb => BackendADB::available(),
            FSBackend::Ftp => Err(Error::descriptive("FTP backend is not implemented yet")),
            FSBackend::None => BackendNone::available(),
        }
    }

    pub fn build_file_list(&self, source: &Path) -> Result<HashSet<PathBuf>> {
        match self {
            FSBackend::Adb => BackendADB::build_file_list(source),
            FSBackend::Ftp => Err(Error::descriptive("FTP backend is not implemented yet")),
            FSBackend::None => BackendNone::build_file_list(source),
        }
    }

    pub fn cp(&self, source: &Path, target: &Path) -> Result<()> {
        match self {
            FSBackend::Adb => BackendADB::cp(source, target),
            FSBackend::Ftp => Err(Error::descriptive("FTP backend is not implemented yet")),
            FSBackend::None => BackendNone::cp(source, target),
        }
    }
}

impl FSEmu for BackendNone {
    #[inline]
    fn available() -> Result<bool> {
        Ok(true)
    }

    fn build_file_list(source: &Path) -> Result<HashSet<PathBuf>> {
        let mut files = HashSet::new();
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                let sub_files = Self::build_file_list(&path)?;
                files.extend(sub_files);
            } else {
                files.insert(path);
            }
        }

        Ok(files)
    }

    fn cp(source: &Path, target: &Path) -> Result<()> {
        use std::{fs, io::ErrorKind};

        if let Err(e) = fs::copy(source, target) {
            if e.kind() == ErrorKind::NotFound {
                let parent = target
                    .parent()
                    .ok_or_else(|| Error::descriptive("Target has no parent directory"))?;

                fs::create_dir_all(parent)?;
                fs::copy(source, target)?;
            } else {
                return Err(e.into());
            }
        }

        Ok(())
    }
}

impl FSEmu for BackendADB {
    fn available() -> Result<bool> {
        let adb = Command::new("adb").args(["devices", "-l"]).output()?;

        if !adb.status.success() {
            return Err(Error::descriptive("adb failed to run"));
        }

        let devices = adb
            .stdout
            .lines()
            .skip(1)
            .filter(|x| x.as_ref().is_ok_and(|z| !z.is_empty()));

        if devices.count() < 1 {
            return Err(Error::descriptive("adb returned no devices"));
        }

        // TODO:
        // Check if at least one device is online.
        // Allow the user to target which device to use by transport id.

        Ok(true)
    }

    fn build_file_list(source: &Path) -> Result<HashSet<PathBuf>> {
        let mut files = HashSet::new();
        let path_str = source.to_string_lossy().replace('\\', "/");
        let output = Command::new("adb")
            .arg("shell")
            .arg("find")
            .arg(&path_str)
            .arg("-type")
            .arg("f")
            .output()?;

        if !output.status.success() {
            let message = format!(
                "adb find failed with code {}: {}",
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stderr)
            );
            return Err(Error::descriptive(message));
        }

        for line in output.stdout.lines() {
            let line = line?;
            files.insert(PathBuf::from(line));
        }

        Ok(files)
    }

    fn cp(source: &Path, target: &Path) -> Result<()> {
        let source = source.to_string_lossy().replace('\\', "/");
        let target = target.to_string_lossy().replace('\\', "/");

        let mut cmd = Command::new("adb");
        cmd.arg("push").arg(source).arg(target);

        let output = cmd.output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let message = format!(
                "adb exited with code {} detailing {}",
                output.status.code().unwrap_or(-1),
                stderr
            );
            return Err(Error::descriptive(message));
        }

        Ok(())
    }
}

/// Returns true when `ext` matches any of the expected extensions.
///
/// Extensions are compared case-insensitively so a library containing `TRACK.FLAC` or
/// `Cover.JPG` isn't silently dropped on case-sensitive filesystems.
fn matches_extension(ext: &str, extensions: &Option<Vec<&'static str>>) -> bool {
    match extensions {
        Some(exts) => exts.iter().any(|allowed| allowed.eq_ignore_ascii_case(ext)),
        None => true,
    }
}

pub fn read_dir_recursively<P: AsRef<Path>>(path: P, extensions: &Option<Vec<&'static str>>) -> Result<Vec<PathBuf>> {
    let mut files = Vec::<PathBuf>::new();

    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            let mut sub_files = read_dir_recursively(path, extensions)?;
            files.append(&mut sub_files);
            continue;
        }

        let ext = match path.extension().and_then(|ext| ext.to_str()) {
            Some(ext) => ext,
            None => continue,
        };

        if matches_extension(ext, extensions) {
            files.push(path);
        }
    }

    Ok(files)
}

pub fn read_selectively<P, I>(paths: I, extensions: &Option<Vec<&'static str>>) -> Result<Vec<PathBuf>>
where
    P: AsRef<Path>,
    I: IntoIterator<Item = P>,
{
    let mut files = Vec::<PathBuf>::new();

    for entry in paths {
        let path = entry.as_ref();

        if !path.exists() {
            return Err(Error::descriptive("File does not exist").with_context(path.to_string_lossy()));
        }

        if path.is_dir() {
            let mut sub_files = read_dir_recursively(path, extensions)?;
            files.append(&mut sub_files);
            continue;
        }

        let ext = match path.extension().and_then(|ext| ext.to_str()) {
            Some(ext) => ext,
            None => continue,
        };

        if matches_extension(ext, extensions) {
            files.push(path.to_path_buf());
        }
    }

    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::read_dir_recursively;
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
    fn read_dir_recursively_matches_extensions_case_insensitively() {
        let root = unique_temp_path("recursive");
        let nested = root.join("Album");
        fs::create_dir_all(&nested).expect("source dirs should be created");

        for (dir, name) in [
            (&root, "a.flac"),
            (&root, "b.FLAC"),
            (&nested, "c.Flac"),
            (&nested, "d.opus"),
            (&nested, "notes.txt"),
            (&nested, "noext"),
        ] {
            fs::write(dir.join(name), b"x").expect("test file should be written");
        }

        let found = read_dir_recursively(&root, &Some(vec!["flac"])).expect("listing should succeed");
        let mut names = found
            .iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .collect::<Vec<_>>();
        names.sort();

        assert_eq!(names, vec!["a.flac", "b.FLAC", "c.Flac"]);

        // Extension-less files are skipped even when no extension filter is given.
        let all = read_dir_recursively(&root, &None).expect("listing should succeed");
        assert_eq!(all.len(), 5);

        // A case-mismatched filter entry should still match the on-disk casing.
        let upper = read_dir_recursively(&root, &Some(vec!["OPUS"])).expect("listing should succeed");
        assert_eq!(upper.len(), 1);
        assert!(upper[0].ends_with("d.opus"));

        let _ = fs::remove_dir_all(root);
    }
}
