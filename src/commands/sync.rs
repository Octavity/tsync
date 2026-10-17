use std::{
    collections::HashSet,
    env, fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
};

use clap::Args;
use colored::*;
use indicatif::{ProgressBar, ProgressStyle};

use crate::{
    errors::{Error, Result},
    format::{Codec, get_track_data},
    utils::{
        cache::{Fingerprint, Probe, ProbeCache},
        ffmpeg::transcode_file,
        fs::{FSBackend, read_dir_recursively, read_selectively},
        parallel::{map_parallel, run_parallel},
        parse_sync_list,
        path::PathExtensions,
    },
};

#[derive(Debug, Args)]
pub struct SyncOpts {
    /// The source directory to sync from.
    source: PathBuf,

    /// The directory to sync to.
    target: PathBuf,

    #[arg(long, short, default_value = "adb")]
    /// Specifies the filesystem backend to use for syncing.
    fs: FSBackend,

    #[arg(long, short)]
    /// The codec to transcode into for tracks matching the transcode_codecs.
    ///
    /// Transcoding will only apply if a value is passed, else only the files matched by `sync_extensions` will synced.
    codec: Option<Codec>,

    #[arg(long, short)]
    /// The bitrate to use while transcoding files matched by `transcode_extensions`.
    /// Only applies if `codec` is set.
    ///
    /// Default values:
    /// - opus: 128K
    /// - vorbis: 192K
    /// - mp3: 320K
    /// - aac-lc: 192K
    bitrate: Option<u32>,

    #[arg(long, default_value_t = false)]
    /// When enabled, extras like covers are included with the sync.
    include_extras: bool,

    #[arg(long, default_value_t = false)]
    /// When enabled, ignores cached metadata probe results and re-reads every file.
    ///
    /// Probe results are cached in ~/.cache/tsync/probe-cache.json, keyed by file path and
    /// invalidated when a file's size or modification time changes.
    refresh_cache: bool,

    #[arg(long, value_delimiter = ',', default_value = "flac,alac")]
    /// A comma-separated list of codecs to match to include in the transcode process.
    transcode_codecs: Vec<Codec>,

    #[arg(long, value_delimiter = ',', default_value = "opus,vorbis,mp3,aac-lc")]
    /// A comma-separated list of codecs to match to include only in the sync process.
    sync_codecs: Vec<Codec>,

    #[arg(
        long,
        long_help = "\
A text file containing a list of folders to sync. Folders listed must be exist within the source directory.

E.g. source -> ~/Music/Library:
    ESAI
    ~/Music/Library/K03
    ~/Music/Library/Various Artists/Stream Palette 4
    Various Artists/Stream Palette 5 -RANKED-"
    )]
    sync_list: Option<PathBuf>,
}

#[derive(Clone)]
struct SyncJob {
    source: PathBuf,
    rel_path: PathBuf,
}

#[derive(Clone)]
struct TranscodeJob {
    source: PathBuf,
    target_rel: PathBuf,
    rel_path: PathBuf,
}

/// Outcome of inspecting a single source file, before any copying happens.
///
/// Classification runs in parallel, so per-file failures (unreadable or unsupported
/// metadata) are represented as [`Classified::Skipped`] instead of aborting the whole run.
enum Classified {
    Transcode(TranscodeJob),
    Sync(SyncJob),
    /// Target already holds the file, so there is nothing to do.
    Existing(PathBuf),
    /// Not synced, along with the reason shown to the user.
    Skipped(PathBuf, String),
}

/// A classification paired with the probe result to persist in the cache.
///
/// `fingerprint` is only set for a result that was just probed from disk. Cached results
/// and path-only skips leave it `None`, so their existing entry is preserved untouched
/// rather than being rewritten from a fingerprint that wasn't re-read.
struct Inspection {
    path: PathBuf,
    fingerprint: Option<Fingerprint>,
    probe: Option<Probe>,
    classified: Classified,
}

/// Returns true when `ext` is produced by a sync codec but never by a transcode codec.
///
/// For such extensions the syncable target path is known without probing the file, so an
/// existing target can be skipped before paying for metadata reads. When `--codec` is not
/// set the sync and transcode codecs cannot overlap, making every extension safe to
/// short-circuit.
fn is_unambiguously_syncable(ext: &str, sync_exts: &[&'static str], transcode_exts: &[&'static str]) -> bool {
    let is_sync = sync_exts.iter().any(|x| x.eq_ignore_ascii_case(ext));
    let is_transcode = transcode_exts.iter().any(|x| x.eq_ignore_ascii_case(ext));

    is_sync && !is_transcode
}

/// Reason shown for files whose metadata couldn't be read, both live and from cache.
///
/// Cached failures can't reproduce the original probe error, so the wording has to be
/// stable between a fresh probe and a cached one.
const UNREADABLE_METADATA: &str = "due to unreadable metadata";

/// Turns a known codec into a concrete job, or explains why the file isn't part of the sync.
///
/// Shared by the cached and freshly probed paths so both always reach the same conclusion.
#[allow(clippy::too_many_arguments)]
fn classify(
    file: &Path,
    rel_path: &Path,
    codec: Codec,
    target_dir: &Path,
    target_file_list: &HashSet<PathBuf>,
    sync_codecs: &[Codec],
    transcode_codecs: &[Codec],
    transcode_codec: Option<Codec>,
) -> Result<Classified> {
    let is_syncable = sync_codecs.contains(&codec);
    let is_transcodable = !is_syncable && transcode_codecs.contains(&codec);

    let classified = match (is_syncable, is_transcodable) {
        (_, true) if transcode_codec.is_some() => {
            let codec = transcode_codec.expect("transcode codec is checked above");
            let target_rel = rel_path.with_extension(codec.extenstion_str());

            if target_file_list.contains(&target_dir.join(&target_rel)) {
                Classified::Existing(target_rel)
            } else {
                Classified::Transcode(TranscodeJob {
                    source: file.to_path_buf(),
                    target_rel,
                    rel_path: rel_path.to_path_buf(),
                })
            }
        }
        (true, _) => {
            if target_file_list.contains(&target_dir.join(rel_path)) {
                Classified::Existing(rel_path.to_path_buf())
            } else {
                Classified::Sync(SyncJob {
                    source: file.to_path_buf(),
                    rel_path: rel_path.to_path_buf(),
                })
            }
        }
        // `--codec` unset with overlapping codec lists lands here: transcoding is
        // disabled, so the file simply isn't part of this sync.
        (false, _) => Classified::Skipped(rel_path.to_path_buf(), "due to no codec".to_string()),
    };

    Ok(classified)
}

/// Units of progress a file that will be copied adds to the bar.
///
/// Every inspected file ticks once for discovery during classification, and files that are
/// then copied (transcoded or not) tick once more when the copy lands. Both the length and
/// the per-file increments are derived from this, so `pos` can never pass `len`.
const COPY_PROGRESS_UNITS: u64 = 2;

impl Classified {
    /// Total units of progress this file contributes to the bar.
    fn progress_units(&self) -> u64 {
        match self {
            Classified::Transcode(_) | Classified::Sync(_) => COPY_PROGRESS_UNITS,
            Classified::Existing(_) | Classified::Skipped(_, _) => 1,
        }
    }
}

pub fn run(opts: SyncOpts) -> Result<()> {
    let fs = opts.fs;

    let source_dir = Path::new(&opts.source);
    let target_dir = Path::new(&opts.target);
    let temp_dir = env::temp_dir().join("tsync");

    // Probe-cache keys must not depend on the working directory, so every recorded path is
    // absolute. This is lexical only: `canonicalize` would resolve symlinks, and a
    // symlinked mount point shouldn't change what gets cached.
    let source_dir_absolute =
        std::path::absolute(source_dir).map_err(|e| Error::from(e).with_context(source_dir.to_string_lossy()))?;
    let source_dir = source_dir_absolute.as_path();

    if !fs.available()? {
        let message = format!("{fs:?} is not available! Make sure everything is right.");
        return Err(Error::descriptive(message));
    }

    let sync_list_files = opts.sync_list.map(|x| parse_sync_list(source_dir, &x)).transpose()?;

    if let Err(e) = fs::create_dir(&temp_dir) {
        if e.kind() != io::ErrorKind::AlreadyExists {
            return Err(e.into());
        }

        fs::remove_dir_all(&temp_dir)?;
        fs::create_dir(&temp_dir)?;
    }

    let bitrate = opts
        .codec
        .as_ref()
        .map(|c| c.matching_bitrate(opts.bitrate))
        .transpose()?;

    // NOTE: this overlap check only runs when `--codec` is set. If it isn't, and
    // transcode_codecs/sync_codecs overlap, `is_syncable` silently wins over
    // `is_transcodable` below (see `!is_syncable &&`) since no transcoding happens
    // in that case anyway. Left as-is intentionally, not an oversight.
    if bitrate.is_some() && opts.transcode_codecs.iter().any(|tc| opts.sync_codecs.contains(tc)) {
        return Err(Error::descriptive("Sync and transcode codecs cannot overlap!"));
    }

    let sync_exts = opts
        .sync_codecs
        .iter()
        .map(|x| x.extenstion_str())
        .collect::<Vec<&'static str>>();

    let transcode_exts = opts
        .transcode_codecs
        .iter()
        .map(|x| x.extenstion_str())
        .collect::<Vec<&'static str>>();

    let files = {
        let readable_extensions = if opts.codec.is_some() {
            transcode_exts
                .iter()
                .chain(sync_exts.iter())
                .copied()
                .collect::<Vec<&'static str>>()
        } else {
            sync_exts.clone()
        };

        if let Some(within) = &sync_list_files {
            read_selectively(within, &Some(readable_extensions))?
        } else {
            read_dir_recursively(source_dir, &Some(readable_extensions))?
        }
    };

    println!("Found {} files", files.len().to_string().green());

    let track_count = files.len();
    let indicator = {
        let len = track_count as u64;
        let style = ProgressStyle::with_template("{msg}\n[{elapsed_precise}] [{wide_bar:.cyan/blue}] [{pos}/{len}]")
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("#>-");
        ProgressBar::new(len).with_style(style)
    };

    indicator.set_message("Building file list...");

    let mut parent_set = HashSet::<PathBuf>::with_capacity(files.len() / 3);
    let target_file_list = target_dir
        .try_exists()
        .map_err(|e| Error::from(e).with_context(target_dir.to_string_lossy()))?
        .then(|| fs.build_file_list(target_dir))
        .transpose()?
        .unwrap_or_else(|| HashSet::with_capacity(0));

    // Probing requires opening and reading the head of every file, so it dominates the
    // runtime of this phase. Do it across all cores, then fold the results back into jobs
    // single-threaded so progress reporting and job ordering stay deterministic.
    let target_file_list = Arc::new(target_file_list);
    let num_threads = thread::available_parallelism().map(|n| n.get()).unwrap_or(4);

    let inspected = {
        // `map_parallel` requires `'static` closures, so the cache is shared rather than
        // borrowed. A hit only records that the path was seen, so brief locks are cheap.
        let cache = Arc::new(Mutex::new(ProbeCache::load(opts.refresh_cache)));
        let target_dir = target_dir.to_path_buf();
        let source_dir = source_dir.to_path_buf();
        let target_file_list = target_file_list.clone();
        let sync_exts = sync_exts.clone();
        let transcode_exts = transcode_exts.clone();
        let transcode_codec = opts.codec;
        let sync_codecs = opts.sync_codecs.clone();
        let transcode_codecs = opts.transcode_codecs.clone();
        let indicator = indicator.clone();
        let worker_cache = cache.clone();

        let results = map_parallel(files, num_threads, move |file| -> Result<Inspection> {
            let rel_path = file
                .strip_prefix(&source_dir)
                .map_err(|_| Error::descriptive("File path is outside of the source directory"))?
                .to_path_buf();
            let extension = file.get_file_ext().ok_or_else(|| {
                Error::descriptive("Track file has no extension").with_context(file.to_string_lossy())
            })?;

            // Without `--codec` nothing can be transcoded, and with it only files that can
            // never be transcoded can be matched against the target by path alone. This
            // lets a repeat sync skip files without paying for a probe at all.
            let known_target = match transcode_codec {
                None => Some(rel_path.clone()),
                Some(_) if is_unambiguously_syncable(&extension, &sync_exts, &transcode_exts) => Some(rel_path.clone()),
                Some(_) => None,
            };

            if let Some(target_rel) = known_target
                && target_file_list.contains(&target_dir.join(&target_rel))
            {
                indicator.inc(1);
                let classified = Classified::Existing(target_rel);

                return Ok(Inspection {
                    path: file,
                    fingerprint: None,
                    probe: None,
                    classified,
                });
            }

            // A cached result avoids reopening the file, which is the expensive part.
            let cached = worker_cache.lock().unwrap_or_else(|e| e.into_inner()).get(&file);

            if let Some(Probe::Codec(codec)) = cached {
                indicator.set_message(format!("Cached {}", rel_path.get_file_name()));
                indicator.inc(1);

                let classified = classify(
                    &file,
                    &rel_path,
                    codec,
                    &target_dir,
                    &target_file_list,
                    &sync_codecs,
                    &transcode_codecs,
                    transcode_codec,
                )?;

                return Ok(Inspection {
                    path: file,
                    fingerprint: None,
                    probe: Some(Probe::Codec(codec)),
                    classified,
                });
            }

            if let Some(Probe::Unreadable) = cached {
                indicator.set_message(format!("Cached unreadable {}", rel_path.get_file_name()));
                indicator.inc(1);

                return Ok(Inspection {
                    path: file,
                    fingerprint: None,
                    probe: Some(Probe::Unreadable),
                    classified: Classified::Skipped(rel_path, UNREADABLE_METADATA.to_string()),
                });
            }

            indicator.set_message(format!("Probing {}", rel_path.get_file_name()));

            // A single unreadable or unsupported file shouldn't abort an entire library sync.
            let meta = match get_track_data(&file, &extension) {
                Ok(meta) => meta,
                Err(e) => {
                    indicator.inc(1);
                    return Ok(Inspection {
                        path: file,
                        fingerprint: None,
                        probe: Some(Probe::Unreadable),
                        classified: Classified::Skipped(rel_path, format!("{UNREADABLE_METADATA} ({e})")),
                    });
                }
            };

            indicator.inc(1);

            let classified = classify(
                &file,
                &rel_path,
                meta.codec,
                &target_dir,
                &target_file_list,
                &sync_codecs,
                &transcode_codecs,
                transcode_codec,
            )?;
            let probe = Some(Probe::Codec(meta.codec));

            Ok(Inspection {
                path: file,
                fingerprint: meta.fingerprint,
                probe,
                classified,
            })
        })?;

        (cache, results)
    };

    let (cache, inspected) = inspected;
    let mut transcode_jobs = Vec::<TranscodeJob>::new();
    let mut sync_jobs = Vec::<SyncJob>::new();
    let mut skipped = Vec::<String>::new();
    // Every inspected file already ticked its discovery unit while being classified, so the
    // length starts there and only grows by the copies still to come. Setting it from the
    // same `progress_units` that classification ticked keeps `pos` from ever passing `len`.
    let mut progress_len: u64 = track_count as u64;

    // All workers were joined by `map_parallel`, so this is the only handle left.
    let mut cache = Arc::into_inner(cache)
        .ok_or_else(|| Error::descriptive("Probe cache handle is still shared"))?
        .into_inner()
        .unwrap_or_else(|e| e.into_inner());

    for inspection in inspected {
        let Inspection {
            path,
            fingerprint,
            probe,
            classified,
        } = inspection;

        // Persist whatever was learned about this file, now that we're single-threaded.
        if let (Some(fingerprint), Some(probe)) = (fingerprint, probe) {
            cache.insert(path, fingerprint, probe);
        }

        // Discovery already ticked one unit for this file; budget the copy it still owes.
        progress_len += classified.progress_units();

        match classified {
            Classified::Transcode(job) => {
                if let Some(parent) = job.source.parent() {
                    parent_set.insert(parent.to_path_buf());
                }

                transcode_jobs.push(job);
            }
            Classified::Sync(job) => {
                if let Some(parent) = job.source.parent() {
                    parent_set.insert(parent.to_path_buf());
                }

                sync_jobs.push(job);
            }
            Classified::Existing(target_rel) => {
                indicator.set_message(format!("{} already exists", target_rel.get_file_name()));
            }
            Classified::Skipped(rel_path, reason) => {
                indicator.set_message(format!("Skipping {} {reason}", rel_path.get_file_name()));
                skipped.push(format!("{} {reason}", rel_path.display()));
            }
        }
    }

    // The length has to cover everything that will tick, set before any copy starts. Setting it
    // here (single-threaded, with classification finished) also keeps the draw rate limiter
    // from skipping the update.
    indicator.set_length(progress_len);

    // Written once, from this single-threaded section, so parallel workers never touch the file.
    cache.prune();
    cache.save();

    if !skipped.is_empty() {
        indicator.suspend(|| {
            println!("\n{} {} file(s):", "Skipped".yellow(), skipped.len());
            for entry in &skipped {
                println!("  {} {entry}", "-".dimmed());
            }
        });
    }

    if !transcode_jobs.is_empty() {
        let codec = opts
            .codec
            .ok_or_else(|| Error::descriptive("Codec must be set for transcode jobs"))?;
        let bitrate = bitrate.ok_or_else(|| Error::descriptive("Bitrate must be set for transcode jobs"))?;
        let temp_dir = Arc::new(temp_dir.clone());
        let target_dir_owned = target_dir.to_path_buf();
        let fs = fs.clone();
        let indicator = indicator.clone();

        run_parallel(transcode_jobs, num_threads, move |job: TranscodeJob| -> Result<()> {
            let TranscodeJob {
                source: file,
                target_rel,
                rel_path,
            } = job;

            let temp_path = temp_dir.join(&target_rel);

            if let Some(parent) = temp_path.parent() {
                let _ = fs::create_dir_all(parent);
            }

            transcode_file(&file, &temp_path, codec, bitrate)?;

            let target_path = target_dir_owned.join(&target_rel);
            indicator.set_message(format!("Syncing {:?}", rel_path.get_file_name()));

            fs.cp(&temp_path, &target_path)?;
            fs::remove_file(&temp_path)?;

            // One unit per job, ticked only once the output is actually in place.
            indicator.inc(1);
            Ok(())
        })?;
    }

    // Sync non-transcoded files in parallel
    if !sync_jobs.is_empty() {
        let target_dir_owned = target_dir.to_path_buf();
        let fs = fs.clone();
        let indicator = indicator.clone();

        run_parallel(sync_jobs, num_threads, move |job: SyncJob| -> Result<()> {
            let SyncJob { source: file, rel_path } = job;
            let target_path = target_dir_owned.join(&rel_path);

            indicator.set_message(format!("Syncing {:?}", rel_path.get_file_name()));

            fs.cp(&file, &target_path).map_err(|e| {
                let context = format!("While copying {file:#?} to {target_path:#?}");
                e.with_context(context)
            })?;

            // Only a completed copy counts towards the bar, since `run_parallel` drains every
            // job even after one fails. Classification already ticked discovery for this file.
            indicator.inc(1);
            Ok(())
        })?;
    }

    if opts.include_extras {
        let exts = vec!["jpg", "png", "jpeg"];
        let files = read_selectively(parent_set, &Some(exts))?;
        let files = files.into_iter().filter(|x| x.is_extra()).collect::<Vec<_>>();

        // Build extra jobs, filtering out already-existing files, before touching the bar.
        // `target_file_list` is the full listing of the target already fetched above, so
        // this avoids one `adb shell` round-trip per cover image.
        //
        // The length is derived from the jobs that will actually be copied. An extra that is
        // already present is not a unit of work, so counting it in both the length and the
        // position is what made the progress read past the total.
        let mut extra_jobs: Vec<SyncJob> = Vec::new();
        let mut existing_extras = Vec::<PathBuf>::new();
        for file in files {
            let rel_path = file
                .strip_prefix(source_dir)
                .map_err(|_| Error::descriptive("Extra file path is outside of source directory"))?
                .to_path_buf();

            if target_file_list.contains(&target_dir.join(&rel_path)) {
                existing_extras.push(rel_path);
                continue;
            }

            extra_jobs.push(SyncJob { source: file, rel_path });
        }

        // Extras are never classified, so they cost only their copy unit. Growing the
        // existing length (instead of recomputing it from `track_count`) keeps the budget in
        // step with the copy units already added for the tracks.
        indicator.inc_length(extra_jobs.len() as u64);
        // Force a draw carrying the new length, otherwise the next copy can report the old
        // length with an already-advanced position, reading like `6/5`.
        indicator.tick();

        for rel_path in &existing_extras {
            indicator.set_message(format!("{} already exists", rel_path.get_file_name()));
        }

        if !extra_jobs.is_empty() {
            let target_dir_owned = target_dir.to_path_buf();
            let fs = fs.clone();
            let indicator = indicator.clone();

            run_parallel(extra_jobs, num_threads, move |job: SyncJob| -> Result<()> {
                let SyncJob { source: file, rel_path } = job;
                let target_path = target_dir_owned.join(&rel_path);

                indicator.set_message(format!("Syncing extra {}", rel_path.get_file_name()));

                fs.cp(&file, &target_path).map_err(|e| {
                    let context = format!("While copying {file:#?} to {target_path:#?}");
                    e.with_context(context)
                })?;

                indicator.inc(1);
                Ok(())
            })?;
        }
    }

    indicator.finish_with_message("Done!");

    Ok(())
}
