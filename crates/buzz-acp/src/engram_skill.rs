//! Write an ENGRAM-composed Agent Skill to disk (Buzz track B2, item 6).
//!
//! ENGRAM's `get_assigned_skill(format="skill_md")` returns a complete Agent
//! Skills directory. Goose re-reads skill files on **every turn** (measured at
//! goose v1.57.0, 2026-11-04), so delivering a skill as a *file* — rather than
//! as prompt text — is what lets a revision take effect inside a live session.
//! That is the whole reason this module exists and why the change doorbells are
//! worth shipping.
//!
//! # Why this file is mostly guards
//!
//! The seam contract (§7.1) says: *"Write `files` as a whole directory,
//! replacing the old one."* On a real agent the skills root holds **five**
//! skills. If "a whole directory" were read as the **root**, one successful
//! fetch would silently delete the other four. ENGRAM pinned the wording after
//! we raised it — `directory` is a single path segment, paths are relative to
//! `<skills_root>/<directory>`, and replacement never touches the root or any
//! sibling — and then asked us to keep asserting it anyway. They were right to:
//! a guarantee checked on both sides is how we found that an earlier draft let
//! `filename` carry a lowercase `skill.md`, which Goose ignores silently.
//!
//! So every guarantee is re-checked here, and **validation completes before
//! anything is written**. A malformed payload must not be able to leave a
//! half-replaced skill on disk.
//!
//! Two failure shapes we have measured in Goose's loader and therefore refuse
//! outright, because each is *silent* if it reaches disk:
//!
//! - a file named `skill.md` is spec-valid elsewhere but invisible to Goose,
//!   which compares the name byte-for-byte against `SKILL.md`;
//! - a stray `SKILL.md` **below** the skill root makes Goose register the inner
//!   directory as its own top-level skill and drops the parent's supporting
//!   files, so `load_skill` answers "has no supporting files" naming no path.

use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

/// The root `SKILL.md`, spelled exactly. Goose's loader compares the file name
/// byte-for-byte, so this is the only accepted spelling.
const ROOT_FILE: &str = "SKILL.md";

/// `skill_md` from `get_assigned_skill(format="skill_md")` (seam contract §7.1).
#[derive(Debug, Clone, Deserialize)]
pub struct SkillMd {
    /// A single path segment, equal to the skill's name. Written at
    /// `<skills_root>/<directory>`.
    pub directory: String,
    /// Whether the skill was split into `references/` parts. Informational
    /// here: `files` is authoritative either way.
    #[serde(default)]
    pub split: bool,
    /// The complete file set: the root `SKILL.md` plus one
    /// `references/<part>.md` per part when split, and nothing else.
    pub files: Vec<SkillFile>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SkillFile {
    /// Relative to `<skills_root>/<directory>`, POSIX-separated.
    pub path: String,
    pub content: String,
}

/// Why a payload was refused, or a write failed. Every variant names the
/// offending value: a refusal that does not say which path was wrong is a
/// refusal the operator cannot act on.
#[derive(Debug, PartialEq, Eq)]
pub enum SkillWriteError {
    /// `directory` is not a single, safe path segment.
    BadDirectory { directory: String, why: &'static str },
    /// A `files[].path` is not a safe relative path.
    BadPath { path: String, why: &'static str },
    /// No entry at the root spelled exactly `SKILL.md`.
    MissingRootSkillMd,
    /// A `SKILL.md` somewhere other than the root: Goose would register the
    /// inner directory as a separate skill and silently strip the parent's
    /// supporting files.
    NestedSkillMd { path: String },
    /// Two entries claim the same path; which one won would be arbitrary.
    DuplicatePath { path: String },
    /// `files` is empty — there is no such thing as a skill with no `SKILL.md`.
    NoFiles,
    /// Filesystem failure, with the operation that failed.
    Io { op: &'static str, path: String, msg: String },
}

impl std::fmt::Display for SkillWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadDirectory { directory, why } => {
                write!(f, "skill_md.directory {directory:?} is unusable: {why}")
            }
            Self::BadPath { path, why } => write!(f, "skill_md file path {path:?} is unusable: {why}"),
            Self::MissingRootSkillMd => write!(
                f,
                "skill_md has no root {ROOT_FILE} (a lowercase `skill.md` is invisible to Goose)"
            ),
            Self::NestedSkillMd { path } => write!(
                f,
                "skill_md contains a nested {ROOT_FILE} at {path:?} — Goose would register that \
                 directory as its own skill and drop this one's supporting files"
            ),
            Self::DuplicatePath { path } => write!(f, "skill_md lists {path:?} twice"),
            Self::NoFiles => write!(f, "skill_md.files is empty"),
            Self::Io { op, path, msg } => write!(f, "{op} {path:?} failed: {msg}"),
        }
    }
}

/// Is this one safe path segment? Rejects separators, traversal and the
/// surprising ones: empty, `.`, `..`, anything with a NUL, and leading or
/// trailing whitespace (which survives a filesystem but not a human reading a
/// directory listing).
fn check_segment(seg: &str) -> Result<(), &'static str> {
    if seg.is_empty() {
        return Err("it is empty");
    }
    if seg == "." || seg == ".." {
        return Err("it is a traversal component");
    }
    if seg.contains('/') || seg.contains('\\') {
        return Err("it contains a path separator");
    }
    if seg.contains('\0') {
        return Err("it contains a NUL byte");
    }
    if seg.trim() != seg {
        return Err("it has leading or trailing whitespace");
    }
    Ok(())
}

/// Build an `Io` error naming the operation and the path. A filesystem failure
/// that does not say which path failed is not actionable.
fn io_err(op: &'static str, path: &Path, e: std::io::Error) -> SkillWriteError {
    SkillWriteError::Io { op, path: path.display().to_string(), msg: e.to_string() }
}

impl SkillMd {
    /// Check every guarantee the contract makes, before anything is written.
    ///
    /// Returns the relative paths in the order they will be written.
    pub fn validate(&self) -> Result<Vec<PathBuf>, SkillWriteError> {
        check_segment(&self.directory).map_err(|why| SkillWriteError::BadDirectory {
            directory: self.directory.clone(),
            why,
        })?;

        if self.files.is_empty() {
            return Err(SkillWriteError::NoFiles);
        }

        let mut rels: Vec<PathBuf> = Vec::with_capacity(self.files.len());
        let mut seen: Vec<String> = Vec::with_capacity(self.files.len());
        let mut has_root = false;

        for entry in &self.files {
            let raw = entry.path.as_str();
            // The contract promises POSIX separators and no leading `/`. A
            // backslash is refused rather than normalised: on Linux it is a
            // legal filename character, so "fixing" it would silently create a
            // file whose name is not the one ENGRAM named.
            if raw.is_empty() {
                return Err(SkillWriteError::BadPath { path: raw.to_string(), why: "it is empty" });
            }
            if raw.starts_with('/') {
                return Err(SkillWriteError::BadPath {
                    path: raw.to_string(),
                    why: "it is absolute",
                });
            }
            if raw.contains('\\') {
                return Err(SkillWriteError::BadPath {
                    path: raw.to_string(),
                    why: "it contains a backslash",
                });
            }

            let rel = Path::new(raw);
            // Reject by COMPONENT, not by substring: a substring test for ".."
            // also rejects a legitimate "notes..md", and misses "a/../b" on
            // some spellings. Components are what the filesystem will act on.
            for comp in rel.components() {
                match comp {
                    Component::Normal(part) => {
                        let part = part.to_string_lossy();
                        check_segment(&part).map_err(|why| SkillWriteError::BadPath {
                            path: raw.to_string(),
                            why,
                        })?;
                    }
                    Component::CurDir => {
                        return Err(SkillWriteError::BadPath {
                            path: raw.to_string(),
                            why: "it contains a `.` component",
                        })
                    }
                    Component::ParentDir => {
                        return Err(SkillWriteError::BadPath {
                            path: raw.to_string(),
                            why: "it contains a `..` component",
                        })
                    }
                    Component::RootDir | Component::Prefix(_) => {
                        return Err(SkillWriteError::BadPath {
                            path: raw.to_string(),
                            why: "it is absolute",
                        })
                    }
                }
            }

            // ⚠️ `Path::components()` NORMALISES: an interior `.` is dropped
            // silently, so the `CurDir` arm above never fires for `a/./b.md`
            // and a component walk alone cannot reject it (measured, 2026-11-04
            // -- the first version of this module shipped that hole and the
            // test caught it). It is not a traversal risk, but it would write
            // `a/b.md` when ENGRAM named `a/./b.md`, which is the same
            // objection as the backslash: the harness must not invent a path.
            //
            // So require NORMAL FORM: the path rebuilt from its components must
            // equal the string as sent. That also catches `a//b.md`, a trailing
            // slash, and anything else where the filesystem would disagree with
            // the literal ENGRAM gave us.
            let rebuilt: PathBuf = rel.components().collect();
            if rebuilt.as_os_str() != rel.as_os_str() {
                return Err(SkillWriteError::BadPath {
                    path: raw.to_string(),
                    why: "it is not in normal form, so the file would land at a path ENGRAM did not name",
                });
            }

            if seen.iter().any(|s| s == raw) {
                return Err(SkillWriteError::DuplicatePath { path: raw.to_string() });
            }
            seen.push(raw.to_string());

            let depth = rel.components().count();
            let base = rel.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if base == ROOT_FILE {
                if depth == 1 {
                    has_root = true;
                } else {
                    return Err(SkillWriteError::NestedSkillMd { path: raw.to_string() });
                }
            }

            rels.push(rel.to_path_buf());
        }

        if !has_root {
            return Err(SkillWriteError::MissingRootSkillMd);
        }
        Ok(rels)
    }

    /// Replace `<skills_root>/<directory>` with this payload, atomically from
    /// the agent's point of view, and touching nothing else in the root.
    ///
    /// Write beside, then swap (the contract's own instruction): a reader never
    /// sees a partially written skill, and a failure part-way leaves the
    /// previous skill in place rather than a mixture of two.
    pub fn write_into(&self, skills_root: &Path) -> Result<PathBuf, SkillWriteError> {
        let rels = self.validate()?;

        let target = skills_root.join(&self.directory);
        // Dot-prefixed so it can never collide with a skill directory: Goose's
        // discovery only collects `SKILL.md`, but a human reading the mount
        // should also see at a glance that these are not skills.
        let staging = skills_root.join(format!(".{}.incoming", &self.directory));
        let retired = skills_root.join(format!(".{}.retired", &self.directory));


        // A leftover staging or retired directory means a previous run died
        // mid-swap. Clearing them is safe precisely because they are ours.
        for scratch in [&staging, &retired] {
            if scratch.exists() {
                fs::remove_dir_all(scratch)
                    .map_err(|e| io_err("clearing stale scratch dir", scratch, e))?;
            }
        }
        fs::create_dir_all(&staging).map_err(|e| io_err("creating staging dir", &staging, e))?;

        for (rel, entry) in rels.iter().zip(&self.files) {
            let dest = staging.join(rel);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(|e| io_err("creating parent dir", parent, e))?;
            }
            fs::write(&dest, &entry.content).map_err(|e| io_err("writing skill file", &dest, e))?;
        }

        // The swap. Two renames rather than one because a rename onto a
        // non-empty directory fails on Unix.
        let had_previous = target.exists();
        if had_previous {
            fs::rename(&target, &retired)
                .map_err(|e| io_err("retiring the previous skill", &target, e))?;
        }
        if let Err(e) = fs::rename(&staging, &target) {
            // Put the previous skill back rather than leaving the agent with no
            // skill at all: a missing skill is silent, and silence is the
            // failure mode this whole track is trying to remove.
            if had_previous {
                let _ = fs::rename(&retired, &target);
            }
            return Err(SkillWriteError::Io {
                op: "installing the new skill",
                path: target.display().to_string(),
                msg: e.to_string(),
            });
        }
        if had_previous {
            // Best-effort: the new skill is already live, so a failure to
            // remove the retired copy must not be reported as a write failure.
            if let Err(e) = fs::remove_dir_all(&retired) {
                tracing::warn!(
                    target: "engram::skill",
                    "installed {} but could not remove {}: {e}",
                    target.display(),
                    retired.display()
                );
            }
        }
        Ok(target)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// B2 item 7 (D20): what the harness DOES about a fetch outcome.
//
// Separated from both the transport and the writer because this is the part
// most likely to be got wrong quietly, and the only part that can be decided
// without any I/O. The trap is finding 3 of §8.1: `no_skill_assigned` and
// `skills_disabled` are RESULTS, not errors. A harness that treats either as a
// failure puts a perfectly healthy agent into degraded mode and announces it in
// a channel -- noise that looks exactly like a real outage.
// ─────────────────────────────────────────────────────────────────────────────

/// `get_assigned_skill`'s answer, reduced to the cases that change behaviour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchOutcome {
    /// `status: "assigned"` with a `skill_md` payload.
    Assigned,
    /// `status: "no_skill_assigned"` — a result. Nothing is wrong.
    NoSkillAssigned,
    /// `status: "skills_disabled"` — the feature is off *on purpose*.
    SkillsDisabled,
    /// Transport failure, or a coded error such as `[project_not_found]`.
    Failed(String),
}

/// What the harness should do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessAction {
    /// Write the directory and carry on.
    InstallSkill,
    /// Start the session normally and say nothing.
    StartNormally,
    /// Try again after a delay; the fetch has not had its chances yet.
    Retry { attempt: u32, delay_secs: u64 },
    /// Start, but announce in-channel that the assigned skill is unavailable
    /// and why, and raise an alert.
    DegradedMode { reason: String },
    /// `require_assigned_skill` is set and we have no skill: take no tasks,
    /// and **do not exit** — D20 is explicit that the agent stays up.
    RefuseTasks { reason: String },
}

/// Exponential backoff, capped. Deliberately small and total: a fetch at
/// session birth must not hold a turn open for minutes.
pub const FETCH_MAX_ATTEMPTS: u32 = 3;

fn backoff_secs(attempt: u32) -> u64 {
    match attempt {
        0 => 1,
        1 => 4,
        _ => 10,
    }
}

/// Decide, given the outcome, the project's `require_assigned_skill`, and how
/// many attempts have already been made.
pub fn decide(outcome: &FetchOutcome, require_assigned_skill: bool, attempts: u32) -> HarnessAction {
    match outcome {
        FetchOutcome::Assigned => HarnessAction::InstallSkill,

        // The feature being off outranks `require_assigned_skill`: requiring an
        // assigned skill in a project where skills are disabled is a
        // contradiction, and resolving it by refusing every task would take the
        // agent down for a setting someone turned off deliberately. Start, and
        // say nothing -- the contract's own instruction.
        FetchOutcome::SkillsDisabled => HarnessAction::StartNormally,

        // A result, not a failure. Nothing to retry and nothing to announce --
        // unless the project insists on a skill, which is what the setting is
        // for.
        FetchOutcome::NoSkillAssigned => {
            if require_assigned_skill {
                HarnessAction::RefuseTasks {
                    reason: "no skill is assigned and this project sets require_assigned_skill".into(),
                }
            } else {
                HarnessAction::StartNormally
            }
        }

        FetchOutcome::Failed(reason) => {
            if attempts < FETCH_MAX_ATTEMPTS {
                HarnessAction::Retry { attempt: attempts, delay_secs: backoff_secs(attempts) }
            } else if require_assigned_skill {
                HarnessAction::RefuseTasks {
                    reason: format!("could not fetch the assigned skill ({reason}); require_assigned_skill is set"),
                }
            } else {
                HarnessAction::DegradedMode { reason: reason.clone() }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, content: &str) -> SkillFile {
        SkillFile { path: path.to_string(), content: content.to_string() }
    }

    fn payload(directory: &str, files: Vec<SkillFile>) -> SkillMd {
        SkillMd { directory: directory.to_string(), split: false, files }
    }

    fn good() -> SkillMd {
        payload(
            "buzz-coder",
            vec![
                file("SKILL.md", "---\nname: buzz-coder\ndescription: d\n---\nbody\n"),
                file("references/part-one.md", "part one\n"),
            ],
        )
    }

    /// A scratch skills root with `n` unrelated sibling skills, so every test
    /// can assert the siblings survived.
    fn root_with_siblings(siblings: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for s in siblings {
            let p = dir.path().join(s);
            fs::create_dir_all(&p).unwrap();
            fs::write(p.join(ROOT_FILE), format!("---\nname: {s}\n---\n")).unwrap();
        }
        dir
    }

    // ── item 7: the decision, where finding 3 would be got wrong ────────────

    #[test]
    fn an_assigned_skill_is_installed() {
        assert_eq!(decide(&FetchOutcome::Assigned, false, 0), HarnessAction::InstallSkill);
        assert_eq!(decide(&FetchOutcome::Assigned, true, 0), HarnessAction::InstallSkill);
    }

    #[test]
    fn no_skill_assigned_is_not_a_failure() {
        // The trap: it is a RESULT. Degrading here would announce an outage in a
        // channel for an agent that is working perfectly.
        assert_eq!(decide(&FetchOutcome::NoSkillAssigned, false, 0), HarnessAction::StartNormally);
    }

    #[test]
    fn skills_disabled_is_silent_even_when_a_skill_is_required() {
        // The feature is off deliberately; refusing every task because of a
        // setting someone switched off would take the agent down for no fault.
        assert_eq!(decide(&FetchOutcome::SkillsDisabled, false, 0), HarnessAction::StartNormally);
        assert_eq!(decide(&FetchOutcome::SkillsDisabled, true, 0), HarnessAction::StartNormally);
    }

    #[test]
    fn require_assigned_skill_refuses_tasks_without_exiting() {
        match decide(&FetchOutcome::NoSkillAssigned, true, 0) {
            HarnessAction::RefuseTasks { reason } => {
                assert!(reason.contains("require_assigned_skill"), "reason must name the setting");
            }
            other => panic!("expected RefuseTasks, got {other:?}"),
        }
    }

    #[test]
    fn a_failure_retries_with_growing_backoff_then_stops() {
        let f = FetchOutcome::Failed("connection refused".into());
        assert_eq!(decide(&f, false, 0), HarnessAction::Retry { attempt: 0, delay_secs: 1 });
        assert_eq!(decide(&f, false, 1), HarnessAction::Retry { attempt: 1, delay_secs: 4 });
        assert_eq!(decide(&f, false, 2), HarnessAction::Retry { attempt: 2, delay_secs: 10 });
        // exhausted
        assert_eq!(
            decide(&f, false, FETCH_MAX_ATTEMPTS),
            HarnessAction::DegradedMode { reason: "connection refused".into() }
        );
    }

    #[test]
    fn an_exhausted_failure_under_require_refuses_rather_than_degrades() {
        let f = FetchOutcome::Failed("[project_not_found]".into());
        match decide(&f, true, FETCH_MAX_ATTEMPTS) {
            HarnessAction::RefuseTasks { reason } => {
                // The operator needs the cause, not just the policy.
                assert!(reason.contains("project_not_found"), "reason must carry the error");
            }
            other => panic!("expected RefuseTasks, got {other:?}"),
        }
    }

    #[test]
    fn degraded_mode_carries_the_reason_for_the_announcement() {
        // D20: the agent says in its channel that its skill is unavailable AND
        // WHY. A reason that does not survive to here cannot be announced.
        let f = FetchOutcome::Failed("engram timed out after 10s".into());
        match decide(&f, false, FETCH_MAX_ATTEMPTS) {
            HarnessAction::DegradedMode { reason } => assert_eq!(reason, "engram timed out after 10s"),
            other => panic!("expected DegradedMode, got {other:?}"),
        }
    }

    // ── validation: the refusals ────────────────────────────────────────────

    #[test]
    fn accepts_the_contract_shape() {
        let rels = good().validate().expect("the documented shape must validate");
        assert_eq!(rels.len(), 2);
    }

    #[test]
    fn refuses_a_directory_that_is_not_one_segment() {
        for bad in ["a/b", "..", ".", "", "a\\b", " lead", "trail "] {
            let p = payload(bad, vec![file("SKILL.md", "x")]);
            match p.validate() {
                Err(SkillWriteError::BadDirectory { .. }) => {}
                other => panic!("directory {bad:?} should be refused, got {other:?}"),
            }
        }
    }

    #[test]
    fn refuses_path_traversal_and_absolutes() {
        for bad in ["../escape.md", "references/../../escape.md", "/etc/passwd", "a/./b.md"] {
            let p = payload("s", vec![file("SKILL.md", "x"), file(bad, "x")]);
            match p.validate() {
                Err(SkillWriteError::BadPath { .. }) => {}
                other => panic!("path {bad:?} should be refused, got {other:?}"),
            }
        }
    }

    #[test]
    fn refuses_a_backslash_rather_than_normalising_it() {
        // On Linux `a\b.md` is one legal filename, so silently "fixing" it
        // would create a file ENGRAM never named.
        let p = payload("s", vec![file("SKILL.md", "x"), file("references\\part.md", "x")]);
        assert!(matches!(p.validate(), Err(SkillWriteError::BadPath { .. })));
    }

    #[test]
    fn refuses_a_lowercase_skill_md_as_the_only_root_file() {
        // Spec-valid elsewhere, invisible to Goose: it would install cleanly
        // and the agent would have no skill.
        let p = payload("s", vec![file("skill.md", "x")]);
        assert_eq!(p.validate(), Err(SkillWriteError::MissingRootSkillMd));
    }

    #[test]
    fn refuses_a_nested_skill_md() {
        // Goose would register `references/` as its own top-level skill and
        // strip this skill's supporting files.
        let p = payload(
            "s",
            vec![file("SKILL.md", "x"), file("references/SKILL.md", "x")],
        );
        assert_eq!(
            p.validate(),
            Err(SkillWriteError::NestedSkillMd { path: "references/SKILL.md".into() })
        );
    }

    #[test]
    fn refuses_duplicate_paths() {
        let p = payload("s", vec![file("SKILL.md", "a"), file("SKILL.md", "b")]);
        assert_eq!(p.validate(), Err(SkillWriteError::DuplicatePath { path: "SKILL.md".into() }));
    }

    #[test]
    fn refuses_an_empty_file_set() {
        assert_eq!(payload("s", vec![]).validate(), Err(SkillWriteError::NoFiles));
    }

    // ── writing ────────────────────────────────────────────────────────────

    #[test]
    fn writes_the_files_and_creates_nested_dirs() {
        let root = root_with_siblings(&[]);
        let target = good().write_into(root.path()).expect("write");
        assert_eq!(target, root.path().join("buzz-coder"));
        assert!(target.join(ROOT_FILE).is_file());
        assert_eq!(fs::read_to_string(target.join("references/part-one.md")).unwrap(), "part one\n");
    }

    #[test]
    fn replacement_removes_stale_files() {
        // "Replace as a whole" means a file that is no longer part of the
        // skill must be GONE. A merge would leave a part the composition
        // dropped, and the agent would keep following it.
        let root = root_with_siblings(&[]);
        good().write_into(root.path()).unwrap();
        let stale = root.path().join("buzz-coder/references/part-one.md");
        assert!(stale.is_file());

        let narrower = payload("buzz-coder", vec![file(ROOT_FILE, "---\nname: x\n---\n")]);
        narrower.write_into(root.path()).unwrap();
        assert!(!stale.exists(), "a dropped part must not survive the replacement");
        assert!(root.path().join("buzz-coder").join(ROOT_FILE).is_file());
    }

    #[test]
    fn siblings_are_untouched() {
        // The finding that made this module mostly guards: on PROD the root
        // holds five skills, and reading "replace the directory" as the ROOT
        // would delete four of them on an ordinary successful fetch.
        let siblings = ["channel-discipline", "fernhill-conventions", "git-workflow", "python-coding"];
        let root = root_with_siblings(&siblings);
        good().write_into(root.path()).unwrap();
        for s in siblings {
            assert!(
                root.path().join(s).join(ROOT_FILE).is_file(),
                "sibling {s} must survive"
            );
        }
        // and nothing else appeared at the root
        let mut names: Vec<String> = fs::read_dir(root.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        let expected: Vec<String> = [
            "buzz-coder", "channel-discipline", "fernhill-conventions", "git-workflow", "python-coding",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(names, expected);
    }

    #[test]
    fn a_refused_payload_writes_nothing_at_all() {
        // Validation runs to completion before any write, so a bad entry
        // cannot leave a half-replaced skill. Here the FIRST entry is fine and
        // the second is poison: a naive loop would already have written one.
        let root = root_with_siblings(&[]);
        let poisoned = payload(
            "buzz-coder",
            vec![file(ROOT_FILE, "good"), file("../escape.md", "bad")],
        );
        assert!(poisoned.write_into(root.path()).is_err());
        assert!(!root.path().join("buzz-coder").exists(), "nothing may be written");
        assert!(!root.path().join("escape.md").exists());
        assert!(!root.path().parent().unwrap().join("escape.md").exists());
    }

    #[test]
    fn a_refused_payload_leaves_the_previous_skill_intact() {
        let root = root_with_siblings(&[]);
        good().write_into(root.path()).unwrap();
        let before = fs::read_to_string(root.path().join("buzz-coder").join(ROOT_FILE)).unwrap();

        let bad = payload("buzz-coder", vec![file("skill.md", "lowercase")]);
        assert!(bad.write_into(root.path()).is_err());

        let after = fs::read_to_string(root.path().join("buzz-coder").join(ROOT_FILE)).unwrap();
        assert_eq!(before, after, "a failed fetch must not degrade a working skill");
    }

    #[test]
    fn a_leftover_staging_dir_from_a_crashed_run_is_cleared() {
        let root = root_with_siblings(&[]);
        let staging = root.path().join(".buzz-coder.incoming");
        fs::create_dir_all(staging.join("references")).unwrap();
        fs::write(staging.join("junk.md"), "from a dead run").unwrap();

        good().write_into(root.path()).unwrap();
        assert!(!staging.exists(), "staging must not survive a successful run");
        assert!(!root.path().join("buzz-coder/junk.md").exists(), "and must not leak into the skill");
    }

    #[test]
    fn scratch_dirs_do_not_look_like_skills() {
        // Dot-prefixed, so a human scanning the mount can tell, and so they can
        // never collide with a skill name ENGRAM chose.
        let root = root_with_siblings(&[]);
        good().write_into(root.path()).unwrap();
        for name in fs::read_dir(root.path()).unwrap() {
            let name = name.unwrap().file_name().to_string_lossy().to_string();
            assert!(!name.starts_with('.'), "left scratch dir {name} behind");
        }
    }

    #[test]
    fn deserialises_the_contract_json() {
        // Field names and the `split` default, straight from seam §7.1.
        let raw = r#"{"directory":"buzz-coder","split":true,
                      "files":[{"path":"SKILL.md","content":"a"},
                               {"path":"references/p.md","content":"b"}]}"#;
        let parsed: SkillMd = serde_json::from_str(raw).expect("contract shape must parse");
        assert_eq!(parsed.directory, "buzz-coder");
        assert!(parsed.split);
        assert_eq!(parsed.files.len(), 2);

        let no_split = r#"{"directory":"d","files":[{"path":"SKILL.md","content":"a"}]}"#;
        let parsed: SkillMd = serde_json::from_str(no_split).unwrap();
        assert!(!parsed.split, "split must default to false");
    }
}
