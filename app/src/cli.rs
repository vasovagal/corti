//! Headless CLI for the `corti` binary.
//!
//! `corti` with no arguments launches the menu-bar tray app (today's behavior). A handful of flags branch
//! off in [`crate::main`] *before* the Tauri event loop starts, run a one-shot task, and `std::process::exit`
//! — the tray, detector, and pipeline worker never start:
//!
//! ```text
//! corti --redo <recording> [--local|--aws|--backend <b>] [--print]
//! corti --list
//! corti --help | --version
//! ```
//!
//! `--redo` re-transcribes an already-captured recording with the (optionally overridden) backend and files
//! a fresh vagus note — the manual re-run for a recording transcribed by the wrong backend (e.g. AWS when
//! you wanted on-device Parakeet). It reuses the same transcription core as the pipeline worker
//! ([`crate::transcribe::transcribe_recording`]) and the same queue/vagus plumbing, so the result matches a
//! live capture. Because vagus filing isn't idempotent, a re-do files a *new* note and reports the old one
//! (from the queue) rather than deleting anything.
//!
//! Arg parsing is hand-rolled (no external crate), mirroring `crates/corti-tap/src/main.rs`.
//!
//! Tests cover the pure parser/resolver logic; the real transcription (`transcribe_recording`, which needs
//! a model dir or AWS creds) and vagus filing are exercised by manual/integration runs, not CI unit tests.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{Local, NaiveDateTime, TimeZone};
use corti_core::{JobStatus, OwningApp, RecordingMeta};
use corti_queue::{Job, JobUpdate, Queue};
use corti_vagus::Vagus;

use crate::config::{AppConfig, BackendChoice};
use crate::transcribe::Backend;

const USAGE: &str = "\
corti — menu-bar call recorder + transcriber

USAGE:
    corti                                  launch the menu-bar tray app (default)
    corti --input <wav> [options]          transcribe a WAV to a note at a path you choose (primitive)
    corti --redo <recording> [options]     re-transcribe a tracked recording and file via vagus
    corti --list                           list tracked recordings and their pipeline status
    corti --help | -h                      show this help
    corti --version | -V                   show the version

INPUT OPTIONS (--input):
    -o, --output <path>      write the rendered note here (default: stdout)
    --title <text>           note title (default: derived from the input filename)
    --source <text>          note `source` frontmatter (default: derived from the input filename)
    --backend <aws|local>    backend to use (default: the configured backend)
    --local | --aws          shorthand for --backend local|aws
    --no-aec                 transcribe the input as-is (skip offline echo cancellation)

REDO OPTIONS (--redo):
    --backend <aws|local>    backend to use for this run (default: the configured backend)
    --local | --aws          shorthand for --backend local|aws
    --print                  print the transcript to stdout; do NOT file a note or touch the queue

--input is the plain primitive: you give it an exact WAV and an exact --output path, it transcribes and
writes a note (frontmatter + title + transcript) — no cache/queue/vagus, nothing is filed or indexed. AEC
runs automatically on a 2-channel (mic+tap) WAV and is skipped on a mono or *-clean.wav input. A 2-channel
input writes a sibling <name>-clean.wav. To index a note written into the vault, run `vagus index`.

--redo is the convenience path: <recording> may be a recordings-cache filename, a path, a *-clean.wav, or
a bare stem (bare names resolve under ~/Library/Caches/corti/recordings, or $CORTI_RECORDINGS_DIR); it
files through vagus and reflects the new note in the queue/tray history.";

/// A parsed command line. `Run` is the default (no/blank args) and launches the tray.
#[derive(Debug, PartialEq)]
pub enum Cli {
    Run,
    Redo(RedoArgs),
    Transcribe(TranscribeArgs),
    List,
    Help,
    Version,
}

/// Options for `--redo`: corti resolves the recording (cache dir + queue) and files through vagus.
#[derive(Debug, PartialEq)]
pub struct RedoArgs {
    /// The recording as typed (filename, path, `-clean.wav`, or bare stem).
    pub input: String,
    /// Backend override for this run; `None` ⇒ the configured/env backend.
    pub backend: Option<BackendChoice>,
    /// `--print`: dump the transcript to stdout instead of filing a note.
    pub print_only: bool,
}

/// Options for `--input`: the low-level primitive — caller supplies exact paths, corti transcribes and
/// renders a note. No cache/queue/vagus resolution; nothing is filed or indexed.
#[derive(Debug, PartialEq)]
pub struct TranscribeArgs {
    /// Exact path to the input WAV.
    pub input: String,
    /// Where to write the rendered note; `None` ⇒ stdout.
    pub output: Option<String>,
    /// Note title override; `None` ⇒ derived from the input filename.
    pub title: Option<String>,
    /// Note `source` (frontmatter) override; `None` ⇒ derived from the input filename.
    pub source: Option<String>,
    /// Backend override for this run; `None` ⇒ the configured/env backend.
    pub backend: Option<BackendChoice>,
    /// `--no-aec`: transcribe the input as-is (skip offline echo cancellation).
    pub no_aec: bool,
}

/// Parse the process arguments, exiting with usage on error. Thin wrapper over [`parse_from`] (which is the
/// unit-tested core).
pub fn parse() -> Cli {
    parse_from(std::env::args().skip(1)).unwrap_or_else(|msg| {
        eprintln!("corti: {msg}\n\n{USAGE}");
        std::process::exit(1);
    })
}

/// The testable parser: `Ok(Cli)` or `Err(usage message)`.
fn parse_from<I: Iterator<Item = String>>(mut args: I) -> Result<Cli, String> {
    let Some(first) = args.next() else {
        return Ok(Cli::Run); // no args ⇒ launch the tray (unchanged default)
    };
    match first.as_str() {
        "--help" | "-h" => Ok(Cli::Help),
        "--version" | "-V" => Ok(Cli::Version),
        "--list" => match args.next() {
            Some(extra) => Err(format!("--list takes no arguments (got `{extra}`)")),
            None => Ok(Cli::List),
        },
        "--redo" => {
            let input = args
                .next()
                .ok_or("--redo requires a recording (filename, path, or stem)")?;
            let mut backend = None;
            let mut print_only = false;
            // `while let` (not `for`) so `--backend` can consume the following value, mirroring corti-tap.
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--backend" => {
                        let v = args
                            .next()
                            .ok_or("--backend requires a value (aws|local)")?;
                        backend = Some(parse_backend_flag(&v)?);
                    }
                    "--local" => backend = Some(BackendChoice::Local),
                    "--aws" => backend = Some(BackendChoice::Aws),
                    "--print" | "--no-file" | "--stdout" => print_only = true,
                    "--file" => print_only = false,
                    other => return Err(format!("unknown option to --redo: `{other}`")),
                }
            }
            Ok(Cli::Redo(RedoArgs {
                input,
                backend,
                print_only,
            }))
        }
        "--input" => {
            let input = args
                .next()
                .ok_or("--input requires a path to a .wav file")?;
            let mut output = None;
            let mut title = None;
            let mut source = None;
            let mut backend = None;
            let mut no_aec = false;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--output" | "-o" => {
                        output = Some(args.next().ok_or("--output requires a path")?);
                    }
                    "--title" => title = Some(args.next().ok_or("--title requires a value")?),
                    "--source" => source = Some(args.next().ok_or("--source requires a value")?),
                    "--backend" => {
                        let v = args
                            .next()
                            .ok_or("--backend requires a value (aws|local)")?;
                        backend = Some(parse_backend_flag(&v)?);
                    }
                    "--local" => backend = Some(BackendChoice::Local),
                    "--aws" => backend = Some(BackendChoice::Aws),
                    "--no-aec" => no_aec = true,
                    other => return Err(format!("unknown option to --input: `{other}`")),
                }
            }
            Ok(Cli::Transcribe(TranscribeArgs {
                input,
                output,
                title,
                source,
                backend,
                no_aec,
            }))
        }
        other => Err(format!("unknown argument: `{other}`")),
    }
}

fn parse_backend_flag(v: &str) -> Result<BackendChoice, String> {
    match v.to_ascii_lowercase().as_str() {
        "aws" => Ok(BackendChoice::Aws),
        "local" => Ok(BackendChoice::Local),
        other => Err(format!("unknown backend `{other}` (expected aws|local)")),
    }
}

/// Run a parsed command (everything except `Run`, which [`crate::main`] handles by launching the tray) and
/// return a process exit code (0 ok, 1 error/usage).
pub fn dispatch(cli: Cli) -> i32 {
    let result = match cli {
        Cli::Run => return 0, // main intercepts Run before dispatch; never reached.
        Cli::Help => {
            println!("{USAGE}");
            return 0;
        }
        Cli::Version => {
            println!("corti {}", env!("CARGO_PKG_VERSION"));
            return 0;
        }
        Cli::List => run_list(),
        Cli::Redo(args) => run_redo(args),
        Cli::Transcribe(args) => run_transcribe(args),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("corti: {e:#}");
            1
        }
    }
}

/// `--list`: every tracked recording, newest first, with its status and filed note (if any).
fn run_list() -> Result<()> {
    let queue = Queue::open().context("opening the job queue")?;
    let jobs = queue.all().context("reading recordings")?;
    if jobs.is_empty() {
        println!("no recordings tracked yet");
        return Ok(());
    }
    // `all()` is oldest-first; show newest first.
    for job in jobs.iter().rev() {
        let when = job.started_at.format("%Y-%m-%d %H:%M");
        let note = job
            .note_path
            .as_deref()
            .map(|p| format!("  → {}", p.display()))
            .unwrap_or_default();
        println!(
            "{:<23}  {:<16}  {when}  {:<12}{note}",
            job.id,
            job.owning_app,
            status_label(job.status),
        );
    }
    Ok(())
}

/// A short, lowercase label for a pipeline status (the `--list` column).
fn status_label(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Recording => "recording",
        JobStatus::PendingTranscription => "pending",
        JobStatus::Transcribing => "transcribing",
        JobStatus::PendingNote => "filing",
        JobStatus::Done => "done",
        JobStatus::Failed => "failed",
    }
}

/// `--redo`: re-transcribe `args.input` with the (optionally overridden) backend, then file a fresh note (or
/// print it with `--print`).
fn run_redo(args: RedoArgs) -> Result<()> {
    // Load config, then let the CLI backend flag win for THIS run. Env (`CORTI_TRANSCRIBE_BACKEND`) still
    // applies underneath when no flag is given.
    let mut cfg = AppConfig::load();
    if let Some(choice) = args.backend {
        cfg.transcribe_backend = choice;
    }
    let backend_label = cfg.backend_name();
    if backend_label == "none" {
        bail!(
            "the requested transcription backend is not compiled into this build (have: {})",
            compiled_backends()
        );
    }
    let aec_enabled = cfg.aec_enabled;

    // Open the queue best-effort: a missing/locked queue must not block re-doing an on-disk file. When it
    // opens it gives us the authoritative recording metadata + the old note path, and lets us reflect the
    // new note back so `--list`/tray history point at it.
    let queue = Queue::open()
        .map_err(|e| eprintln!("[corti] queue unavailable ({e:#}); resolving from disk only"))
        .ok();

    let resolved = resolve_recording(&args.input, queue.as_ref())?;
    let effective_aec = aec_enabled && !resolved.skip_aec;
    eprintln!(
        "[corti] re-transcribing {} (id {}) with {backend_label}; AEC {}",
        resolved.audio.display(),
        resolved.id,
        if effective_aec { "on" } else { "off" },
    );

    let backend = Backend::init(cfg);
    let (transcript, used) = crate::transcribe::transcribe_recording(
        &backend,
        aec_enabled,
        resolved.skip_aec,
        &resolved.id,
        &resolved.meta,
        &resolved.audio,
    )
    .context("transcription failed")?;
    eprintln!(
        "[corti] transcribed {} segment(s) from {}",
        transcript.segments.len(),
        used.display(),
    );

    if args.print_only {
        // Print mode: dump the transcript; leave the vault and queue untouched.
        print!("{}", transcript.to_markdown());
        return Ok(());
    }

    let vagus = Vagus::discover()
        .context("vagus not available (needed to file the note; pass --print to skip filing)")?;
    let note = vagus
        .file_recording(&resolved.meta, &transcript)
        .context("filing the note into vagus")?;
    println!("filed note: {}", note.display());

    // Best-effort: point the queue row at the new note. The note is already filed, so this column is purely
    // cosmetic — a brief lock race against a running tray is harmless and just skips the update.
    if resolved.had_row
        && let Some(q) = &queue
        && let Err(e) = q.update(
            &resolved.id,
            JobUpdate {
                status: Some(JobStatus::Done),
                note_path: Some(note.clone()),
                ..Default::default()
            },
        )
    {
        eprintln!(
            "[corti] note filed, but could not update the queue row for {} ({e:#})",
            resolved.id
        );
    }

    // Report (never delete) the stale note from the earlier run so you can remove it yourself.
    if let Some(old) = resolved.old_note
        && old != note
    {
        println!("previous note left in place (delete it if you no longer want it):");
        println!("  {}", old.display());
    }

    Ok(())
}

/// `--input`: the low-level primitive. Transcribe an explicit WAV and render a note to `--output` (or
/// stdout). No cache/queue/vagus resolution — the caller owns the paths and any filing/indexing.
fn run_transcribe(args: TranscribeArgs) -> Result<()> {
    let mut cfg = AppConfig::load();
    if let Some(choice) = args.backend {
        cfg.transcribe_backend = choice;
    }
    let backend_label = cfg.backend_name();
    if backend_label == "none" {
        bail!(
            "the requested transcription backend is not compiled into this build (have: {})",
            compiled_backends()
        );
    }

    let input = Path::new(&args.input);
    if !input.exists() {
        bail!("no such file: {}", input.display());
    }
    let name = input
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = stem_from_name(&name);
    // AEC runs by default; --no-aec forces it off, and an already-`-clean.wav` is never re-cancelled
    // (write_clean_wav itself reports "nothing to do" for a mono/tap-only WAV via Ok(None)).
    let aec_enabled = !args.no_aec;
    let skip_aec = name.ends_with("-clean.wav");
    let job_id = if stem.is_empty() {
        "input".to_string()
    } else {
        stem.clone()
    };
    let meta = derive_meta_from_stem(&stem, input);

    eprintln!(
        "[corti] transcribing {} with {backend_label}; AEC {}",
        input.display(),
        if aec_enabled && !skip_aec {
            "on"
        } else {
            "off"
        },
    );
    let backend = Backend::init(cfg);
    let (transcript, used) = crate::transcribe::transcribe_recording(
        &backend,
        aec_enabled,
        skip_aec,
        &job_id,
        &meta,
        input,
    )
    .context("transcription failed")?;
    eprintln!(
        "[corti] transcribed {} segment(s) from {}",
        transcript.segments.len(),
        used.display(),
    );

    let title = args.title.unwrap_or_else(|| meta.note_title());
    let source = args.source.unwrap_or_else(|| meta.source());
    let body = corti_vagus::recording_body(&meta, &transcript);
    let note = render_note(&title, &source, &body, Local::now());

    match &args.output {
        Some(path) => {
            std::fs::write(path, &note).with_context(|| format!("writing note to {path}"))?;
            println!("wrote note: {path}");
        }
        None => print!("{note}"),
    }
    Ok(())
}

/// Render a standalone note — YAML frontmatter + an H1 title + `body` — mirroring what `vagus add-note`
/// produces, so a note written by `--input --output` matches one filed by `--redo`/vagus. corti renders
/// this itself on purpose: the `vagus` CLI can only file into its own inbox with its own generated
/// filename, and `--input` exists precisely to let the caller choose the path. This duplicates vagus's
/// frontmatter shape (`created`/`status`/`source`); keep it in sync if vagus's note format changes.
fn render_note(title: &str, source: &str, body: &str, created: chrono::DateTime<Local>) -> String {
    format!(
        "---\ncreated: {}\nstatus: inbox\nsource: {source}\n---\n\n# {title}\n\n{body}",
        created.format("%Y-%m-%dT%H:%M"),
    )
}

/// A recording resolved for re-transcription: which audio file to feed the backend, the metadata for the
/// note, and bookkeeping about its queue row.
struct Resolved {
    /// The durable id (queue job id / recording stem).
    id: String,
    /// The audio file to transcribe (raw 2-track WAV, or a `-clean.wav` when the raw is gone).
    audio: PathBuf,
    /// Metadata for the filed note (authoritative from the queue, else synthesized from the filename).
    meta: RecordingMeta,
    /// `true` when `audio` is already a `-clean.wav` (AEC output) — skip AEC to avoid double-cancelling.
    skip_aec: bool,
    /// The note filed by the previous run, if any (reported so the user can delete the stale one).
    old_note: Option<PathBuf>,
    /// Whether a queue row exists (⇒ safe to write the new note path back).
    had_row: bool,
}

/// Resolve the `--redo` argument to a [`Resolved`]. Prefers the durable queue row (authoritative metadata +
/// the old note); falls back to the on-disk file and metadata synthesized from the filename.
fn resolve_recording(input: &str, queue: Option<&Queue>) -> Result<Resolved> {
    let path = Path::new(input);
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .with_context(|| format!("`{input}` is not a recording path"))?;
    let stem = stem_from_name(&name);
    if stem.is_empty() {
        bail!("`{input}` has no recording stem to identify");
    }
    let arg_is_clean = name.ends_with("-clean.wav");
    let has_parent = path.parent().is_some_and(|p| !p.as_os_str().is_empty());

    // The durable row (by stem) is authoritative for app/timestamps and carries the old note path.
    let row = match queue {
        Some(q) => q.get(&stem).context("reading the job queue")?,
        None => None,
    };

    // Which audio file to transcribe (+ whether it's already a -clean.wav).
    let (audio, skip_aec) = if has_parent {
        // An explicit path: honor exactly what was given.
        if !path.exists() {
            bail!("no such file: {}", path.display());
        }
        (path.to_path_buf(), arg_is_clean)
    } else {
        resolve_audio_in_cache(&stem, &name, arg_is_clean, row.as_ref())?
    };

    let (id, meta, old_note, had_row) = match &row {
        Some(job) => (job.id.clone(), job.meta(), job.note_path.clone(), true),
        None => (
            stem.clone(),
            derive_meta_from_stem(&stem, &audio),
            None,
            false,
        ),
    };

    Ok(Resolved {
        id,
        audio,
        meta,
        skip_aec,
        old_note,
        had_row,
    })
}

/// Resolve a bare filename/stem to an audio file in the recordings cache. Prefers the raw WAV the row was
/// recorded to (then its `-clean.wav`); with no row, looks up the literal name, then `<stem>.wav`, then
/// `<stem>-clean.wav`.
fn resolve_audio_in_cache(
    stem: &str,
    name: &str,
    arg_is_clean: bool,
    row: Option<&Job>,
) -> Result<(PathBuf, bool)> {
    if let Some(job) = row {
        if job.audio_path.exists() {
            return Ok((job.audio_path.clone(), false));
        }
        let clean = corti_capture::clean_wav_path(&job.audio_path);
        if clean.exists() {
            return Ok((clean, true));
        }
        bail!(
            "recording `{stem}` is tracked but its audio file is gone (pruned by the 30-day retention?); \
             nothing to re-transcribe"
        );
    }

    let dir = corti_capture::recordings_dir().context("resolving the recordings cache dir")?;
    let literal = dir.join(name);
    if literal.exists() {
        return Ok((literal, arg_is_clean));
    }
    let raw = dir.join(format!("{stem}.wav"));
    if raw.exists() {
        return Ok((raw, false));
    }
    let clean = dir.join(format!("{stem}-clean.wav"));
    if clean.exists() {
        return Ok((clean, true));
    }
    bail!(
        "no recording found for `{name}` in {} (looked for {stem}.wav and {stem}-clean.wav)",
        dir.display()
    );
}

/// Recover the canonical recording stem (= queue job id) from a file name: drop a trailing `.wav`, then a
/// trailing `-clean` (the AEC-output suffix). `20260608-160056-slack-clean.wav` → `20260608-160056-slack`.
fn stem_from_name(name: &str) -> String {
    let no_wav = name.strip_suffix(".wav").unwrap_or(name);
    no_wav.strip_suffix("-clean").unwrap_or(no_wav).to_string()
}

/// Synthesize a [`RecordingMeta`] for a recording that isn't in the queue, from its filename. The
/// `YYYYMMDD-HHMMSS-` prefix gives the start time; the trailing slug gives a humanized app name. `bundle_id`
/// stays `None` (unrecoverable), which also keeps `note_title()` from inventing a `" call"` suffix. A
/// bad/short prefix falls back to "now" so re-do still works on a hand-named file.
fn derive_meta_from_stem(stem: &str, audio: &Path) -> RecordingMeta {
    RecordingMeta {
        started_at: parse_stem_timestamp(stem).unwrap_or_else(Local::now),
        ended_at: None,
        owning_app: owning_app_from_stem(stem),
        audio_path: audio.to_path_buf(),
    }
}

/// Parse the `YYYYMMDD-HHMMSS` prefix of a recording stem into a local datetime. `None` if the prefix isn't
/// a valid timestamp (e.g. a hand-named file).
fn parse_stem_timestamp(stem: &str) -> Option<chrono::DateTime<Local>> {
    // The timestamp prefix is exactly 15 chars: 8 (date) + 1 (`-`) + 6 (time).
    let prefix: String = stem.chars().take(15).collect();
    let naive = NaiveDateTime::parse_from_str(&prefix, "%Y%m%d-%H%M%S").ok()?;
    Local.from_local_datetime(&naive).single()
}

/// The [`OwningApp`] from a recording stem. For a corti stem (timestamp prefix) the app is the trailing
/// slug: `20260608-160056-microsoft-teams` → `Microsoft Teams`. For an arbitrary `--input` filename with no
/// timestamp, the whole stem is humanized (`team-standup` → `Team Standup`). An empty slug falls back to
/// [`OwningApp::unknown`].
fn owning_app_from_stem(stem: &str) -> OwningApp {
    let slug = if parse_stem_timestamp(stem).is_some() {
        stem.get(16..).unwrap_or("") // after the 16-char `YYYYMMDD-HHMMSS-` prefix
    } else {
        stem
    }
    .trim_matches('-');
    if slug.is_empty() {
        OwningApp::unknown()
    } else {
        OwningApp {
            bundle_id: None,
            name: humanize_slug(slug),
        }
    }
}

/// Title-case a dash-separated recording slug: `slack` → `Slack`, `microsoft-teams` → `Microsoft Teams`.
fn humanize_slug(slug: &str) -> String {
    slug.split('-')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Comma-separated list of transcription backends compiled into this build (for error messages).
fn compiled_backends() -> String {
    let mut v = Vec::new();
    if cfg!(feature = "aws") {
        v.push("aws");
    }
    if cfg!(feature = "local") {
        v.push("local");
    }
    if v.is_empty() {
        "none".to_string()
    } else {
        v.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse from a slice of `&str` args (post-binary-name), like `parse_from(env::args().skip(1))`.
    fn p(args: &[&str]) -> Result<Cli, String> {
        parse_from(args.iter().map(|s| (*s).to_string()))
    }

    #[test]
    fn parses_top_level_commands() {
        assert_eq!(p(&[]), Ok(Cli::Run));
        assert_eq!(p(&["--help"]), Ok(Cli::Help));
        assert_eq!(p(&["-h"]), Ok(Cli::Help));
        assert_eq!(p(&["--version"]), Ok(Cli::Version));
        assert_eq!(p(&["-V"]), Ok(Cli::Version));
        assert_eq!(p(&["--list"]), Ok(Cli::List));
        assert!(p(&["--list", "extra"]).is_err());
        assert!(p(&["--bogus"]).is_err());
    }

    #[test]
    fn parses_redo_with_options() {
        assert_eq!(
            p(&["--redo", "rec.wav"]),
            Ok(Cli::Redo(RedoArgs {
                input: "rec.wav".into(),
                backend: None,
                print_only: false,
            }))
        );
        assert_eq!(
            p(&["--redo", "rec.wav", "--local"]),
            Ok(Cli::Redo(RedoArgs {
                input: "rec.wav".into(),
                backend: Some(BackendChoice::Local),
                print_only: false,
            }))
        );
        assert_eq!(
            p(&["--redo", "rec.wav", "--backend", "aws", "--print"]),
            Ok(Cli::Redo(RedoArgs {
                input: "rec.wav".into(),
                backend: Some(BackendChoice::Aws),
                print_only: true,
            }))
        );
        // `--file` flips print_only back off; last write wins.
        assert_eq!(
            p(&["--redo", "rec.wav", "--print", "--file"]),
            Ok(Cli::Redo(RedoArgs {
                input: "rec.wav".into(),
                backend: None,
                print_only: false,
            }))
        );
    }

    #[test]
    fn redo_error_cases() {
        assert!(p(&["--redo"]).is_err()); // missing recording
        assert!(p(&["--redo", "r.wav", "--backend"]).is_err()); // --backend needs a value
        assert!(p(&["--redo", "r.wav", "--backend", "xx"]).is_err()); // bad backend value
        assert!(p(&["--redo", "r.wav", "--nope"]).is_err()); // unknown option
    }

    #[test]
    fn parses_input_with_options() {
        assert_eq!(
            p(&["--input", "rec.wav"]),
            Ok(Cli::Transcribe(TranscribeArgs {
                input: "rec.wav".into(),
                output: None,
                title: None,
                source: None,
                backend: None,
                no_aec: false,
            }))
        );
        assert_eq!(
            p(&[
                "--input",
                "/a/b.wav",
                "-o",
                "/out/note.md",
                "--title",
                "My Call",
                "--source",
                "Slack",
                "--local",
                "--no-aec",
            ]),
            Ok(Cli::Transcribe(TranscribeArgs {
                input: "/a/b.wav".into(),
                output: Some("/out/note.md".into()),
                title: Some("My Call".into()),
                source: Some("Slack".into()),
                backend: Some(BackendChoice::Local),
                no_aec: true,
            }))
        );
        // `--output` long form.
        assert_eq!(
            p(&["--input", "x.wav", "--output", "y.md"]),
            Ok(Cli::Transcribe(TranscribeArgs {
                input: "x.wav".into(),
                output: Some("y.md".into()),
                title: None,
                source: None,
                backend: None,
                no_aec: false,
            }))
        );
    }

    #[test]
    fn input_error_cases() {
        assert!(p(&["--input"]).is_err()); // missing input path
        assert!(p(&["--input", "x.wav", "--output"]).is_err()); // --output needs a value
        assert!(p(&["--input", "x.wav", "--title"]).is_err()); // --title needs a value
        assert!(p(&["--input", "x.wav", "--backend", "xx"]).is_err()); // bad backend
        assert!(p(&["--input", "x.wav", "--nope"]).is_err()); // unknown option
    }

    #[test]
    fn renders_note_like_vagus() {
        let created = Local.with_ymd_and_hms(2026, 6, 8, 16, 0, 56).unwrap();
        let note = render_note(
            "Slack call — 2026-06-08 16:00",
            "Slack · 2026-06-08 16:00",
            "> Auto-captured by corti from Slack.\n\n## Transcript\n\n**[00:00] Me:** hi\n",
            created,
        );
        assert!(note.starts_with(
            "---\ncreated: 2026-06-08T16:00\nstatus: inbox\nsource: Slack · 2026-06-08 16:00\n---\n"
        ));
        assert!(note.contains("\n# Slack call — 2026-06-08 16:00\n"));
        assert!(note.contains("## Transcript\n\n**[00:00] Me:** hi\n"));
    }

    #[test]
    fn stem_strips_clean_and_wav() {
        assert_eq!(
            stem_from_name("20260608-160056-slack.wav"),
            "20260608-160056-slack"
        );
        assert_eq!(
            stem_from_name("20260608-160056-slack-clean.wav"),
            "20260608-160056-slack"
        );
        assert_eq!(
            stem_from_name("20260608-160056-slack"),
            "20260608-160056-slack"
        );
        assert_eq!(
            stem_from_name("20260608-160056-slack-clean"),
            "20260608-160056-slack"
        );
    }

    #[test]
    fn parses_stem_timestamp() {
        let dt = parse_stem_timestamp("20260608-160056-slack").unwrap();
        assert_eq!(
            dt.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2026-06-08 16:00:56"
        );
        assert!(parse_stem_timestamp("not-a-timestamp").is_none());
        assert!(parse_stem_timestamp("short").is_none());
    }

    #[test]
    fn humanizes_app_slug() {
        assert_eq!(owning_app_from_stem("20260608-160056-slack").name, "Slack");
        assert_eq!(
            owning_app_from_stem("20260608-160056-microsoft-teams").name,
            "Microsoft Teams"
        );
        assert!(
            owning_app_from_stem("20260608-160056-slack")
                .bundle_id
                .is_none()
        );
        // Empty slug ⇒ Unknown app.
        assert_eq!(owning_app_from_stem("20260608-160056-").name, "Unknown app");
        // Arbitrary `--input` filename (no timestamp prefix): humanize the whole stem.
        assert_eq!(owning_app_from_stem("team-standup").name, "Team Standup");
        assert_eq!(owning_app_from_stem("meeting").name, "Meeting");
    }

    #[test]
    fn resolves_explicit_path_without_queue() {
        let dir = std::env::temp_dir().join(format!("corti-cli-path-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Raw WAV given as an explicit path: honored verbatim, AEC on, meta synthesized from the name.
        let raw = dir.join("20260608-160056-slack.wav");
        std::fs::write(&raw, b"x").unwrap();
        let r = resolve_recording(raw.to_str().unwrap(), None).unwrap();
        assert_eq!(r.audio, raw);
        assert!(!r.skip_aec);
        assert!(!r.had_row);
        assert_eq!(r.id, "20260608-160056-slack");
        assert_eq!(r.meta.owning_app.name, "Slack");

        // A -clean.wav explicit path ⇒ skip AEC (avoid double-cancel); same stem/id.
        let clean = dir.join("20260608-160056-slack-clean.wav");
        std::fs::write(&clean, b"x").unwrap();
        let rc = resolve_recording(clean.to_str().unwrap(), None).unwrap();
        assert_eq!(rc.id, "20260608-160056-slack");
        assert!(rc.skip_aec);

        // Missing explicit path ⇒ error.
        assert!(resolve_recording(dir.join("nope.wav").to_str().unwrap(), None).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
