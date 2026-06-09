//! The one and only vagus touchpoint.
//!
//! corti never writes the iCloud vault or the vagus index directly (that would violate vagus's
//! guardrails). Instead it shells out to the installed `vagus` CLI exactly like vagus's own Claude Code
//! skills do:
//!
//! ```text
//! vagus add-note "<title>" --source "<source>" --print-path  < body-on-stdin
//! ```
//!
//! `--print-path` makes `vagus` skip the editor and print the created note path, which we capture.
//!
//! Discovery order: `$VAGUS_BIN` if set (an explicit override — never falls back), else `vagus` on
//! `PATH`, else well-known install dirs (Homebrew, `~/.cargo/bin`). The fallback dirs exist because a
//! Finder-launched .app inherits launchd's bare `PATH` (`/usr/bin:/bin:/usr/sbin:/sbin`), so a
//! brew-installed `vagus` is invisible via `PATH` and shell exports like `$VAGUS_BIN` never reach a
//! GUI process at all.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use corti_core::{DiarizedTranscript, RecordingMeta};

/// A handle to the `vagus` binary.
#[derive(Debug)]
pub struct Vagus {
    bin: PathBuf,
}

impl Vagus {
    /// Resolve the `vagus` binary: `$VAGUS_BIN` override, else `vagus` on `PATH`, else the well-known
    /// install dirs in [`default_candidates`].
    ///
    /// Verifies it is runnable now so callers fail fast with a clear message rather than at filing time.
    pub fn discover() -> Result<Self> {
        Self::discover_with(
            std::env::var_os("VAGUS_BIN").map(PathBuf::from),
            &default_candidates(),
        )
    }

    /// The resolved binary path: absolute when found via the fallback dirs, the bare name when found on
    /// `PATH` (`PATH` is fixed for the process lifetime, so [`Vagus::add_note`] resolves to the same
    /// binary the probe verified).
    pub fn bin(&self) -> &Path {
        &self.bin
    }

    /// Inner resolution with everything injected, so tests never touch process env or the real `PATH`
    /// (edition-2024 `std::env::set_var` is unsafe and races parallel tests).
    fn discover_with(env_bin: Option<PathBuf>, candidates: &[PathBuf]) -> Result<Self> {
        // $VAGUS_BIN is an explicit user statement — never fall back from it. A wrong value erroring
        // loudly beats silently filing through some other binary the user didn't choose.
        if let Some(bin) = env_bin {
            return match Self::try_candidate(bin.clone())? {
                Some(v) => Ok(v),
                None => bail!(
                    "$VAGUS_BIN is set to `{}` but nothing runnable is there — fix or unset it.",
                    bin.display()
                ),
            };
        }
        for cand in candidates {
            if let Some(v) = Self::try_candidate(cand.clone())? {
                return Ok(v);
            }
        }
        bail!(
            "`vagus` not found ({}). Install it (`brew install vagus`) or set $VAGUS_BIN — the app doesn't see your shell PATH.",
            looked_in(candidates)
        )
    }

    /// Probe one candidate by running `--version`.
    ///
    /// - `Ok(Some(_))` — runnable; the handle stores this exact path so filing later spawns the same
    ///   binary the probe verified.
    /// - `Ok(None)` — nothing there (ENOENT); the caller keeps looking. No `is_file()` pre-check:
    ///   spawning a missing path returns ENOENT in microseconds, avoids a TOCTOU stat/spawn pair, and
    ///   keeps one code path for bare names and absolute candidates.
    /// - `Err` — a binary IS there but is broken (bad exit, not executable, …). Fatal on purpose:
    ///   silently falling through to a *different* vagus could file notes into a different vault.
    fn try_candidate(bin: PathBuf) -> Result<Option<Self>> {
        match Command::new(&bin).arg("--version").output() {
            Ok(out) if out.status.success() => Ok(Some(Self { bin })),
            Ok(out) => bail!(
                "`{} --version` failed (exit {}): {}",
                bin.display(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("running `{}`", bin.display())),
        }
    }

    /// Create a note with `vagus add-note`, piping `body` on stdin. Returns the created note path.
    pub fn add_note(&self, title: &str, source: &str, body: &str) -> Result<PathBuf> {
        let mut child = Command::new(&self.bin)
            .args(["add-note", title, "--source", source, "--print-path"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawning `{} add-note`", self.bin.display()))?;

        child
            .stdin
            .take()
            .context("vagus stdin unavailable")?
            .write_all(body.as_bytes())
            .context("writing note body to vagus stdin")?;

        let out = child.wait_with_output().context("waiting for vagus")?;
        if !out.status.success() {
            bail!(
                "vagus add-note failed (exit {}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if path.is_empty() {
            bail!("vagus add-note printed no path");
        }
        Ok(PathBuf::from(path))
    }

    /// File a completed recording's transcript as a vagus note. Returns the created note path.
    pub fn file_recording(
        &self,
        meta: &RecordingMeta,
        transcript: &DiarizedTranscript,
    ) -> Result<PathBuf> {
        self.add_note(
            &meta.note_title(),
            &meta.source(),
            &recording_body(meta, transcript),
        )
    }
}

/// Candidates tried in order when `$VAGUS_BIN` is unset. First the bare name (normal `PATH` lookup —
/// covers shell launches and custom prefixes on `PATH`), then the standard install locations a
/// Finder-launched .app can't see via launchd's bare `PATH`: Apple Silicon Homebrew, Intel Homebrew
/// (the app ships aarch64-only, but an x86_64 brew prefix still holds a runnable vagus via Rosetta),
/// and `cargo install`.
fn default_candidates() -> Vec<PathBuf> {
    let mut c = vec![
        PathBuf::from("vagus"),
        PathBuf::from("/opt/homebrew/bin/vagus"),
        PathBuf::from("/usr/local/bin/vagus"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        c.push(PathBuf::from(home).join(".cargo/bin/vagus"));
    }
    c
}

/// Render the search list for the not-found error — built from the live candidate list so the message
/// can never drift from what was actually probed. Bare names read as "`x` on PATH", absolute paths
/// verbatim.
fn looked_in(candidates: &[PathBuf]) -> String {
    let parts: Vec<String> = candidates
        .iter()
        .map(|c| {
            if c.is_absolute() {
                c.display().to_string()
            } else {
                format!("`{}` on PATH", c.display())
            }
        })
        .collect();
    format!("checked {}", parts.join(", "))
}

/// Compose the note body: a short auto-capture context line, then the diarized transcript. `pub` so the app
/// can render the same body when it writes a note to an explicit path (the `corti --input --output` CLI)
/// instead of filing through `vagus add-note`.
pub fn recording_body(meta: &RecordingMeta, transcript: &DiarizedTranscript) -> String {
    let mut body = format!(
        "> Auto-captured by corti from {}.\n\n",
        meta.owning_app.name
    );
    body.push_str("## Transcript\n\n");
    body.push_str(&transcript.to_markdown());
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use corti_core::{OwningApp, Speaker, TranscriptSegment};
    use std::path::PathBuf;

    fn meta() -> RecordingMeta {
        RecordingMeta {
            started_at: chrono::Local::now(),
            ended_at: None,
            owning_app: OwningApp::from_bundle_id("us.zoom.xos"),
            audio_path: PathBuf::from("/tmp/x.wav"),
        }
    }

    #[test]
    fn body_includes_context_and_transcript() {
        let t = DiarizedTranscript::new(vec![TranscriptSegment {
            speaker: Speaker::Me,
            start: 0.0,
            end: 1.0,
            text: "hello".into(),
        }]);
        let body = recording_body(&meta(), &t);
        assert!(body.contains("Auto-captured by corti from Zoom."));
        assert!(body.contains("## Transcript"));
        assert!(body.contains("**[00:00] Me:** hello"));
    }

    /// A unique temp dir per test, so parallel tests never share fake binaries.
    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("corti-vagus-discover-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write an executable `#!/bin/sh` script that exits `code`, faking a vagus binary.
    fn fake_bin(dir: &Path, name: &str, exit_code: i32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\nexit {exit_code}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    fn env_bin_wins_over_candidates() {
        let dir = test_dir("env-wins");
        let a = fake_bin(&dir, "vagus-a", 0);
        let b = fake_bin(&dir, "vagus-b", 0);
        let v = Vagus::discover_with(Some(a.clone()), &[b]).unwrap();
        assert_eq!(v.bin(), a.as_path());
    }

    #[test]
    fn env_bin_never_falls_back_when_missing() {
        let dir = test_dir("env-strict");
        let absent = dir.join("absent");
        let working = fake_bin(&dir, "vagus", 0);
        let err = Vagus::discover_with(Some(absent.clone()), &[working])
            .unwrap_err()
            .to_string();
        assert!(err.contains("$VAGUS_BIN"), "got: {err}");
        assert!(err.contains(absent.to_str().unwrap()), "got: {err}");
    }

    #[test]
    fn env_bin_broken_fails_loudly() {
        let dir = test_dir("env-broken");
        let broken = fake_bin(&dir, "vagus", 1);
        let err = Vagus::discover_with(Some(broken), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("--version` failed"), "got: {err}");
    }

    #[test]
    fn skips_missing_candidates_to_first_working() {
        let dir = test_dir("skip-missing");
        let working = fake_bin(&dir, "vagus", 0);
        let candidates = vec![
            PathBuf::from("corti-test-no-such-bin-xyz"), // bare name: PATH miss → continue
            dir.join("absent"),                          // absolute miss → continue
            working.clone(),
        ];
        let v = Vagus::discover_with(None, &candidates).unwrap();
        assert_eq!(v.bin(), working.as_path());
    }

    #[test]
    fn broken_candidate_stops_the_search() {
        let dir = test_dir("broken-stops");
        let broken = fake_bin(&dir, "vagus-broken", 1);
        let working = fake_bin(&dir, "vagus", 0);
        let err = Vagus::discover_with(None, &[broken.clone(), working])
            .unwrap_err()
            .to_string();
        assert!(err.contains(broken.to_str().unwrap()), "got: {err}");
    }

    #[test]
    fn not_found_error_enumerates_everywhere() {
        let dir = test_dir("enumerates");
        let abs_a = dir.join("absent-a");
        let abs_b = dir.join("absent-b");
        let candidates = vec![
            PathBuf::from("corti-test-no-such-bin-xyz"),
            abs_a.clone(),
            abs_b.clone(),
        ];
        let err = Vagus::discover_with(None, &candidates)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("`corti-test-no-such-bin-xyz` on PATH"),
            "got: {err}"
        );
        assert!(err.contains(abs_a.to_str().unwrap()), "got: {err}");
        assert!(err.contains(abs_b.to_str().unwrap()), "got: {err}");
        assert!(err.contains("brew install vagus"), "got: {err}");
        assert!(err.contains("$VAGUS_BIN"), "got: {err}");
        assert!(err.contains("doesn't see your shell PATH"), "got: {err}");
    }

    #[test]
    fn default_candidates_cover_brew_and_cargo() {
        let c = default_candidates();
        assert_eq!(c[0], PathBuf::from("vagus"));
        assert!(c.contains(&PathBuf::from("/opt/homebrew/bin/vagus")));
        assert!(c.contains(&PathBuf::from("/usr/local/bin/vagus")));
        // HOME is always set under `cargo test`.
        assert!(c.last().unwrap().ends_with(".cargo/bin/vagus"));
    }
}
