use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use colored::*;
use serde_json::{Map, Value};

use crate::{
    errors::{Error, Result},
    format::Codec,
};

/// Bumped whenever the on-disk shape changes incompatibly; a mismatch discards the file
/// rather than trying to migrate it.
const CACHE_VERSION: u64 = 1;

/// Metadata fingerprint of a source file. A cache entry is only trusted while both parts
/// still match, so editing a file forces it to be probed again.
pub type Fingerprint = (u64, i64);

/// Codec detected for a file, or why it couldn't be detected.
///
/// Failed probes are cached as well, so a library with a handful of broken files doesn't
/// pay for re-reading (and re-reporting) them on every single run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    Codec(Codec),
    Unreadable,
}

/// JSON key used for cached probe failures.
const UNREADABLE: &str = "unreadable";

impl Probe {
    fn to_json_name(self) -> &'static str {
        match self {
            Probe::Codec(codec) => codec.to_variant_name(),
            Probe::Unreadable => UNREADABLE,
        }
    }

    fn from_json_name(name: &str) -> Option<Probe> {
        if name == UNREADABLE {
            return Some(Probe::Unreadable);
        }

        Codec::from_variant_name(name).map(Probe::Codec)
    }
}

/// Returns the fingerprint of `path`, or `None` if its metadata can't be read.
///
/// A file modified within the same timestamp tick as the probe could in principle get a
/// fingerprint that predates its final contents. At worst that costs one stale cache hit;
/// the next run notices the new fingerprint and re-probes.
pub fn fingerprint_of(path: &Path) -> Option<Fingerprint> {
    let meta = fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    let nanos = match mtime.duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX),
        // Pre-epoch mtimes are legal on some filesystems, and still usable for comparison.
        Err(e) => i64::try_from(e.duration().as_nanos()).map_or(i64::MIN, |n| -n),
    };

    Some((meta.len(), nanos))
}

/// On-disk cache of probe results, keyed by absolute source path.
///
/// Only ever loaded once, consulted in memory, and written back once at the end of a run,
/// so whole-file replacement via [`ProbeCache::save`] is the entire access pattern.
#[derive(Debug, Default)]
pub struct ProbeCache {
    entries: HashMap<PathBuf, Entry>,
    path: Option<PathBuf>,
    /// True when the run was asked to ignore cached results, in which case entries are
    /// rebuilt from scratch instead of being merged into whatever was loaded.
    refreshed: bool,
    seen: HashSet<PathBuf>,
    dirty: bool,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    size: u64,
    mtime_ns: i64,
    probe: Probe,
}

impl ProbeCache {
    /// Loads the cache from its default location, tolerating every failure mode.
    ///
    /// A missing file is the normal first-run case. A corrupt or unrecognised file is
    /// treated as empty (with a warning) because a cache must never break a sync.
    pub fn load(refresh: bool) -> Self {
        let Some(path) = default_cache_path() else {
            return Self {
                refreshed: refresh,
                ..Default::default()
            };
        };

        let mut cache = Self {
            path: Some(path.clone()),
            refreshed: refresh,
            ..Default::default()
        };

        // A refresh rewrites the file from scratch, so the old contents are irrelevant.
        if refresh {
            return cache;
        }

        match fs::read_to_string(&path) {
            Ok(contents) => match Self::parse(&contents) {
                Some(entries) => cache.entries = entries,
                None => warn(format!(
                    "Ignoring unreadable probe cache at {}, it will be rebuilt",
                    path.display()
                )),
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => warn(format!(
                "Couldn't read probe cache at {} ({e}), it will be rebuilt",
                path.display()
            )),
        }

        cache
    }

    /// Looks up a previously recorded probe result, stat-ing the file for its fingerprint.
    ///
    /// A hit also marks the path as seen for [`ProbeCache::prune`]: cached files are never
    /// re-inserted, so without this a warm run would consider every file untouched and
    /// discard the cache it just used.
    pub fn get(&mut self, path: &Path) -> Option<Probe> {
        let fingerprint = fingerprint_of(path)?;
        let probe = self.get_with_fingerprint(path, &fingerprint);

        if probe.is_some() {
            self.seen.insert(path.to_path_buf());
        }

        probe
    }

    /// Looks up a probe result for an already known `fingerprint`, avoiding a second `stat`.
    pub fn get_with_fingerprint(&self, path: &Path, fingerprint: &Fingerprint) -> Option<Probe> {
        if self.refreshed {
            return None;
        }

        let entry = self.entries.get(path)?;
        let (size, mtime_ns) = *fingerprint;

        (entry.size == size && entry.mtime_ns == mtime_ns).then_some(entry.probe)
    }

    /// Records the probe result for `path`, marking it as seen for [`ProbeCache::prune`].
    ///
    /// Keys must be absolute: a relative key would be resolved against whatever working
    /// directory a later run happens to use, silently producing wrong hits or misses.
    pub fn insert(&mut self, path: PathBuf, fingerprint: Fingerprint, probe: Probe) {
        debug_assert!(
            path.is_absolute(),
            "probe cache keys must be absolute: {}",
            path.display()
        );

        let (size, mtime_ns) = fingerprint;
        self.seen.insert(path.clone());

        let unchanged = self
            .entries
            .get(&path)
            .is_some_and(|old| old.size == size && old.mtime_ns == mtime_ns && old.probe == probe);

        if !unchanged {
            self.entries.insert(path, Entry { size, mtime_ns, probe });
            self.dirty = true;
        }
    }

    /// Drops entries that weren't visited this run, keeping the file from growing forever
    /// as different libraries are synced.
    ///
    /// `seen` holds every file this run looked at, whether it was a cache hit or not, which
    /// is what makes a warm run keep its entries.
    pub fn prune(&mut self) {
        let seen = &self.seen;
        let before = self.entries.len();
        self.entries.retain(|path, _| seen.contains(path));

        if self.entries.len() != before {
            self.dirty = true;
        }
    }

    /// Writes the cache back atomically (temp file + rename) when anything changed.
    ///
    /// Failures are reported as warnings rather than errors: losing a cache is never a
    /// reason to fail a sync that otherwise succeeded.
    pub fn save(&self) {
        if !self.dirty {
            return;
        }

        let Some(path) = &self.path else {
            return;
        };

        if let Err(e) = self.write_atomically(path) {
            warn(format!("Couldn't write probe cache to {} ({e})", path.display()));
        }
    }

    fn write_atomically(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| Error::descriptive("Probe cache path has no parent directory"))?;

        fs::create_dir_all(parent)?;

        let contents = self.serialize();
        let temp_path = parent.join(format!(".probe-cache.{}.tmp", std::process::id()));

        fs::write(&temp_path, contents)?;

        // Rename within the same directory is atomic, so readers never observe a torn file.
        if let Err(e) = fs::rename(&temp_path, path) {
            let _ = fs::remove_file(&temp_path);
            return Err(e.into());
        }

        Ok(())
    }

    fn serialize(&self) -> String {
        let mut entries = Map::new();
        for (path, entry) in &self.entries {
            let mut value = Map::new();
            value.insert("size".to_string(), Value::from(entry.size));
            value.insert("mtime_ns".to_string(), Value::from(entry.mtime_ns));
            value.insert("codec".to_string(), Value::from(entry.probe.to_json_name()));
            entries.insert(encode_key(path), Value::Object(value));
        }

        let mut root = Map::new();
        root.insert("version".to_string(), Value::from(CACHE_VERSION));
        root.insert("entries".to_string(), Value::Object(entries));

        serde_json::to_string(&Value::Object(root)).unwrap_or_else(|_| String::from("{}"))
    }

    /// Parses the on-disk format, returning `None` for anything not of the expected shape.
    ///
    /// Individual malformed entries are skipped rather than discarding the whole file.
    fn parse(contents: &str) -> Option<HashMap<PathBuf, Entry>> {
        let value: Value = serde_json::from_str(contents).ok()?;
        let root = value.as_object()?;

        if root.get("version")?.as_u64()? != CACHE_VERSION {
            return None;
        }

        let mut entries = HashMap::new();
        for (path, entry) in root.get("entries")?.as_object()? {
            let Some(entry) = entry.as_object() else {
                continue;
            };

            let (Some(size), Some(mtime_ns), Some(codec)) = (
                entry.get("size").and_then(Value::as_u64),
                entry.get("mtime_ns").and_then(Value::as_i64),
                entry.get("codec").and_then(Value::as_str),
            ) else {
                continue;
            };

            let Some(probe) = Probe::from_json_name(codec) else {
                continue;
            };

            entries.insert(decode_key(path), Entry { size, mtime_ns, probe });
        }

        Some(entries)
    }
}

/// Encodes a path as a JSON object key.
///
/// Paths aren't guaranteed to be UTF-8, and two different paths can render to the same
/// lossy string (`b"a\xffb"` and `b"a\xfeb"` both become `"a\u{fffd}b"`). Such paths are
/// escaped byte-wise instead, so distinct files never share a key. Valid UTF-8 paths are
/// stored as-is, keeping the file readable.
fn encode_key(path: &Path) -> String {
    match path.to_str() {
        Some(text) => text.to_string(),
        None => encode_non_utf8(path),
    }
}

/// Marker prefix for byte-wise escaped keys.
///
/// `\` cannot appear literally in the encoding (it always starts the marker) and the
/// payload is hex, so an escaped key can never collide with a UTF-8 path. It must also
/// survive a JSON round trip unchanged, which is why it isn't a control character: JSON
/// treats `"\0"` and `"\\0"` as the same string after dequoting, silently losing it.
const ESCAPED_KEY_PREFIX: &str = "\\0";

#[cfg(unix)]
fn encode_non_utf8(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;

    let mut encoded = String::from(ESCAPED_KEY_PREFIX);
    for byte in path.as_os_str().as_bytes() {
        encoded.push_str(&format!("{byte:02x}"));
    }

    encoded
}

#[cfg(not(unix))]
fn encode_non_utf8(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

/// Decodes a key produced by [`encode_key`].
fn decode_key(key: &str) -> PathBuf {
    if let Some(hex) = key.strip_prefix(ESCAPED_KEY_PREFIX) {
        return decode_escaped(hex).unwrap_or_else(|| PathBuf::from(key));
    }

    PathBuf::from(key)
}

#[cfg(unix)]
fn decode_escaped(hex: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;

    let bytes = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect::<Option<Vec<u8>>>()?;

    Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

#[cfg(not(unix))]
fn decode_escaped(_: &str) -> Option<PathBuf> {
    None
}

fn warn(message: String) {
    eprintln!("{} {message}", "warning:".yellow());
}

/// Resolves `~/.cache/tsync`, honouring `XDG_CACHE_HOME` when it is set.
fn default_cache_path() -> Option<PathBuf> {
    let dir = match std::env::var_os("XDG_CACHE_HOME").filter(|x| !x.is_empty()) {
        Some(cache_home) => PathBuf::from(cache_home),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".cache"),
    };

    Some(dir.join("tsync").join("probe-cache.json"))
}

#[cfg(test)]
mod tests {
    use super::{Probe, ProbeCache, fingerprint_of};
    use crate::format::Codec;
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
        std::env::temp_dir().join(format!("tsync-cache-test-{name}-{nanos}"))
    }

    /// Builds a cache backed by a temp directory, without touching the user's real one.
    fn temp_cache(name: &str) -> (ProbeCache, PathBuf) {
        let root = unique_temp_path(name);
        let path = root.join("tsync").join("probe-cache.json");
        let cache = ProbeCache {
            path: Some(path),
            dirty: true,
            ..Default::default()
        };
        (cache, root)
    }

    #[test]
    fn round_trips_entries_including_paths_with_spaces() {
        let (mut cache, root) = temp_cache("round-trip");

        // Library paths routinely contain spaces and non-ascii characters.
        let track = PathBuf::from("/music/Artist 00/Album 01/01 Ünïcode Track.flac");
        let fingerprint = (8596, 1_700_000_000_000_000_000);

        cache.insert(track.clone(), fingerprint, Probe::Codec(Codec::Flac));
        cache.insert(PathBuf::from("/music/broken.flac"), fingerprint, Probe::Unreadable);
        cache.save();

        let contents = fs::read_to_string(cache.path.as_ref().expect("path is set")).expect("cache file");
        let reloaded = ProbeCache::parse(&contents).expect("cache should parse");

        assert_eq!(reloaded[&track].probe, Probe::Codec(Codec::Flac));
        assert_eq!(reloaded[&track].size, 8596);
        assert_eq!(reloaded[&track].mtime_ns, fingerprint.1);
        assert_eq!(reloaded[&PathBuf::from("/music/broken.flac")].probe, Probe::Unreadable);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fingerprint_mismatch_is_a_miss() {
        let (mut cache, root) = temp_cache("fingerprint");
        let track = PathBuf::from("/music/track.flac");

        cache.insert(track.clone(), (100, 1), Probe::Codec(Codec::Flac));

        assert_eq!(
            cache.get_with_fingerprint(&track, &(100, 1)),
            Some(Probe::Codec(Codec::Flac))
        );

        // A changed size or mtime means the file was rewritten, so the entry is stale.
        assert_eq!(cache.get_with_fingerprint(&track, &(101, 1)), None);
        assert_eq!(cache.get_with_fingerprint(&track, &(100, 2)), None);
        assert_eq!(
            cache.get_with_fingerprint(&PathBuf::from("/music/other.flac"), &(100, 1)),
            None
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn refresh_ignores_existing_entries() {
        let (mut cache, root) = temp_cache("refresh");
        let track = PathBuf::from("/music/track.flac");

        cache.insert(track.clone(), (100, 1), Probe::Codec(Codec::Flac));
        assert!(cache.get_with_fingerprint(&track, &(100, 1)).is_some());

        cache.refreshed = true;
        assert_eq!(cache.get_with_fingerprint(&track, &(100, 1)), None);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn warm_lookups_survive_prune() {
        let (cache, root) = temp_cache("warm-prune");

        // A file from an earlier run, present on disk so a warm run can hit it.
        let real_file = root.join("Track 01.flac");
        fs::create_dir_all(&root).expect("dir");
        fs::write(&real_file, b"data").expect("file");

        // A file that has since disappeared: it can never be looked up, so it must go.
        let vanished = root.join("Vanished.flac");

        // Seed the cache the way a previous run would have left it, without marking
        // anything as seen by this run.
        let fingerprint = fingerprint_of(&real_file).expect("fingerprint");
        let seeded = format!(
            r#"{{"version":1,"entries":{{{}:{{"size":{},"mtime_ns":{},"codec":"Flac"}},{}:{{"size":1,"mtime_ns":1,"codec":"Mp3"}}}}}}"#,
            serde_json::to_string(&real_file.to_string_lossy()).expect("key"),
            fingerprint.0,
            fingerprint.1,
            serde_json::to_string(&vanished.to_string_lossy()).expect("key"),
        );

        let mut cache: ProbeCache = ProbeCache {
            entries: ProbeCache::parse(&seeded).expect("seed should parse"),
            ..cache
        };
        assert_eq!(cache.entries.len(), 2);

        // A warm run only looks files up; it never re-inserts cached ones.
        let hit = cache.get(&real_file);
        assert!(hit.is_some(), "the seeded entry should hit");
        cache.prune();

        let parsed = ProbeCache::parse(&cache.serialize()).expect("cache should parse");

        assert_eq!(parsed.len(), 1);
        assert!(parsed.contains_key(&real_file));
        assert!(!parsed.contains_key(&vanished));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn prune_drops_entries_not_seen_this_run() {
        let (mut cache, root) = temp_cache("prune");
        let kept = PathBuf::from("/music/kept.flac");
        let dropped = PathBuf::from("/music/dropped.flac");

        cache.insert(kept.clone(), (100, 1), Probe::Codec(Codec::Flac));
        cache.insert(dropped.clone(), (100, 1), Probe::Codec(Codec::Mp3));

        // A new run only sees `kept`: cached hits aren't re-inserted, so the run records
        // them as seen directly, the same way the fold does for a synced file.
        let mut cache = ProbeCache {
            seen: [kept.clone()].into_iter().collect(),
            ..cache
        };
        cache.prune();

        let parsed = ProbeCache::parse(&cache.serialize()).expect("cache should parse");

        assert!(parsed.contains_key(&kept));
        assert!(!parsed.contains_key(&dropped));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn tolerates_corrupt_and_foreign_cache_files() {
        // A cache must never break a sync, so every malformed shape parses to `None`.
        for contents in [
            "",
            "not json",
            "[]",
            "{}",
            r#"{"version": 1}"#,
            r#"{"version": 999, "entries": {}}"#,
            r#"{"version": "1", "entries": {}}"#,
        ] {
            assert!(ProbeCache::parse(contents).is_none(), "should reject: {contents}");
        }

        // Malformed individual entries are skipped, keeping the valid ones.
        let partial = r#"{
            "version": 1,
            "entries": {
                "/music/good.flac": {"size": 10, "mtime_ns": 5, "codec": "Flac"},
                "/music/no-size.flac": {"mtime_ns": 5, "codec": "Flac"},
                "/music/bad-codec.flac": {"size": 10, "mtime_ns": 5, "codec": "Ogg"},
                "/music/not-object.flac": 7
            }
        }"#;

        let parsed = ProbeCache::parse(partial).expect("cache should parse");
        assert_eq!(parsed.len(), 1);
        assert!(parsed.contains_key(&PathBuf::from("/music/good.flac")));
    }

    #[test]
    fn save_creates_missing_directories_and_leaves_no_temp_file() {
        let (mut cache, root) = temp_cache("save");
        let path = cache.path.clone().expect("path is set");

        assert!(!path.exists());

        cache.insert(PathBuf::from("/music/a.flac"), (1, 1), Probe::Codec(Codec::Flac));
        cache.save();

        assert!(path.exists());

        let parsed = ProbeCache::parse(&fs::read_to_string(&path).expect("cache file")).expect("cache should parse");
        assert_eq!(parsed.len(), 1);

        // Only the cache file itself should be left next to it.
        let leftovers = fs::read_dir(path.parent().expect("parent")).expect("read dir").count();
        assert_eq!(leftovers, 1);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn save_is_a_noop_when_nothing_changed() {
        let (mut cache, root) = temp_cache("noop");
        let path = cache.path.clone().expect("path is set");

        cache.dirty = false;
        cache.save();
        assert!(!path.exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fingerprint_of_reports_size() {
        let root = unique_temp_path("fingerprint-of");
        fs::create_dir_all(&root).expect("dir");
        let file = root.join("track.flac");
        fs::write(&file, b"12345").expect("file");

        assert_eq!(fingerprint_of(&file).expect("fingerprint").0, 5);

        fs::write(&file, b"123456").expect("file");
        assert_eq!(fingerprint_of(&file).expect("fingerprint").0, 6);

        assert!(fingerprint_of(&root.join("missing.flac")).is_none());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn codec_variant_names_round_trip() {
        for codec in [
            Codec::Opus,
            Codec::Vorbis,
            Codec::Mp3,
            Codec::AacLc,
            Codec::Flac,
            Codec::Alac,
        ] {
            let name = codec.to_variant_name();
            assert_eq!(Codec::from_variant_name(name), Some(codec), "{name} should round trip");
        }

        assert_eq!(Codec::from_variant_name("Ogg"), None);
        assert_eq!(Codec::from_variant_name(""), None);
    }

    #[test]
    fn non_utf8_paths_round_trip_exactly() {
        use std::os::unix::ffi::OsStrExt;

        let (mut cache, root) = temp_cache("non-utf8");

        // Library files can have non-UTF-8 names. They must survive a save/load cycle
        // exactly, and must not collide with each other or with valid UTF-8 paths.
        let bad = PathBuf::from(std::ffi::OsStr::from_bytes(b"/music/bad\xffname.flac"));
        let different_bad = PathBuf::from(std::ffi::OsStr::from_bytes(b"/music/bad\xfename.flac"));
        let lossy_lookalike = PathBuf::from("/music/bad\u{fffd}name.flac");

        cache.insert(bad.clone(), (1, 1), Probe::Codec(Codec::Flac));
        cache.insert(different_bad.clone(), (2, 2), Probe::Codec(Codec::Mp3));
        cache.insert(lossy_lookalike.clone(), (3, 3), Probe::Unreadable);

        let parsed = ProbeCache::parse(&cache.serialize()).expect("cache should parse");

        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[&bad].probe, Probe::Codec(Codec::Flac));
        assert_eq!(parsed[&different_bad].probe, Probe::Codec(Codec::Mp3));
        assert_eq!(parsed[&lossy_lookalike].probe, Probe::Unreadable);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn serialized_shape_is_versioned() {
        let (cache, root) = temp_cache("shape");
        let parsed: serde_json::Value = serde_json::from_str(&cache.serialize()).expect("should be valid json");

        assert_eq!(parsed["version"], serde_json::Value::from(1));
        assert!(parsed["entries"].is_object());

        let _ = fs::remove_dir_all(root);
    }
}
