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
use std::time::Duration;

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
    BadDirectory {
        directory: String,
        why: &'static str,
    },
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
    Io {
        op: &'static str,
        path: String,
        msg: String,
    },
}

impl std::fmt::Display for SkillWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadDirectory { directory, why } => {
                write!(f, "skill_md.directory {directory:?} is unusable: {why}")
            }
            Self::BadPath { path, why } => {
                write!(f, "skill_md file path {path:?} is unusable: {why}")
            }
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
    SkillWriteError::Io {
        op,
        path: path.display().to_string(),
        msg: e.to_string(),
    }
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
                return Err(SkillWriteError::BadPath {
                    path: raw.to_string(),
                    why: "it is empty",
                });
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
                return Err(SkillWriteError::DuplicatePath {
                    path: raw.to_string(),
                });
            }
            seen.push(raw.to_string());

            let depth = rel.components().count();
            let base = rel
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if base == ROOT_FILE {
                if depth == 1 {
                    has_root = true;
                } else {
                    return Err(SkillWriteError::NestedSkillMd {
                        path: raw.to_string(),
                    });
                }
            }

            rels.push(rel.to_path_buf());
        }

        if !has_root {
            return Err(SkillWriteError::MissingRootSkillMd);
        }
        Ok(rels)
    }

    /// The reference parts, in delivery order — every file that is not the
    /// root `SKILL.md`. Empty for a single-file skill.
    pub fn reference_paths(&self) -> Vec<String> {
        self.files
            .iter()
            .map(|f| f.path.as_str())
            .filter(|p| *p != ROOT_FILE)
            .map(|p| p.to_string())
            .collect()
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
///
/// ⚠️ This is the number of **retries**, not of calls. `decide` retries while
/// `attempts < FETCH_MAX_ATTEMPTS`, so a fetch that never succeeds makes
/// `1 + FETCH_MAX_ATTEMPTS` = **4** calls and waits `1 + 4 + 10` = **15
/// seconds** in total. The 15 seconds is the figure that matters: it is why
/// this cannot live in the session-birth hook, whose budget is 3 seconds.
/// `the_ladder_is_four_calls_and_fifteen_seconds` holds both numbers.
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
pub fn decide(
    outcome: &FetchOutcome,
    require_assigned_skill: bool,
    attempts: u32,
) -> HarnessAction {
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
                    reason: "no skill is assigned and this project sets require_assigned_skill"
                        .into(),
                }
            } else {
                HarnessAction::StartNormally
            }
        }

        FetchOutcome::Failed(reason) => {
            if attempts < FETCH_MAX_ATTEMPTS {
                HarnessAction::Retry {
                    attempt: attempts,
                    delay_secs: backoff_secs(attempts),
                }
            } else if require_assigned_skill {
                HarnessAction::RefuseTasks {
                    reason: format!("could not fetch the assigned skill ({reason}); require_assigned_skill is set"),
                }
            } else {
                HarnessAction::DegradedMode {
                    reason: reason.clone(),
                }
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Item 4: whose `require_assigned_skill` wins (seam v1.16 §7.1).
//
// Three sources can have an opinion, and §7.1 orders them: the value in the
// most recent response, then what an earlier response in this process said,
// then the setting the harness holds locally (`ENGRAM_REQUIRE_ASSIGNED_SKILL`).
// The last one applies **only** on a cold start with nothing yet observed.
//
// ⚠️ ONE AMBIGUITY, AND WE CHOSE A READING. §7.1 says the local setting applies
// "only when no response has been seen for this project in this process". A
// response that carries no such field has still been *seen*, so read strictly
// that sentence would make the local setting inert the moment any response
// arrives -- and against ENGRAM v1.20.4, where NO status carries the field, it
// would make `ENGRAM_REQUIRE_ASSIGNED_SKILL` permanently dead on the first
// fetch while looking like it worked. So we resolve on "no VALUE observed"
// rather than "no response seen": absence never clears an explicit operator
// setting, and it never escalates one either. That keeps §7.1's own stated
// reason intact (an unreachable ENGRAM must not by itself make an agent
// refuse) while leaving the operator's switch meaning something on a rollback.
// Raised with ENGRAM Lead; `requirement_field_missing` is what makes the case
// visible in the log rather than inferable.
// ─────────────────────────────────────────────────────────────────────────────

/// §7.1's precedence as one expression: the response, then the cache, then the
/// local setting.
///
/// ⚠️ Deliberately **not** fail-closed. Refusing tasks whenever ENGRAM is
/// unreachable would take every agent down during an outage -- a larger failure
/// than the one the setting guards against, and §7.1 says so explicitly.
pub fn effective_requirement(observed: Option<bool>, cached: Option<bool>, local: bool) -> bool {
    observed.or(cached).unwrap_or(local)
}

/// True when a response on a status that §7.1 says carries
/// `require_assigned_skill` did not carry it.
///
/// ⚠️ This is the rollback detector, and it is the reason the condition is a
/// named function rather than an inline `is_none()`. On ENGRAM v1.20.4 every
/// `assigned` and `no_skill_assigned` takes this branch, and the fallback that
/// follows is then doing load-bearing work that nothing else would report.
/// `Failed` and `SkillsDisabled` are excluded because neither is expected to
/// carry it -- counting those would make the signal useless.
pub fn requirement_field_missing(outcome: &FetchOutcome, observed: Option<bool>) -> bool {
    observed.is_none()
        && matches!(
            outcome,
            FetchOutcome::Assigned | FetchOutcome::NoSkillAssigned
        )
}

/// The per-project cache §7.1 asks for: "the value in the most recent response
/// wins; a harness caches it per project".
///
/// ⚠️ Deliberately **not** a process-wide static. Two reasons, one of them
/// measured: `fetch_with_policy`'s own tests would then write shared state --
/// the trap recorded on `StartupOutcome::Installed`, which cost a day once --
/// and the thing that needs to outlive a single call is the degraded-retry loop
/// in `run_startup_fetch`, which can own it. One process serves one
/// `ENGRAM_PROJECT`, so the map has one entry in practice; it is keyed anyway
/// because the contract is written for a harness that may serve several, and a
/// cache that silently answers for the wrong project is the failure this key
/// prevents.
#[derive(Debug, Default)]
pub struct RequirementCache {
    seen: std::collections::HashMap<String, bool>,
}

impl RequirementCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a value a response actually carried.
    ///
    /// Only ever called with a real value: letting an absent field overwrite
    /// what we were told is the one write that would turn a rollback into a
    /// silent configuration change.
    pub fn observe(&mut self, project: &str, value: bool) {
        self.seen.insert(project.to_string(), value);
    }

    pub fn get(&self, project: &str) -> Option<bool> {
        self.seen.get(project).copied()
    }

    /// Record what this response carried, then resolve the requirement for it.
    pub fn resolve(&mut self, project: &str, observed: Option<bool>, local: bool) -> bool {
        if let Some(value) = observed {
            self.observe(project, value);
        }
        effective_requirement(observed, self.get(project), local)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Item 1: the fetch. `get_assigned_skill(project, format="skill_md")` over
// streamable HTTP, reduced to a `FetchOutcome` plus the payload.
//
// The transport is split from the interpretation on purpose: everything that
// decides *meaning* is a pure function over bytes, tested against payloads
// recorded from the live DEV relay (`tests/fixtures/assigned_*.json`). Only the
// three HTTP calls are untested here, and they are what S6 measures on DEV.
// ─────────────────────────────────────────────────────────────────────────────

/// What a fetch produced: the outcome `decide` reasons about, and the payload
/// when there is one.
#[derive(Debug, Clone)]
pub struct Fetched {
    pub outcome: FetchOutcome,
    /// `Some` only when `outcome` is `Assigned`.
    pub skill_md: Option<SkillMd>,
    /// The project's `require_assigned_skill` **as this response carried it**
    /// (seam v1.16 §7.1).
    ///
    /// ⚠️ `None` is **not** `Some(false)`. It means the response said nothing
    /// about it: `skills_disabled` carries no such field, a server older than
    /// v1.16 carries it on no status at all, and a failed fetch never got a
    /// payload. Collapsing absence to `false` would make a rollback to ENGRAM
    /// v1.20.4 read as "this project does not require a skill", which is a
    /// claim no response made -- and the precedence rule exists precisely to
    /// keep those two cases apart.
    pub require_assigned_skill: Option<bool>,
}

impl Fetched {
    fn failed(reason: impl Into<String>) -> Self {
        Fetched {
            outcome: FetchOutcome::Failed(reason.into()),
            skill_md: None,
            require_assigned_skill: None,
        }
    }

    fn plain(outcome: FetchOutcome) -> Self {
        Fetched {
            outcome,
            skill_md: None,
            require_assigned_skill: None,
        }
    }

    /// A result that carried the v1.16 field (or carried no value for it).
    fn result(outcome: FetchOutcome, require_assigned_skill: Option<bool>) -> Self {
        Fetched {
            outcome,
            skill_md: None,
            require_assigned_skill,
        }
    }
}

/// ENGRAM's tool errors arrive behind this prefix, and the contract says the
/// text after it "starts with a bracketed code and only the code".
const TOOL_ERROR_PREFIX: &str = "Error executing tool get_assigned_skill: ";

/// The bracketed code from a tool error, e.g. `project_not_found`.
///
/// Returns `None` when the text is not in that shape — in which case the whole
/// text is the diagnosis and must be carried verbatim rather than summarised.
pub fn error_code(text: &str) -> Option<&str> {
    let rest = text
        .strip_prefix(TOOL_ERROR_PREFIX)
        .unwrap_or(text)
        .trim_start();
    let rest = rest.strip_prefix('[')?;
    let end = rest.find(']')?;
    let code = &rest[..end];
    if code.is_empty() || code.contains(char::is_whitespace) {
        return None;
    }
    Some(code)
}

/// Pull the JSON-RPC object out of a streamable-HTTP body.
///
/// A streamable-HTTP endpoint may answer with a bare JSON object or with SSE
/// frames; the same request can get either, so both are handled rather than
/// configured.
pub fn extract_envelope(raw: &str) -> Result<serde_json::Value, String> {
    let trimmed = raw.trim_start();
    if trimmed.is_empty() {
        return Err("empty response body".to_string());
    }
    if trimmed.starts_with("event:") || trimmed.starts_with("data:") {
        // Take the first `data:` frame. A JSON-RPC reply to a single request is
        // one frame; anything further belongs to a stream we did not ask for.
        for line in trimmed.lines() {
            if let Some(payload) = line.strip_prefix("data:") {
                let payload = payload.trim();
                if payload.is_empty() {
                    continue;
                }
                return serde_json::from_str(payload)
                    .map_err(|e| format!("SSE frame is not JSON: {e}"));
            }
        }
        return Err("SSE body carried no data frame".to_string());
    }
    serde_json::from_str(trimmed).map_err(|e| format!("response is not JSON: {e}"))
}

/// Interpret the text a successful `tools/call` returned.
///
/// ⚠️ The payload is the tool's JSON **directly** — `{"status": …}`. It is NOT
/// wrapped in `{"result": "<json string>"}`; that wrapper is how some MCP
/// *clients* render a tool result for display, and building this parser from a
/// client's rendering rather than from the wire would look right and never
/// match a real response.
pub fn interpret_payload(text: &str) -> Fetched {
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => return Fetched::failed(format!("tool text is not JSON: {e}")),
    };

    let status = match value.get("status").and_then(|s| s.as_str()) {
        Some(s) => s,
        // No `status` at all is a contract breach, and a silent one: every
        // branch below would otherwise fall through to "nothing assigned",
        // which is the one reading that announces nothing.
        None => return Fetched::failed("payload carried no `status` field"),
    };

    // §7.1 [v1.16]: the field rides on `assigned` and on `no_skill_assigned`,
    // so the harness never needs a second call. Read it here and nowhere else.
    //
    // A present-but-not-boolean value is a contract breach, and a quiet one --
    // `as_bool()` would turn `"true"` into the same `None` as an absent field.
    // It is read as absent (the skill itself is fine) and said out loud.
    let require_assigned_skill = match value.get("require_assigned_skill") {
        None => None,
        Some(v) => match v.as_bool() {
            Some(b) => Some(b),
            None => {
                tracing::warn!(
                    value = %v,
                    "get_assigned_skill: `require_assigned_skill` is not a boolean; \
                     reading it as absent"
                );
                None
            }
        },
    };

    match status {
        "no_skill_assigned" => {
            Fetched::result(FetchOutcome::NoSkillAssigned, require_assigned_skill)
        }
        // Not read on this status: §7.1 gives it no such field, and it outranks
        // the requirement whatever is cached, so a value here could only
        // mislead a later reader into thinking it mattered.
        "skills_disabled" => Fetched::plain(FetchOutcome::SkillsDisabled),
        "assigned" => match value.get("skill_md") {
            Some(raw) => match serde_json::from_value::<SkillMd>(raw.clone()) {
                Ok(skill_md) => Fetched {
                    outcome: FetchOutcome::Assigned,
                    skill_md: Some(skill_md),
                    require_assigned_skill,
                },
                Err(e) => Fetched::failed(format!("`skill_md` did not parse: {e}")),
            },
            // We asked for `format="skill_md"`. Getting `assigned` without it
            // is not a skill we can install, so it must not read as success.
            None => Fetched::failed("status `assigned` carried no `skill_md`"),
        },
        other => Fetched::failed(format!("unknown status `{other}`")),
    }
}

/// Interpret a whole JSON-RPC envelope from a `tools/call`.
pub fn interpret_envelope(envelope: &serde_json::Value) -> Fetched {
    if let Some(err) = envelope.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unspecified JSON-RPC error");
        return Fetched::failed(format!("JSON-RPC error: {msg}"));
    }

    let result = match envelope.get("result") {
        Some(r) => r,
        None => return Fetched::failed("envelope carried neither `result` nor `error`"),
    };

    let text = result
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|blocks| {
            blocks.iter().find_map(|b| {
                if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                    b.get("text").and_then(|t| t.as_str())
                } else {
                    None
                }
            })
        });

    let text = match text {
        Some(t) => t,
        None => return Fetched::failed("tool result carried no text block"),
    };

    // A tool error is reported in the result, not as a JSON-RPC error. Read the
    // bracketed code when there is one and carry the text when there is not.
    let is_error = result
        .get("isError")
        .or_else(|| result.get("is_error"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if is_error {
        return match error_code(text) {
            Some(code) => Fetched::failed(format!("[{code}]")),
            None => Fetched::failed(text.to_string()),
        };
    }

    interpret_payload(text)
}

/// The three HTTP calls: `initialize`, `notifications/initialized`,
/// `tools/call`. One short-lived MCP session per fetch — a session held open
/// across turns would be state we would have to reason about on reconnect, and
/// the fetch happens once per process start.
pub struct EngramSkillClient {
    url: String,
    pat: String,
    http: reqwest::Client,
}

impl EngramSkillClient {
    /// `url` is ENGRAM's MCP endpoint (`ENGRAM_MCP_URL`), `pat` the agent's own
    /// token (`ENGRAM_PAT`). The PAT is held here and never logged: it is a
    /// credential, and a credential in a log line is a credential to rotate.
    pub fn new(
        url: impl Into<String>,
        pat: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| format!("could not build HTTP client: {e}"))?;
        Ok(Self {
            url: url.into(),
            pat: pat.into(),
            http,
        })
    }

    async fn post(&self, body: String, session: &mut Option<String>) -> Result<String, String> {
        let mut req = self
            .http
            .post(&self.url)
            .header("Authorization", format!("Bearer {}", self.pat))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(body);
        if let Some(id) = session.as_deref() {
            req = req.header("Mcp-Session-Id", id);
        }

        let resp = req.send().await.map_err(|e| {
            // `reqwest`'s Display includes the URL but never the headers, so
            // this cannot leak the PAT.
            format!("transport: {e}")
        })?;

        if let Some(id) = resp
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|v| v.to_str().ok())
        {
            *session = Some(id.to_string());
        }

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| format!("could not read body: {e}"))?;
        if !status.is_success() {
            // Carry the code AND a bounded slice of the body: a bare 401 does
            // not distinguish a wrong PAT from a revoked one.
            let snippet: String = text.chars().take(200).collect();
            return Err(format!("HTTP {status}: {snippet}"));
        }
        Ok(text)
    }

    /// Fetch this agent's assigned skill for `project`.
    ///
    /// Never returns `Err`: every failure is a `FetchOutcome::Failed` carrying
    /// its reason, because the caller's next step is `decide`, and a `Result`
    /// at this boundary invites a `?` that would abort session start — the one
    /// thing D20 says must not happen.
    pub async fn fetch(&self, project: &str) -> Fetched {
        let mut session: Option<String> = None;

        let init = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "buzz-acp", "version": env!("CARGO_PKG_VERSION") }
            }
        });
        if let Err(e) = self.post(init.to_string(), &mut session).await {
            return Fetched::failed(format!("initialize failed: {e}"));
        }

        let ready = serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        if let Err(e) = self.post(ready.to_string(), &mut session).await {
            return Fetched::failed(format!("initialized notification failed: {e}"));
        }

        let call = serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {
                "name": "get_assigned_skill",
                // The project is explicit. Finding 2 of the review: an ambient
                // call returns `[project_not_found]` as soon as the agent can
                // read more than one project, and that error reads like a
                // permissions fault rather than a missing argument.
                "arguments": { "project": project, "format": "skill_md" }
            }
        });
        let raw = match self.post(call.to_string(), &mut session).await {
            Ok(r) => r,
            Err(e) => return Fetched::failed(format!("tools/call failed: {e}")),
        };

        match extract_envelope(&raw) {
            Ok(envelope) => interpret_envelope(&envelope),
            Err(e) => Fetched::failed(e),
        }
    }
}

/// The protocol version our handshake declares. Pinned rather than derived: a
/// version we have not called ENGRAM with is a version we have not tested.
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

// ─────────────────────────────────────────────────────────────────────────────
// Item 3 + item 7: the wiring and the policy around it.
//
// This runs at PROCESS START, not at session birth. The reason is measured, not
// stylistic: the session-birth hook in `pool.rs` has a 3-second budget and
// fails open ("we'd rather start the session with no core hint than block
// session creation on a stalled relay"), and item 7's backoff ladder is 15
// seconds, so it cannot live there. And it does not need to: Goose re-reads
// skill files every turn (review §9), so the WRITE is not bound to a session's
// lifetime at all.
// ─────────────────────────────────────────────────────────────────────────────

/// Everything the fetch needs, read from the environment once.
#[derive(Debug, Clone)]
pub struct SkillFetchConfig {
    pub url: String,
    /// The agent's own ENGRAM token. Never logged, never put on a command line.
    pat: String,
    /// Explicit, never ambient — see `fetch`.
    pub project: String,
    /// The directory Goose scans. `<skills_root>/<skill name>` is replaced; the
    /// root itself is never touched.
    pub skills_root: PathBuf,
    /// Mirrors the project's own `require_assigned_skill`. Supplied by config
    /// rather than read from the payload so the agent's behaviour is decidable
    /// without a successful fetch — which is exactly the case it governs.
    pub require_assigned_skill: bool,
}

impl SkillFetchConfig {
    /// `Ok(None)` means the switch is off — start normally and say nothing.
    ///
    /// `Err` means the switch is ON and the configuration is incomplete. That
    /// is reported loudly and the agent still starts: an operator's typo must
    /// not take an agent down, and must not pass unnoticed either.
    pub fn from_env() -> Result<Option<Self>, String> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// `from_env` over an injected lookup, so every branch is testable without
    /// mutating the process environment — which is global, and which parallel
    /// test threads would race on.
    pub fn from_lookup<F>(get: F) -> Result<Option<Self>, String>
    where
        F: Fn(&str) -> Option<String>,
    {
        let flag = |name: &str| -> bool {
            matches!(
                get(name)
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
                    .as_str(),
                "1" | "true" | "yes" | "on"
            )
        };
        let text = |name: &str| -> Option<String> {
            get(name)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };

        if !flag("ENGRAM_SKILL_FETCH") {
            return Ok(None);
        }
        let url = text("ENGRAM_MCP_URL")
            .ok_or("ENGRAM_SKILL_FETCH is on but ENGRAM_MCP_URL is unset or empty")?;
        let pat = text("ENGRAM_PAT")
            .ok_or("ENGRAM_SKILL_FETCH is on but ENGRAM_PAT is unset or empty")?;
        let project = text("ENGRAM_PROJECT").ok_or(
            "ENGRAM_SKILL_FETCH is on but ENGRAM_PROJECT is unset or empty; an ambient call \
             returns [project_not_found] as soon as the agent can read more than one project",
        )?;
        let skills_root = match text("ENGRAM_SKILL_ROOT") {
            Some(p) => PathBuf::from(p),
            None => PathBuf::from(text("HOME").ok_or(
                "neither ENGRAM_SKILL_ROOT nor HOME is set, so the skills root cannot be resolved",
            )?)
            .join(".agents")
            .join("skills"),
        };
        Ok(Some(SkillFetchConfig {
            url,
            pat,
            project,
            skills_root,
            require_assigned_skill: flag("ENGRAM_REQUIRE_ASSIGNED_SKILL"),
        }))
    }

    pub fn client(&self, timeout: Duration) -> Result<EngramSkillClient, String> {
        EngramSkillClient::new(self.url.clone(), self.pat.clone(), timeout)
    }
}

/// How far a single HTTP call may run. Generous next to the session-birth hook
/// because nothing is waiting on it: this is process start, off the turn path.
pub const FETCH_HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// While degraded, keep trying at this cadence. This is item 7's "refetch when
/// ENGRAM returns": without it a relay that was down for one minute leaves the
/// agent on a stale skill until someone restarts it.
pub const DEGRADED_RETRY_INTERVAL: Duration = Duration::from_secs(300);

/// What the startup fetch settled on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupOutcome {
    /// Written. Carries the directory path for the log line and the record the
    /// session bootstrap needs.
    ///
    /// ⚠️ This RETURNS the record rather than storing it. Storing it here made
    /// `fetch_with_policy`'s own tests write a process-wide slot that altered an
    /// unrelated test's expected system prompt
    /// (`pool::model_switch_tests::session_new_sends_policy_specific_base_and_scope_specific_title`).
    /// The global write now happens only in `run_startup_fetch`, which no test
    /// calls, so no tested path can leak into another test.
    Installed {
        path: PathBuf,
        skill: InstalledSkill,
    },
    /// Nothing to do, and nothing to say.
    StartedNormally,
    /// Up, but without the skill it was supposed to have. Announce and alert.
    Degraded { reason: String },
    /// `require_assigned_skill` with no skill: take no tasks, stay up.
    RefuseTasks { reason: String },
}

/// Run the fetch under item 7's policy: up to `FETCH_MAX_ATTEMPTS` tries with
/// the backoff ladder, then install, start quietly, degrade, or refuse.
///
/// Generic over the fetch so the ladder is tested against every branch without
/// a network, with tokio's clock paused.
pub async fn fetch_with_policy<F, Fut>(
    cfg: &SkillFetchConfig,
    requirements: &mut RequirementCache,
    mut fetch: F,
) -> StartupOutcome
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Fetched>,
{
    let mut attempts = 0;
    loop {
        let fetched = fetch().await;

        // Item 4: the requirement is the project's, not the environment's --
        // `cfg.require_assigned_skill` is now only the last resort.
        if requirement_field_missing(&fetched.outcome, fetched.require_assigned_skill) {
            tracing::warn!(
                project = %cfg.project,
                outcome = ?fetched.outcome,
                "engram skill fetch: response carried no `require_assigned_skill`, which seam \
                 v1.16 §7.1 says this status carries; falling back to the cache, then to \
                 ENGRAM_REQUIRE_ASSIGNED_SKILL"
            );
        }
        let require_assigned_skill = requirements.resolve(
            &cfg.project,
            fetched.require_assigned_skill,
            cfg.require_assigned_skill,
        );

        match decide(&fetched.outcome, require_assigned_skill, attempts) {
            HarnessAction::InstallSkill => {
                let Some(skill) = fetched.skill_md else {
                    // `Assigned` without a payload is refused by
                    // `interpret_payload`, so this is unreachable through the
                    // real client. Degrade rather than unwrap: a panic here
                    // takes down an agent at startup.
                    return StartupOutcome::Degraded {
                        reason: "ENGRAM reported a skill but sent no files".to_string(),
                    };
                };
                return match skill.write_into(&cfg.skills_root) {
                    // Built only after the write succeeded: an instruction to
                    // load a skill that is not on disk sends the agent to a
                    // tool call that fails.
                    Ok(path) => StartupOutcome::Installed {
                        path,
                        skill: InstalledSkill {
                            directory: skill.directory.clone(),
                            references: skill.reference_paths(),
                        },
                    },
                    // A write failure is NOT retried: the payload was fine and
                    // the filesystem is not going to change its mind within
                    // the ladder. The reason names the offending path.
                    Err(e) => StartupOutcome::Degraded {
                        reason: e.to_string(),
                    },
                };
            }
            HarnessAction::StartNormally => return StartupOutcome::StartedNormally,
            HarnessAction::Retry { delay_secs, .. } => {
                attempts += 1;
                tokio::time::sleep(Duration::from_secs(delay_secs)).await;
            }
            HarnessAction::DegradedMode { reason } => return StartupOutcome::Degraded { reason },
            HarnessAction::RefuseTasks { reason } => return StartupOutcome::RefuseTasks { reason },
        }
    }
}

/// Record what an outcome means for the process-wide state, and say so.
///
/// ⚠️ **This is one function because the retry loop used to clear the marker
/// only on `Installed`.** A retry that returned `StartedNormally` -- ENGRAM
/// reachable again with nothing assigned, or the project's requirement turned
/// off -- left the loop with the trouble marker still set. Under item 7 that
/// cost a room its recovery line; under item 5 it is an agent that **refuses
/// tasks forever after the condition has passed**, and it is the exact
/// transition acceptance check 3 measures ("flipping the project setting
/// changes behaviour with no container restart"). Found by reading the loop
/// while building the gate, not by a test -- there was no test that could see
/// it, because nothing consumed the marker.
fn apply_outcome(outcome: &StartupOutcome, retry: bool) {
    match outcome {
        StartupOutcome::Installed { path, skill } => {
            record_installed(skill.clone());
            // Clears the marker, so the next wake in a channel that was told
            // carries the recovery line rather than silence.
            clear_trouble();
            tracing::info!(
                path = %path.display(),
                parts = skill.references.len(),
                "engram skill fetch: {}",
                if retry { "recovered and installed" } else { "installed" }
            )
        }
        StartupOutcome::StartedNormally => {
            // Includes `no_skill_assigned`, which is a RESULT and never degraded.
            clear_trouble();
            tracing::info!(
                recovered = retry,
                "engram skill fetch: nothing assigned, starting normally"
            )
        }
        StartupOutcome::Degraded { reason } => {
            record_trouble(SkillTrouble::Degraded {
                reason: reason.clone(),
            });
            tracing::error!(reason = %reason, "engram skill fetch: DEGRADED")
        }
        StartupOutcome::RefuseTasks { reason } => {
            record_trouble(SkillTrouble::Refusing {
                reason: reason.clone(),
            });
            tracing::error!(reason = %reason, "engram skill fetch: refusing tasks")
        }
    }
}

/// The whole startup path: read the config, fetch, write, and log what happened.
///
/// Spawned, never awaited by startup. Returns the outcome so a caller that has
/// a channel can make the item 7 announcement; the process-start site has no
/// channel, which is why this logs rather than posts.
pub async fn run_startup_fetch() -> Option<StartupOutcome> {
    let cfg = match SkillFetchConfig::from_env() {
        Ok(Some(cfg)) => cfg,
        // Off. The contract's own instruction for a deliberately disabled
        // feature: start normally, announce nothing.
        Ok(None) => return None,
        Err(why) => {
            tracing::error!("engram skill fetch is enabled but misconfigured: {why}");
            return Some(StartupOutcome::Degraded { reason: why });
        }
    };

    let client = match cfg.client(FETCH_HTTP_TIMEOUT) {
        Ok(c) => c,
        Err(why) => {
            tracing::error!("engram skill fetch: {why}");
            return Some(StartupOutcome::Degraded { reason: why });
        }
    };

    mark_fetch_started();

    // The URL and project are logged; the PAT is not, and there is no branch
    // here that could put it in a line.
    tracing::info!(
        url = %cfg.url,
        project = %cfg.project,
        skills_root = %cfg.skills_root.display(),
        "engram skill fetch: starting"
    );

    // Owned here rather than inside `fetch_with_policy`, so that what a
    // response said survives the degraded-retry loop below. Without that, a
    // retry after an outage is a "cold start" by the contract's own wording and
    // the local setting comes back -- see the item 4 banner.
    let mut requirements = RequirementCache::new();

    let mut outcome =
        fetch_with_policy(&cfg, &mut requirements, || client.fetch(&cfg.project)).await;

    apply_outcome(&outcome, false);

    // Item 7's refetch: while degraded, keep trying. A relay that was briefly
    // down otherwise leaves the agent on a stale skill until a restart.
    while matches!(
        outcome,
        StartupOutcome::Degraded { .. } | StartupOutcome::RefuseTasks { .. }
    ) {
        tokio::time::sleep(DEGRADED_RETRY_INTERVAL).await;
        tracing::info!("engram skill fetch: retrying after degradation");
        outcome = fetch_with_policy(&cfg, &mut requirements, || client.fetch(&cfg.project)).await;
        apply_outcome(&outcome, true);
    }

    Some(outcome)
}

// ─────────────────────────────────────────────────────────────────────────────
// The session bootstrap.
//
// Added to this release on 2026-11-04 by Igor's decision, and it is the part
// that makes the rest of it matter. A skill's BODY is never in the system
// prompt -- only its name and description -- so the body arrives if and only if
// the model calls `load_skill`. Measured on DEV: the rule in a split skill
// arrived in 1 of 4 runs unprompted, and in the runs where it did not, the
// agent recommended the exact action its skill forbids. With this instruction
// in the system prompt: 6 of 6, every reference loaded.
//
// So the writer putting a file on disk is necessary and NOT sufficient, and
// that is why this lives beside it rather than in a follow-up.
// ─────────────────────────────────────────────────────────────────────────────

/// What the startup fetch installed, kept for the session bootstrap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledSkill {
    /// The skill's directory name, which is also the name `load_skill` takes.
    pub directory: String,
    /// Its reference parts, relative to the skill directory. Empty when the
    /// skill is a single file — in which case one `load_skill` gets everything.
    pub references: Vec<String>,
}

/// Process-wide, because the fetch happens once at process start and every
/// session afterwards needs to know what it found. `None` means no fetch has
/// installed anything — the switch is off, or it failed — and the bootstrap is
/// then absent rather than empty: an instruction to load a skill that is not
/// there is worse than no instruction.
static INSTALLED: std::sync::OnceLock<std::sync::RwLock<Option<InstalledSkill>>> =
    std::sync::OnceLock::new();

fn installed_slot() -> &'static std::sync::RwLock<Option<InstalledSkill>> {
    INSTALLED.get_or_init(|| std::sync::RwLock::new(None))
}

pub fn record_installed(skill: InstalledSkill) {
    if let Ok(mut slot) = installed_slot().write() {
        *slot = Some(skill);
    }
}

pub fn installed_skill() -> Option<InstalledSkill> {
    installed_slot().read().ok().and_then(|slot| slot.clone())
}

/// Whether a fetch is enabled and has not yet recorded a result.
///
/// ⚠️ This exists because of a window measured on DEV, not imagined: the agent
/// reached `agent_pool_ready` and `presence set to online` **4.2 seconds
/// before** `engram skill fetch: installed`. The fetch is spawned and never
/// awaited, which is the design and is why startup is not blocked — but it
/// means a session born inside that window composes its system prompt with no
/// `<assigned-skill>` section, and the bootstrap is composed **once** at
/// session birth.
///
/// ⚠⚠ **The 4.2 s is DEV's, and PROD measured it SMALLER and sometimes
/// NEGATIVE** (2026-11-06, one bring-up of all three agents): the skill landed
/// **524 ms after** the coder came online, and **131 ms and 164 ms BEFORE** the
/// researcher and the orchestrator did. So this is a **race between the fetch
/// and the relay connect sequence with no ordering guarantee**, decided by
/// whether the fetch takes ~160 ms or ~850 ms — not a window of known width or
/// even of known sign. Two consequences: the exposure can be **zero** for an
/// agent whose fetch wins, and the counter below will therefore fire far less
/// on PROD than on DEV, so a low count there is not evidence the window closed.
///
/// The exposure is narrower than it looks: Goose re-reads skill *files* every
/// turn, so such a session still gets the files from its next turn. What it
/// misses for its whole life is the *instruction to load them* — which is
/// exactly the condition measured at 1-in-4.
///
/// So this is instrumentation first. Whether it is worth closing depends on
/// whether it ever fires in practice, and the honest way to find that out is
/// to count it rather than to reason about it.
pub fn fetch_pending() -> bool {
    FETCH_STARTED.load(std::sync::atomic::Ordering::Relaxed)
        && installed_slot()
            .read()
            .map(|slot| slot.is_none())
            .unwrap_or(false)
}

/// Set once the switch is known to be on and a fetch is under way.
static FETCH_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn mark_fetch_started() {
    FETCH_STARTED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// The bootstrap text for the current session, or `None` when nothing is
/// installed.
///
/// ⚠️ The wording is load-bearing and was measured. Two things it must keep:
/// it must say **"and every reference it lists"**, because `load_skill("<skill>")`
/// on a split skill returns only the index; and it must give the explicit
/// `"<skill>/references/<part>.md"` form, because the index alone did not get
/// the parts loaded. Changing this string means re-running the measurement.
pub fn bootstrap_instruction() -> Option<String> {
    bootstrap_text(installed_skill().as_ref())
}

/// `bootstrap_instruction` over an explicit value, so it is testable without
/// the process-wide slot.
pub fn bootstrap_text(skill: Option<&InstalledSkill>) -> Option<String> {
    let skill = skill?;
    let dir = &skill.directory;
    if skill.references.is_empty() {
        // One file: a single load gets the whole skill, and mentioning
        // references that do not exist would invite a call that fails.
        return Some(format!(
            "Before you do anything else, load the skill you have been assigned and follow it:\n\
             - load_skill(name: \"{dir}\")"
        ));
    }
    let mut out = format!(
        "Before you do anything else: load your assigned skill **and every reference it lists**. \
         A split skill's SKILL.md is only an index — each part needs its own \
         load_skill(name: \"<skill>/references/<part>.md\"). Follow the rules in all of them.\n\n\
         Your assigned skill is `{dir}`, in {n} parts. Load each of these before you begin:\n\
         - load_skill(name: \"{dir}\")",
        n = skill.references.len()
    );
    for reference in &skill.references {
        out.push_str(&format!("\n- load_skill(name: \"{dir}/{reference}\")"));
    }
    Some(out)
}

// ── Option A: the degraded-mode announcement (item 7, §7b) ─────────────────
//
// D20 asks for *"a marked degraded mode — an in-channel announcement that the
// assigned skill is unavailable and why"*. The obstacle is structural: the fetch
// runs at **process start, and process start has no channel.** The agent has not
// joined a session and does not know which rooms it will be addressed in, so
// announcing at the moment of failure means announcing nowhere.
//
// Igor chose **option A with option B's phrasing** (2026-11-05): announce **once
// per channel**, the first time the agent is woken there while degraded, with
// wording that says both what is degraded *and* that work continues.
//
// Four guards, each for a mistake that is easy to make here:
//
// 1. **Never on `no_skill_assigned`.** That is a *result*, not a failure — DEV
//    returns it today — and announcing it would put a healthy agent into a
//    visible degraded mode (§8.1 finding 3). This falls out of keying the
//    announcement on an explicit degraded marker rather than on "no skill
//    installed", which is also true when nothing was ever assigned.
// 2. **Not during the startup window**, however wide it turns out to be. The
//    same marker handles it: while the fetch is in flight it has neither
//    succeeded nor exhausted its retries, so the marker is unset and nothing is
//    said. Note `fetch_pending()` could NOT be used as the discriminator — it
//    is true both in flight and after failure.
//
//    ⚠️ **Do not re-derive this guard from a gap of 4.2 s.** That figure is
//    DEV's; PROD measured **+524 ms, −131 ms and −164 ms** across one bring-up
//    of the three agents (2026-11-06), so the skill can land *before* the agent
//    is online and the gap has no guaranteed sign. **This guard is correct
//    because it keys on the marker rather than on a time window** — which is
//    precisely what makes it survive a measurement that moved.
// 3. **Once per channel, not once per session.** `session_policy=channel` makes
//    those nearly the same; the flag makes it exact across session recycling.
// 4. **No mentions, and no thread.** The relay parses `@Name` out of the message
//    **body** into `p` tags, and a `p` tag *wakes* its subject — so an
//    announcement that named the room would spend a real model turn per member
//    to tell them something they never asked about. It is a top-level post, not
//    a reply, so it carries no thread tags either.
//
// One addition beyond the three options as written: **when the refetch loop
// recovers, the channels that were told are told once more.** Without it, a room
// that saw the announcement believes the agent is degraded forever — which §7b's
// table listed as option A's cost. This is the answer to that cost.

/// What is wrong, and whether it stops the agent working.
///
/// ⚠️ The two are **not** interchangeable, and conflating them was already
/// shipping a false statement: a `Degraded` agent answers from its general
/// instructions, a `Refusing` one takes no tasks at all (D20). Item 7 recorded
/// both as a bare reason string, so a refusing agent announced
/// *"I will answer from my general instructions"* and would then -- once item 5
/// gated the turn -- answer nothing. The room would have been told the one
/// thing that was not true.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillTrouble {
    /// Up without the assigned skill. Answers, worse.
    Degraded { reason: String },
    /// `require_assigned_skill` with no skill: takes no tasks, stays up.
    Refusing { reason: String },
}

impl SkillTrouble {
    pub fn reason(&self) -> &str {
        match self {
            SkillTrouble::Degraded { reason } | SkillTrouble::Refusing { reason } => reason,
        }
    }

    /// Which **kind** this is, for the per-channel record of what a room was
    /// told. Compared rather than the reason, because a reason that changes
    /// between retries is not news and a kind that changes always is.
    pub fn kind(&self) -> TroubleKind {
        match self {
            SkillTrouble::Degraded { .. } => TroubleKind::Degraded,
            SkillTrouble::Refusing { .. } => TroubleKind::Refusing,
        }
    }
}

/// `SkillTrouble` without its reason: what a channel has been told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TroubleKind {
    Degraded,
    Refusing,
}

/// Set while the fetch has failed its last retry, or while the project requires
/// a skill the agent does not have; cleared by any later fetch that resolves.
/// `None` means "no trouble", which is **not** the same as "no skill
/// installed" — see guard 1.
static TROUBLE: std::sync::OnceLock<std::sync::RwLock<Option<SkillTrouble>>> =
    std::sync::OnceLock::new();

fn trouble_slot() -> &'static std::sync::RwLock<Option<SkillTrouble>> {
    TROUBLE.get_or_init(|| std::sync::RwLock::new(None))
}

/// Channels that have been told, and **what** they were told, so that a
/// Degraded → Refusing transition is announced rather than swallowed by "this
/// room already knows".
static TOLD: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, TroubleKind>>> =
    std::sync::OnceLock::new();

fn told_slot() -> &'static std::sync::Mutex<std::collections::HashMap<String, TroubleKind>> {
    TOLD.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

pub fn record_trouble(trouble: SkillTrouble) {
    if let Ok(mut slot) = trouble_slot().write() {
        *slot = Some(trouble);
    }
}

pub fn clear_trouble() {
    if let Ok(mut slot) = trouble_slot().write() {
        *slot = None;
    }
}

pub fn trouble() -> Option<SkillTrouble> {
    trouble_slot().read().ok().and_then(|slot| slot.clone())
}

/// A lock for any test that drives this module's process-wide slots.
///
/// ⚠️ `pub(crate)` on purpose. The slots are process-wide, and `lib.rs` has a
/// test for item 5's gate that sets them too. A lock private to one test
/// module serialises that module and nothing else — which looks like
/// serialisation and is not, and is the exact failure already recorded on
/// `announcement_tests`.
#[cfg(test)]
pub(crate) static STATE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Takes the lock without caring whether a previous panic poisoned it: a
/// failing test should report its own assertion, not drag the next one down
/// with a poison error that hides it.
#[cfg(test)]
pub(crate) fn state_test_guard() -> std::sync::MutexGuard<'static, ()> {
    STATE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Item 5's gate, read on the inbound path: `Some(reason)` means take no tasks.
///
/// One uncontended lock read per accepted event, which is the budget the
/// inbound path already sets for `announcement_possible`. It is deliberately
/// **not** nested inside that check even though `announcement_possible` is
/// true whenever this is: the gate would then stop firing if that predicate's
/// definition ever changed, and nothing would report it.
pub fn refusing_reason() -> Option<String> {
    match trouble() {
        Some(SkillTrouble::Refusing { reason }) => Some(reason),
        _ => None,
    }
}

/// What, if anything, to say in a channel right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Announcement {
    /// First wake in this channel while degraded.
    Degraded { reason: String },
    /// First wake in this channel while refusing tasks (item 5 / D20).
    Refusing { reason: String },
    /// The trouble passed after this channel had been told about it.
    Recovered,
}

/// The policy, over explicit inputs so every branch is testable without the
/// process-wide slots.
///
/// `told` is **what** this room was told, not merely whether it was told
/// something: a room told "degraded" and then silently switched to refusing
/// would be holding a statement that is no longer true.
pub fn decide_announcement(
    trouble: Option<&SkillTrouble>,
    told: Option<TroubleKind>,
) -> Option<Announcement> {
    match (trouble, told) {
        // In trouble and this room does not know yet.
        (Some(t), None) => Some(announcement_of(t)),
        // In trouble, and the room was told something else. The kind changed,
        // which is news; the reason changing is not.
        (Some(t), Some(k)) if t.kind() != k => Some(announcement_of(t)),
        // In trouble and already said so: silence. Guard 3.
        (Some(_), Some(_)) => None,
        // Resolved, and this room was told. Close the loop.
        (None, Some(_)) => Some(Announcement::Recovered),
        // Healthy, nothing said: the overwhelmingly common case, and it must
        // cost nothing. Guards 1 and 2 both land here.
        (None, None) => None,
    }
}

fn announcement_of(t: &SkillTrouble) -> Announcement {
    match t {
        SkillTrouble::Degraded { reason } => Announcement::Degraded {
            reason: reason.clone(),
        },
        SkillTrouble::Refusing { reason } => Announcement::Refusing {
            reason: reason.clone(),
        },
    }
}

/// A cheap pre-check for the inbound path, which runs on **every accepted
/// event**. The steady state is a healthy agent with nothing to say, and that
/// case must cost two lock reads and no task — not a spawned future per event.
pub fn announcement_possible() -> bool {
    trouble().is_some()
        || told_slot()
            .lock()
            .map(|told| !told.is_empty())
            .unwrap_or(false)
}

/// `decide_announcement` against the process-wide state, recording what was
/// said so it is not said twice.
pub fn announcement_for(channel: &str) -> Option<Announcement> {
    let trouble = trouble();
    let mut told = told_slot().lock().ok()?;
    let decision = decide_announcement(trouble.as_ref(), told.get(channel).copied());
    match &decision {
        Some(Announcement::Degraded { .. }) => {
            told.insert(channel.to_string(), TroubleKind::Degraded);
        }
        Some(Announcement::Refusing { .. }) => {
            told.insert(channel.to_string(), TroubleKind::Refusing);
        }
        Some(Announcement::Recovered) => {
            told.remove(channel);
        }
        None => {}
    }
    decision
}

/// The message body. Option B's phrasing, chosen because it says **what is
/// degraded** and **that work continues** — a room told only the first will
/// either escalate or stop asking.
///
/// ⚠️ Deliberately NOT pinned to a golden fixture, unlike `bootstrap_text`.
/// That one is pinned because its wording was *measured* at 6-of-6 against
/// 1-of-4; this wording has no such measurement behind it, and pinning it
/// byte-for-byte would imply evidence that does not exist. The tests assert the
/// load-bearing clauses instead.
pub fn announcement_body(a: &Announcement) -> String {
    match a {
        Announcement::Degraded { reason } => format!(
            "I am running without my assigned skill — I could not fetch it from ENGRAM \
             ({reason}). I will answer from my general instructions and pick the skill up \
             automatically once ENGRAM is reachable again. Until then, treat my answers here \
             as missing this project's specific conventions."
        ),
        // Item 5 / D20. Says the three things a room needs and nothing it does
        // not: that no work will happen, why, and that nobody needs to restart
        // anything. ⚠️ It must NOT reuse the degraded wording -- "I will answer
        // from my general instructions" is the one sentence that is false here.
        Announcement::Refusing { reason } => format!(
            "I am not taking tasks in this project: it requires an assigned skill and I do not \
             have one ({reason}). I am still running and still watching this channel, so there \
             is nothing to restart — I will pick work up again as soon as a fetch delivers the \
             skill, or as soon as the project stops requiring one."
        ),
        // ⚠️ Deliberately neutral about the skill itself. The retry loop can go
        // from Degraded to `no_skill_assigned` -- ENGRAM reachable again, nothing
        // assigned -- and "my skill has arrived" would then be a false statement
        // to the room. What recovered is the REACHABILITY, which is what the
        // degraded note actually claimed was broken.
        //
        // ⚠️ It also covers the recovery from Refusing, where what changed may
        // be the project's setting rather than anything about ENGRAM. "No
        // longer applies" is true in both cases; naming a cause would be a
        // guess in one of them.
        Announcement::Recovered => "The note above no longer applies — I am working \
             normally again."
            .to_string(),
    }
}

/// Publisher + keys, stored once at startup for the same reason `INSTALLED` is:
/// the fetch outcome is process-wide, and the place that needs to *say* so is
/// the inbound path, which has a channel but no relay handle.
pub struct Announcer {
    publisher: crate::relay::RelayEventPublisher,
    keys: nostr::Keys,
}

static ANNOUNCER: std::sync::OnceLock<Announcer> = std::sync::OnceLock::new();

/// Idempotent: a second call is ignored rather than panicking, because the
/// caller is a startup path that a reconnect may run twice.
pub fn record_announcer(publisher: crate::relay::RelayEventPublisher, keys: nostr::Keys) {
    let _ = ANNOUNCER.set(Announcer { publisher, keys });
}

/// Say it, if there is anything to say. Called from the inbound path with the
/// channel the agent was woken in.
///
/// Never returns an error: a failed announcement must not cost the turn the
/// agent was woken for. It logs, because an announcement that cannot be posted
/// is exactly the condition an operator needs to know about.
pub async fn maybe_announce(channel_id: uuid::Uuid) {
    let Some(a) = announcement_for(&channel_id.to_string()) else {
        return;
    };
    let Some(announcer) = ANNOUNCER.get() else {
        tracing::warn!(
            channel = %channel_id,
            "skill-state announcement decided but no announcer is registered; nothing was posted"
        );
        return;
    };
    let body = announcement_body(&a);
    // No thread ref and NO mentions -- guard 4.
    let built = match buzz_sdk::build_message(channel_id, &body, None, &[], false, &[], &[]) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "could not build the skill-state announcement");
            return;
        }
    };
    match built.sign_with_keys(&announcer.keys) {
        Ok(signed) => match announcer.publisher.publish_event(signed).await {
            Ok(_) => tracing::info!(
                channel = %channel_id,
                kind = match &a {
                    Announcement::Degraded { .. } => "degraded",
                    Announcement::Refusing { .. } => "refusing",
                    Announcement::Recovered => "recovered",
                },
                "skill-state announcement posted"
            ),
            Err(e) => tracing::warn!(error = %e, "could not publish the skill-state announcement"),
        },
        Err(e) => tracing::warn!(error = %e, "could not sign the skill-state announcement"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, content: &str) -> SkillFile {
        SkillFile {
            path: path.to_string(),
            content: content.to_string(),
        }
    }

    fn payload(directory: &str, files: Vec<SkillFile>) -> SkillMd {
        SkillMd {
            directory: directory.to_string(),
            split: false,
            files,
        }
    }

    fn good() -> SkillMd {
        payload(
            "buzz-coder",
            vec![
                file(
                    "SKILL.md",
                    "---\nname: buzz-coder\ndescription: d\n---\nbody\n",
                ),
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
        assert_eq!(
            decide(&FetchOutcome::Assigned, false, 0),
            HarnessAction::InstallSkill
        );
        assert_eq!(
            decide(&FetchOutcome::Assigned, true, 0),
            HarnessAction::InstallSkill
        );
    }

    #[test]
    fn no_skill_assigned_is_not_a_failure() {
        // The trap: it is a RESULT. Degrading here would announce an outage in a
        // channel for an agent that is working perfectly.
        assert_eq!(
            decide(&FetchOutcome::NoSkillAssigned, false, 0),
            HarnessAction::StartNormally
        );
    }

    #[test]
    fn skills_disabled_is_silent_even_when_a_skill_is_required() {
        // The feature is off deliberately; refusing every task because of a
        // setting someone switched off would take the agent down for no fault.
        assert_eq!(
            decide(&FetchOutcome::SkillsDisabled, false, 0),
            HarnessAction::StartNormally
        );
        assert_eq!(
            decide(&FetchOutcome::SkillsDisabled, true, 0),
            HarnessAction::StartNormally
        );
    }

    #[test]
    fn require_assigned_skill_refuses_tasks_without_exiting() {
        match decide(&FetchOutcome::NoSkillAssigned, true, 0) {
            HarnessAction::RefuseTasks { reason } => {
                assert!(
                    reason.contains("require_assigned_skill"),
                    "reason must name the setting"
                );
            }
            other => panic!("expected RefuseTasks, got {other:?}"),
        }
    }

    #[test]
    fn a_failure_retries_with_growing_backoff_then_stops() {
        let f = FetchOutcome::Failed("connection refused".into());
        assert_eq!(
            decide(&f, false, 0),
            HarnessAction::Retry {
                attempt: 0,
                delay_secs: 1
            }
        );
        assert_eq!(
            decide(&f, false, 1),
            HarnessAction::Retry {
                attempt: 1,
                delay_secs: 4
            }
        );
        assert_eq!(
            decide(&f, false, 2),
            HarnessAction::Retry {
                attempt: 2,
                delay_secs: 10
            }
        );
        // exhausted
        assert_eq!(
            decide(&f, false, FETCH_MAX_ATTEMPTS),
            HarnessAction::DegradedMode {
                reason: "connection refused".into()
            }
        );
    }

    #[test]
    fn an_exhausted_failure_under_require_refuses_rather_than_degrades() {
        let f = FetchOutcome::Failed("[project_not_found]".into());
        match decide(&f, true, FETCH_MAX_ATTEMPTS) {
            HarnessAction::RefuseTasks { reason } => {
                // The operator needs the cause, not just the policy.
                assert!(
                    reason.contains("project_not_found"),
                    "reason must carry the error"
                );
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
            HarnessAction::DegradedMode { reason } => {
                assert_eq!(reason, "engram timed out after 10s")
            }
            other => panic!("expected DegradedMode, got {other:?}"),
        }
    }

    // ── validation: the refusals ────────────────────────────────────────────

    #[test]
    fn accepts_the_contract_shape() {
        let rels = good()
            .validate()
            .expect("the documented shape must validate");
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
        for bad in [
            "../escape.md",
            "references/../../escape.md",
            "/etc/passwd",
            "a/./b.md",
        ] {
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
        let p = payload(
            "s",
            vec![file("SKILL.md", "x"), file("references\\part.md", "x")],
        );
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
            Err(SkillWriteError::NestedSkillMd {
                path: "references/SKILL.md".into()
            })
        );
    }

    #[test]
    fn refuses_duplicate_paths() {
        let p = payload("s", vec![file("SKILL.md", "a"), file("SKILL.md", "b")]);
        assert_eq!(
            p.validate(),
            Err(SkillWriteError::DuplicatePath {
                path: "SKILL.md".into()
            })
        );
    }

    #[test]
    fn refuses_an_empty_file_set() {
        assert_eq!(
            payload("s", vec![]).validate(),
            Err(SkillWriteError::NoFiles)
        );
    }

    // ── writing ────────────────────────────────────────────────────────────

    #[test]
    fn writes_the_files_and_creates_nested_dirs() {
        let root = root_with_siblings(&[]);
        let target = good().write_into(root.path()).expect("write");
        assert_eq!(target, root.path().join("buzz-coder"));
        assert!(target.join(ROOT_FILE).is_file());
        assert_eq!(
            fs::read_to_string(target.join("references/part-one.md")).unwrap(),
            "part one\n"
        );
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
        assert!(
            !stale.exists(),
            "a dropped part must not survive the replacement"
        );
        assert!(root.path().join("buzz-coder").join(ROOT_FILE).is_file());
    }

    #[test]
    fn siblings_are_untouched() {
        // The finding that made this module mostly guards: on PROD the root
        // holds five skills, and reading "replace the directory" as the ROOT
        // would delete four of them on an ordinary successful fetch.
        let siblings = [
            "channel-discipline",
            "fernhill-conventions",
            "git-workflow",
            "python-coding",
        ];
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
            "buzz-coder",
            "channel-discipline",
            "fernhill-conventions",
            "git-workflow",
            "python-coding",
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
        assert!(
            !root.path().join("buzz-coder").exists(),
            "nothing may be written"
        );
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
        assert_eq!(
            before, after,
            "a failed fetch must not degrade a working skill"
        );
    }

    #[test]
    fn a_leftover_staging_dir_from_a_crashed_run_is_cleared() {
        let root = root_with_siblings(&[]);
        let staging = root.path().join(".buzz-coder.incoming");
        fs::create_dir_all(staging.join("references")).unwrap();
        fs::write(staging.join("junk.md"), "from a dead run").unwrap();

        good().write_into(root.path()).unwrap();
        assert!(
            !staging.exists(),
            "staging must not survive a successful run"
        );
        assert!(
            !root.path().join("buzz-coder/junk.md").exists(),
            "and must not leak into the skill"
        );
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
#[cfg(test)]
mod fetch_tests {
    use super::*;

    /// Recorded from the live DEV relay on 2026-11-04 as `user_067`, with
    /// `format="skill_md"`. `composed_markdown` and `parts` are trimmed — the
    /// client does not read them — everything the client touches is verbatim.
    const SPLIT: &str = include_str!("../tests/fixtures/assigned_split.json");
    const SINGLE: &str = include_str!("../tests/fixtures/assigned_single_file.json");

    // ── error_code ──────────────────────────────────────────────────────────

    #[test]
    fn error_code_reads_the_bracketed_code_behind_the_mcp_prefix() {
        let text = "Error executing tool get_assigned_skill: [project_not_found]";
        assert_eq!(error_code(text), Some("project_not_found"));
    }

    #[test]
    fn error_code_reads_a_bare_bracketed_code_too() {
        assert_eq!(error_code("[not_an_agent]"), Some("not_an_agent"));
    }

    #[test]
    fn error_code_declines_prose_so_the_caller_carries_it_verbatim() {
        // The negative control for the three above: text that is NOT in the
        // documented shape must return None rather than a plausible guess, so
        // the whole message survives into the operator's log.
        assert_eq!(error_code("upstream timed out"), None);
        assert_eq!(error_code("[two words]"), None);
        assert_eq!(error_code("[]"), None);
        assert_eq!(error_code("[unterminated"), None);
    }

    // ── extract_envelope ────────────────────────────────────────────────────

    #[test]
    fn extract_envelope_reads_plain_json() {
        let v = extract_envelope(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#).unwrap();
        assert_eq!(v["id"], 2);
    }

    #[test]
    fn extract_envelope_reads_an_sse_frame() {
        let body =
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"ok\":true}}\n\n";
        let v = extract_envelope(body).unwrap();
        assert_eq!(v["result"]["ok"], true);
    }

    #[test]
    fn extract_envelope_skips_an_empty_data_line() {
        let body = "data:\ndata: {\"id\":7}\n";
        assert_eq!(extract_envelope(body).unwrap()["id"], 7);
    }

    #[test]
    fn extract_envelope_names_what_was_wrong() {
        assert!(extract_envelope("").unwrap_err().contains("empty"));
        assert!(extract_envelope("   ").unwrap_err().contains("empty"));
        assert!(extract_envelope("not json")
            .unwrap_err()
            .contains("not JSON"));
        assert!(extract_envelope("event: ping\n")
            .unwrap_err()
            .contains("no data frame"));
        assert!(extract_envelope("data: not json\n")
            .unwrap_err()
            .contains("not JSON"));
    }

    // ── interpret_payload: the two results that are NOT failures ────────────

    #[test]
    fn no_skill_assigned_is_a_result() {
        let f = interpret_payload(r#"{"status":"no_skill_assigned"}"#);
        assert_eq!(f.outcome, FetchOutcome::NoSkillAssigned);
        assert!(f.skill_md.is_none());
        // And the whole point of separating it: with no requirement set, this
        // starts the agent silently.
        assert_eq!(decide(&f.outcome, false, 0), HarnessAction::StartNormally);
    }

    #[test]
    fn skills_disabled_is_a_result() {
        let f = interpret_payload(r#"{"status":"skills_disabled"}"#);
        assert_eq!(f.outcome, FetchOutcome::SkillsDisabled);
        assert_eq!(decide(&f.outcome, true, 0), HarnessAction::StartNormally);
    }

    // ── interpret_payload: the real payloads ────────────────────────────────

    #[test]
    fn the_recorded_split_payload_installs_eight_files_in_order() {
        let f = interpret_payload(SPLIT);
        assert_eq!(f.outcome, FetchOutcome::Assigned);
        let skill = f
            .skill_md
            .expect("a recorded `assigned` payload carries skill_md");
        assert_eq!(skill.directory, "buzz-coder");
        assert!(skill.split);
        let rels = skill
            .validate()
            .expect("the live payload must satisfy the contract");
        let got: Vec<String> = rels
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(
            got,
            vec![
                "SKILL.md",
                "references/working-discipline.md",
                "references/working-together.md",
                "references/working-with-engram.md",
                "references/buzz-coder.md",
                "references/channel-discipline.md",
                "references/python-coding.md",
                "references/git-workflow.md",
            ]
        );
    }

    #[test]
    fn the_recorded_single_file_payload_installs_one_file() {
        let f = interpret_payload(SINGLE);
        assert_eq!(f.outcome, FetchOutcome::Assigned);
        let skill = f
            .skill_md
            .expect("a recorded `assigned` payload carries skill_md");
        assert!(!skill.split);
        assert_eq!(skill.files.len(), 1);
        assert_eq!(skill.files[0].path, "SKILL.md");
        // The manifest form is the split one; the single file carries the body.
        assert!(skill.files[0]
            .content
            .contains("## Two things you must not do"));
        assert_eq!(skill.validate().unwrap().len(), 1);
    }

    #[test]
    fn the_split_root_is_a_manifest_that_names_load_skill() {
        // Why this is asserted rather than assumed: it is the whole reason the
        // split shape behaves differently in a channel (review §12). If ENGRAM
        // ever inlines the parts instead, this test is how we find out.
        let skill = interpret_payload(SPLIT).skill_md.unwrap();
        let root = &skill.files[0];
        assert_eq!(root.path, "SKILL.md");
        assert!(root.content.contains("load_skill"));
        assert!(root.content.contains("This skill has 7 parts"));
    }

    // ── interpret_payload: the refusals ─────────────────────────────────────

    #[test]
    fn assigned_without_skill_md_is_a_failure_not_a_success() {
        let f = interpret_payload(r#"{"status":"assigned","skill":{"name":"x"}}"#);
        match f.outcome {
            FetchOutcome::Failed(why) => assert!(why.contains("no `skill_md`"), "{why}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_status_is_a_failure_rather_than_silence() {
        match interpret_payload(r#"{"project_id":"project:1"}"#).outcome {
            FetchOutcome::Failed(why) => assert!(why.contains("`status`"), "{why}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    // ── Item 4: `require_assigned_skill` on the wire (seam v1.16 §7.1) ──

    #[test]
    fn nothing_assigned_carries_the_requirement_both_ways() {
        let on = interpret_payload(
            r#"{"status":"no_skill_assigned","project_id":"project:1","require_assigned_skill":true}"#,
        );
        assert_eq!(on.outcome, FetchOutcome::NoSkillAssigned);
        assert_eq!(on.require_assigned_skill, Some(true));

        let off = interpret_payload(
            r#"{"status":"no_skill_assigned","project_id":"project:1","require_assigned_skill":false}"#,
        );
        assert_eq!(off.outcome, FetchOutcome::NoSkillAssigned);
        assert_eq!(off.require_assigned_skill, Some(false));
    }

    #[test]
    fn an_assigned_payload_carries_it_without_displacing_the_skill() {
        let mut v: serde_json::Value = serde_json::from_str(SPLIT).unwrap();
        v["require_assigned_skill"] = serde_json::Value::Bool(true);
        let f = interpret_payload(&v.to_string());
        assert_eq!(f.outcome, FetchOutcome::Assigned);
        assert_eq!(f.require_assigned_skill, Some(true));
        assert!(
            f.skill_md.is_some(),
            "reading the new field must not cost us the payload"
        );
    }

    #[test]
    fn a_pre_v1_16_payload_carries_no_value_rather_than_false() {
        // The recorded fixtures were taken from a v1.15 server, so this is the
        // rollback shape measured rather than imagined: absence, not `false`.
        assert_eq!(interpret_payload(SPLIT).require_assigned_skill, None);
        assert_eq!(interpret_payload(SINGLE).require_assigned_skill, None);
        assert_eq!(
            interpret_payload(r#"{"status":"no_skill_assigned"}"#).require_assigned_skill,
            None
        );
    }

    #[test]
    fn skills_disabled_carries_no_requirement_even_when_one_is_sent() {
        let f = interpret_payload(r#"{"status":"skills_disabled","require_assigned_skill":true}"#);
        assert_eq!(f.outcome, FetchOutcome::SkillsDisabled);
        assert_eq!(
            f.require_assigned_skill, None,
            "§7.1 gives this status no such field, and it outranks the requirement anyway"
        );
    }

    #[test]
    fn a_non_boolean_requirement_reads_as_absent() {
        // `as_bool()` answers `None` for `"true"` just as it does for a field
        // that was never sent, so without this the two are indistinguishable.
        // Reading it as absent is the safe half; the warn line is the loud half.
        for raw in [r#""true""#, "1", "null", "[]", "{}"] {
            let text =
                format!(r#"{{"status":"no_skill_assigned","require_assigned_skill":{raw}}}"#);
            let f = interpret_payload(&text);
            assert_eq!(f.outcome, FetchOutcome::NoSkillAssigned, "{raw}");
            assert_eq!(f.require_assigned_skill, None, "{raw}");
        }
    }

    #[test]
    fn an_unknown_status_names_the_value() {
        match interpret_payload(r#"{"status":"on_fire"}"#).outcome {
            FetchOutcome::Failed(why) => assert!(why.contains("on_fire"), "{why}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn a_tool_result_wrapped_for_display_does_not_parse_as_assigned() {
        // The trap this test exists to hold shut: some MCP clients render a
        // tool result as {"result": "<json string>"}. ENGRAM does not send
        // that, and a parser written from a client's rendering would look
        // correct and never match a real response. Refuse it loudly.
        let display_shape = r#"{"result":"{\"status\":\"assigned\",\"skill_md\":{\"directory\":\"buzz-coder\",\"files\":[{\"path\":\"SKILL.md\",\"content\":\"x\"}]}}"}"#;
        match interpret_payload(display_shape).outcome {
            FetchOutcome::Failed(why) => assert!(why.contains("`status`"), "{why}"),
            other => panic!("a display wrapper must not read as success, got {other:?}"),
        }
    }

    // ── interpret_envelope ──────────────────────────────────────────────────

    #[test]
    fn a_tool_error_becomes_its_bracketed_code() {
        let env = serde_json::json!({
            "jsonrpc": "2.0", "id": 2,
            "result": {
                "isError": true,
                "content": [{"type": "text", "text": "Error executing tool get_assigned_skill: [project_not_found]"}]
            }
        });
        assert_eq!(
            interpret_envelope(&env).outcome,
            FetchOutcome::Failed("[project_not_found]".to_string())
        );
    }

    #[test]
    fn a_tool_error_without_a_code_keeps_its_whole_text() {
        let env = serde_json::json!({
            "result": { "is_error": true, "content": [{"type":"text","text":"upstream exploded"}] }
        });
        assert_eq!(
            interpret_envelope(&env).outcome,
            FetchOutcome::Failed("upstream exploded".to_string())
        );
    }

    #[test]
    fn a_jsonrpc_error_is_carried_with_its_message() {
        let env = serde_json::json!({"error": {"code": -32601, "message": "Method not found"}});
        match interpret_envelope(&env).outcome {
            FetchOutcome::Failed(why) => assert!(why.contains("Method not found"), "{why}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn an_envelope_with_neither_result_nor_error_fails() {
        let env = serde_json::json!({"jsonrpc": "2.0", "id": 2});
        match interpret_envelope(&env).outcome {
            FetchOutcome::Failed(why) => assert!(why.contains("neither"), "{why}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn a_result_with_no_text_block_fails() {
        let env = serde_json::json!({"result": {"content": [{"type":"image","data":"…"}]}});
        match interpret_envelope(&env).outcome {
            FetchOutcome::Failed(why) => assert!(why.contains("no text block"), "{why}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn the_whole_path_from_an_sse_body_to_an_installable_skill() {
        // The one test that exercises the real sequence end to end with no
        // network: SSE body → envelope → tool text → payload → validated paths.
        let payload: serde_json::Value = serde_json::from_str(SPLIT).unwrap();
        let body = format!(
            "event: message\ndata: {}\n\n",
            serde_json::json!({
                "jsonrpc": "2.0", "id": 2,
                "result": { "content": [{"type": "text", "text": payload.to_string()}] }
            })
        );
        let env = extract_envelope(&body).expect("the SSE body must yield an envelope");
        let f = interpret_envelope(&env);
        assert_eq!(f.outcome, FetchOutcome::Assigned);
        assert_eq!(decide(&f.outcome, false, 0), HarnessAction::InstallSkill);
        assert_eq!(f.skill_md.unwrap().validate().unwrap().len(), 8);
    }

    #[test]
    fn the_client_builds_and_holds_its_configuration() {
        let c = EngramSkillClient::new(
            "https://engram-dev.example/mcp/",
            "engram_pat_example",
            Duration::from_secs(10),
        )
        .expect("a well-formed client must build");
        assert_eq!(c.url, "https://engram-dev.example/mcp/");
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn cfg_from(pairs: &[(&str, &str)]) -> Result<Option<SkillFetchConfig>, String> {
        let map = env(pairs);
        SkillFetchConfig::from_lookup(|k| map.get(k).cloned())
    }

    const COMPLETE: &[(&str, &str)] = &[
        ("ENGRAM_SKILL_FETCH", "1"),
        ("ENGRAM_MCP_URL", "https://engram-dev.example/mcp/"),
        ("ENGRAM_PAT", "engram_pat_example"),
        ("ENGRAM_PROJECT", "project:b5cb944650f21b76"),
        ("HOME", "/root"),
    ];

    // ── the switch ──────────────────────────────────────────────────────────

    #[test]
    fn the_switch_defaults_off() {
        assert!(cfg_from(&[]).unwrap().is_none());
    }

    #[test]
    fn the_switch_is_off_for_anything_that_is_not_an_affirmative() {
        for v in ["0", "false", "no", "off", "", "maybe", "2"] {
            let mut pairs = COMPLETE.to_vec();
            pairs[0] = ("ENGRAM_SKILL_FETCH", v);
            assert!(
                cfg_from(&pairs).unwrap().is_none(),
                "{v:?} must not enable the fetch"
            );
        }
    }

    #[test]
    fn the_switch_is_on_for_the_four_affirmatives_in_any_case() {
        for v in ["1", "true", "TRUE", "yes", "On"] {
            let mut pairs = COMPLETE.to_vec();
            pairs[0] = ("ENGRAM_SKILL_FETCH", v);
            assert!(cfg_from(&pairs).unwrap().is_some(), "{v:?} must enable it");
        }
    }

    // ── a switch that is on with missing configuration names the key ────────

    #[test]
    fn each_missing_key_names_itself() {
        for (missing, expect) in [
            ("ENGRAM_MCP_URL", "ENGRAM_MCP_URL"),
            ("ENGRAM_PAT", "ENGRAM_PAT"),
            ("ENGRAM_PROJECT", "ENGRAM_PROJECT"),
        ] {
            let pairs: Vec<(&str, &str)> = COMPLETE
                .iter()
                .copied()
                .filter(|(k, _)| *k != missing)
                .collect();
            let err = cfg_from(&pairs).expect_err("an incomplete config must be an error");
            assert!(err.contains(expect), "{err}");
        }
    }

    #[test]
    fn a_whitespace_only_value_counts_as_missing() {
        let mut pairs = COMPLETE.to_vec();
        pairs[3] = ("ENGRAM_PROJECT", "   ");
        assert!(cfg_from(&pairs).unwrap_err().contains("ENGRAM_PROJECT"));
    }

    #[test]
    fn the_project_error_explains_why_an_ambient_call_will_not_do() {
        let pairs: Vec<(&str, &str)> = COMPLETE
            .iter()
            .copied()
            .filter(|(k, _)| *k != "ENGRAM_PROJECT")
            .collect();
        let err = cfg_from(&pairs).unwrap_err();
        assert!(err.contains("project_not_found"), "{err}");
    }

    // ── the skills root ────────────────────────────────────────────────────

    #[test]
    fn the_skills_root_defaults_under_home() {
        let cfg = cfg_from(COMPLETE).unwrap().unwrap();
        assert_eq!(cfg.skills_root, PathBuf::from("/root/.agents/skills"));
    }

    #[test]
    fn an_explicit_skills_root_wins_over_home() {
        let mut pairs = COMPLETE.to_vec();
        pairs.push(("ENGRAM_SKILL_ROOT", "/srv/skills"));
        let cfg = cfg_from(&pairs).unwrap().unwrap();
        assert_eq!(cfg.skills_root, PathBuf::from("/srv/skills"));
    }

    #[test]
    fn without_home_or_an_explicit_root_the_config_refuses() {
        let pairs: Vec<(&str, &str)> = COMPLETE
            .iter()
            .copied()
            .filter(|(k, _)| *k != "HOME")
            .collect();
        assert!(cfg_from(&pairs).unwrap_err().contains("skills root"));
    }

    #[test]
    fn require_assigned_skill_is_off_unless_asked_for() {
        assert!(!cfg_from(COMPLETE).unwrap().unwrap().require_assigned_skill);
        let mut pairs = COMPLETE.to_vec();
        pairs.push(("ENGRAM_REQUIRE_ASSIGNED_SKILL", "yes"));
        assert!(cfg_from(&pairs).unwrap().unwrap().require_assigned_skill);
    }

    // ── fetch_with_policy: the ladder ──────────────────────────────────────

    fn test_cfg(root: &Path, require: bool) -> SkillFetchConfig {
        SkillFetchConfig {
            url: "https://engram-dev.example/mcp/".to_string(),
            pat: "engram_pat_example".to_string(),
            project: "project:test".to_string(),
            skills_root: root.to_path_buf(),
            require_assigned_skill: require,
        }
    }

    fn assigned() -> Fetched {
        interpret_payload(include_str!("../tests/fixtures/assigned_split.json"))
    }

    /// A fetch that returns a scripted sequence and counts its calls.
    struct Script {
        answers: RefCell<Vec<Fetched>>,
        calls: RefCell<usize>,
    }

    impl Script {
        fn new(answers: Vec<Fetched>) -> Self {
            Script {
                answers: RefCell::new(answers),
                calls: RefCell::new(0),
            }
        }
        fn next(&self) -> Fetched {
            *self.calls.borrow_mut() += 1;
            let mut a = self.answers.borrow_mut();
            if a.is_empty() {
                Fetched::failed("script exhausted")
            } else {
                a.remove(0)
            }
        }
        fn calls(&self) -> usize {
            *self.calls.borrow()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_assigned_payload_is_written_on_the_first_attempt() {
        let root = tempfile::tempdir().unwrap();
        let cfg = test_cfg(root.path(), false);
        let script = Script::new(vec![assigned()]);
        let out = fetch_with_policy(&cfg, &mut RequirementCache::new(), || async {
            script.next()
        })
        .await;
        match &out {
            StartupOutcome::Installed { path, skill } => {
                assert_eq!(path, &root.path().join("buzz-coder"));
                assert_eq!(skill.directory, "buzz-coder");
                assert_eq!(skill.references.len(), 7);
            }
            other => panic!("expected Installed, got {other:?}"),
        }
        assert_eq!(script.calls(), 1, "a success must not retry");
        assert!(root.path().join("buzz-coder/SKILL.md").is_file());
        assert!(root
            .path()
            .join("buzz-coder/references/git-workflow.md")
            .is_file());
    }

    #[tokio::test(start_paused = true)]
    async fn two_failures_then_a_success_still_installs() {
        let root = tempfile::tempdir().unwrap();
        let cfg = test_cfg(root.path(), false);
        let script = Script::new(vec![
            Fetched::failed("transport: connection refused"),
            Fetched::failed("transport: connection refused"),
            assigned(),
        ]);
        let out = fetch_with_policy(&cfg, &mut RequirementCache::new(), || async {
            script.next()
        })
        .await;
        assert!(matches!(out, StartupOutcome::Installed { .. }), "{out:?}");
        assert_eq!(script.calls(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn the_ladder_is_four_calls_and_fifteen_seconds() {
        // Written expecting THREE calls, because the constant is called
        // FETCH_MAX_ATTEMPTS. It is the retry count: `decide` retries while
        // `attempts < FETCH_MAX_ATTEMPTS`, so the initial call plus three
        // retries is four. Both numbers are asserted here so the next reader
        // does not have to re-derive them -- and the 15 seconds is the figure
        // that rules out the session-birth hook.
        let root = tempfile::tempdir().unwrap();
        let cfg = test_cfg(root.path(), false);
        let script = Script::new(vec![
            Fetched::failed("transport: timed out"),
            Fetched::failed("transport: timed out"),
            Fetched::failed("transport: timed out"),
            Fetched::failed("transport: timed out"),
        ]);
        let started = tokio::time::Instant::now();
        let out = fetch_with_policy(&cfg, &mut RequirementCache::new(), || async {
            script.next()
        })
        .await;
        let waited = started.elapsed();

        match out {
            StartupOutcome::Degraded { reason } => {
                assert!(reason.contains("timed out"), "{reason}")
            }
            other => panic!("expected Degraded, got {other:?}"),
        }
        assert_eq!(
            script.calls(),
            1 + FETCH_MAX_ATTEMPTS as usize,
            "the ladder is the initial call plus FETCH_MAX_ATTEMPTS retries"
        );
        assert_eq!(waited, Duration::from_secs(15), "1 + 4 + 10");
    }

    #[tokio::test(start_paused = true)]
    async fn an_exhausted_ladder_under_a_requirement_refuses_rather_than_degrading() {
        let root = tempfile::tempdir().unwrap();
        let cfg = test_cfg(root.path(), true);
        let script = Script::new(vec![
            Fetched::failed("transport: timed out"),
            Fetched::failed("transport: timed out"),
            Fetched::failed("transport: timed out"),
            Fetched::failed("transport: timed out"),
        ]);
        match fetch_with_policy(&cfg, &mut RequirementCache::new(), || async {
            script.next()
        })
        .await
        {
            StartupOutcome::RefuseTasks { reason } => {
                assert!(reason.contains("require_assigned_skill"), "{reason}")
            }
            other => panic!("expected RefuseTasks, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_assigned_starts_quietly_and_writes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let cfg = test_cfg(root.path(), false);
        let script = Script::new(vec![Fetched::plain(FetchOutcome::NoSkillAssigned)]);
        let out = fetch_with_policy(&cfg, &mut RequirementCache::new(), || async {
            script.next()
        })
        .await;
        assert_eq!(out, StartupOutcome::StartedNormally);
        assert_eq!(script.calls(), 1, "a result must not retry");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_assigned_under_a_requirement_refuses_tasks() {
        let root = tempfile::tempdir().unwrap();
        let cfg = test_cfg(root.path(), true);
        let script = Script::new(vec![Fetched::plain(FetchOutcome::NoSkillAssigned)]);
        match fetch_with_policy(&cfg, &mut RequirementCache::new(), || async {
            script.next()
        })
        .await
        {
            StartupOutcome::RefuseTasks { .. } => {}
            other => panic!("expected RefuseTasks, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn skills_disabled_outranks_a_requirement() {
        let root = tempfile::tempdir().unwrap();
        let cfg = test_cfg(root.path(), true);
        let script = Script::new(vec![Fetched::plain(FetchOutcome::SkillsDisabled)]);
        assert_eq!(
            fetch_with_policy(&cfg, &mut RequirementCache::new(), || async {
                script.next()
            })
            .await,
            StartupOutcome::StartedNormally
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_write_failure_degrades_naming_the_path_and_does_not_retry() {
        // The negative control for the install branch: the payload is perfect
        // and the filesystem refuses. Retrying would not help, so it must not.
        let root = tempfile::tempdir().unwrap();
        let blocked = root.path().join("not-a-dir");
        std::fs::write(&blocked, b"i am a file").unwrap();
        let cfg = test_cfg(&blocked, false);
        let script = Script::new(vec![assigned()]);
        match fetch_with_policy(&cfg, &mut RequirementCache::new(), || async {
            script.next()
        })
        .await
        {
            StartupOutcome::Degraded { reason } => {
                assert!(!reason.is_empty(), "a write failure must name something")
            }
            other => panic!("expected Degraded, got {other:?}"),
        }
        assert_eq!(script.calls(), 1, "a write failure must not retry");
    }

    // ── Item 4: precedence and the per-project cache (seam v1.16 §7.1) ──

    #[test]
    fn the_response_wins_then_the_cache_then_the_local_setting() {
        // the response beats both
        assert!(effective_requirement(Some(true), Some(false), false));
        assert!(!effective_requirement(Some(false), Some(true), true));
        // the cache beats the local setting
        assert!(effective_requirement(None, Some(true), false));
        assert!(!effective_requirement(None, Some(false), true));
        // and the local setting applies only with nothing observed at all
        assert!(effective_requirement(None, None, true));
        assert!(!effective_requirement(None, None, false));
    }

    #[test]
    fn an_absent_field_does_not_clear_what_a_response_said() {
        let mut cache = RequirementCache::new();
        assert!(cache.resolve("project:test", Some(true), false));
        // Then a response that says nothing: a rollback, or a failed fetch.
        assert!(
            cache.resolve("project:test", None, false),
            "absence is not `false`"
        );
        assert_eq!(cache.get("project:test"), Some(true));
    }

    #[test]
    fn the_cache_answers_per_project_and_never_for_another() {
        let mut cache = RequirementCache::new();
        cache.observe("project:one", true);
        assert_eq!(cache.get("project:two"), None);
        assert!(!cache.resolve("project:two", None, false));
    }

    #[test]
    fn the_missing_field_signal_fires_only_where_the_contract_promises_one() {
        assert!(requirement_field_missing(&FetchOutcome::Assigned, None));
        assert!(requirement_field_missing(
            &FetchOutcome::NoSkillAssigned,
            None
        ));
        assert!(!requirement_field_missing(
            &FetchOutcome::Assigned,
            Some(false)
        ));
        assert!(!requirement_field_missing(
            &FetchOutcome::SkillsDisabled,
            None
        ));
        assert!(!requirement_field_missing(
            &FetchOutcome::Failed("transport: timed out".into()),
            None
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn the_projects_requirement_overrides_an_unset_environment() {
        let root = tempfile::tempdir().unwrap();
        let cfg = test_cfg(root.path(), false);
        let script = Script::new(vec![Fetched::result(
            FetchOutcome::NoSkillAssigned,
            Some(true),
        )]);
        match fetch_with_policy(&cfg, &mut RequirementCache::new(), || async {
            script.next()
        })
        .await
        {
            StartupOutcome::RefuseTasks { reason } => {
                assert!(reason.contains("require_assigned_skill"), "{reason}")
            }
            other => panic!("expected RefuseTasks, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_project_that_turns_it_off_beats_an_environment_that_turns_it_on() {
        // The direction an operator gets wrong: ENGRAM is the authority, so a
        // stale `ENGRAM_REQUIRE_ASSIGNED_SKILL=1` must not refuse tasks in a
        // project that does not ask for it.
        let root = tempfile::tempdir().unwrap();
        let cfg = test_cfg(root.path(), true);
        let script = Script::new(vec![Fetched::result(
            FetchOutcome::NoSkillAssigned,
            Some(false),
        )]);
        assert_eq!(
            fetch_with_policy(&cfg, &mut RequirementCache::new(), || async {
                script.next()
            })
            .await,
            StartupOutcome::StartedNormally
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_cached_false_survives_an_outage_and_degrades_rather_than_refusing() {
        // Why `run_startup_fetch` owns the cache rather than this function:
        // ENGRAM said this project requires nothing, then went away. The local
        // setting must not resurrect a requirement ENGRAM disowned.
        let root = tempfile::tempdir().unwrap();
        let cfg = test_cfg(root.path(), true);
        let mut cache = RequirementCache::new();

        let first = Script::new(vec![Fetched::result(
            FetchOutcome::NoSkillAssigned,
            Some(false),
        )]);
        assert_eq!(
            fetch_with_policy(&cfg, &mut cache, || async { first.next() }).await,
            StartupOutcome::StartedNormally
        );

        // An exhausted script fails every call, which is the outage.
        let outage = Script::new(vec![]);
        match fetch_with_policy(&cfg, &mut cache, || async { outage.next() }).await {
            StartupOutcome::Degraded { .. } => {}
            other => panic!("expected Degraded, got {other:?}"),
        }
        assert_eq!(outage.calls(), 1 + FETCH_MAX_ATTEMPTS as usize);
    }

    #[tokio::test(start_paused = true)]
    async fn skills_disabled_outranks_a_cached_requirement() {
        let root = tempfile::tempdir().unwrap();
        let cfg = test_cfg(root.path(), false);
        let mut cache = RequirementCache::new();
        cache.observe("project:test", true);
        let script = Script::new(vec![Fetched::plain(FetchOutcome::SkillsDisabled)]);
        assert_eq!(
            fetch_with_policy(&cfg, &mut cache, || async { script.next() }).await,
            StartupOutcome::StartedNormally
        );
    }

    #[tokio::test(start_paused = true)]
    async fn assigned_with_no_files_degrades_rather_than_panicking() {
        let root = tempfile::tempdir().unwrap();
        let cfg = test_cfg(root.path(), false);
        let script = Script::new(vec![Fetched {
            outcome: FetchOutcome::Assigned,
            skill_md: None,
            require_assigned_skill: None,
        }]);
        match fetch_with_policy(&cfg, &mut RequirementCache::new(), || async {
            script.next()
        })
        .await
        {
            StartupOutcome::Degraded { reason } => assert!(reason.contains("no files"), "{reason}"),
            other => panic!("expected Degraded, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod bootstrap_tests {
    use super::*;

    fn split_skill() -> InstalledSkill {
        let skill = interpret_payload(include_str!("../tests/fixtures/assigned_split.json"))
            .skill_md
            .unwrap();
        InstalledSkill {
            directory: skill.directory.clone(),
            references: skill.reference_paths(),
        }
    }

    /// ⚠️ A golden file, and it is not decoration. The string in
    /// `tests/fixtures/bootstrap_split.txt` is the **exact** text that was put
    /// in front of the model on DEV for the measurement that justifies this
    /// whole mechanism: 6 of 6 runs loaded all seven references and refused the
    /// forbidden action. If this test fails, the code no longer emits the
    /// string that was measured, and the measurement no longer supports it —
    /// so the correct response is to re-run the measurement, not to update the
    /// fixture.
    #[test]
    fn the_emitted_string_is_byte_for_byte_the_one_that_was_measured() {
        assert_eq!(
            bootstrap_text(Some(&split_skill())).unwrap(),
            include_str!("../tests/fixtures/bootstrap_split.txt")
        );
    }

    #[test]
    fn nothing_installed_means_no_instruction_at_all() {
        // Not an empty string: an instruction to load a skill that is not on
        // disk sends the agent to a tool call that fails, so absence is the
        // correct output when the switch is off or the fetch failed.
        assert_eq!(bootstrap_text(None), None);
    }

    #[test]
    fn reference_paths_excludes_the_root_and_keeps_delivery_order() {
        let skill = interpret_payload(include_str!("../tests/fixtures/assigned_split.json"))
            .skill_md
            .unwrap();
        let refs = skill.reference_paths();
        assert_eq!(refs.len(), 7, "8 files minus the root");
        assert!(!refs.iter().any(|r| r == "SKILL.md"));
        assert_eq!(refs[0], "references/working-discipline.md");
        assert_eq!(refs[6], "references/git-workflow.md");
    }

    #[test]
    fn a_single_file_skill_gets_one_load_and_no_talk_of_references() {
        let skill = interpret_payload(include_str!("../tests/fixtures/assigned_single_file.json"))
            .skill_md
            .unwrap();
        let installed = InstalledSkill {
            directory: skill.directory.clone(),
            references: skill.reference_paths(),
        };
        assert!(installed.references.is_empty());
        let text = bootstrap_text(Some(&installed)).unwrap();
        assert!(text.contains(r#"load_skill(name: "buzz-coder")"#));
        // Mentioning references that do not exist would invite a failing call.
        assert!(!text.contains("references/"), "{text}");
    }

    #[test]
    fn a_split_skill_names_every_part_explicitly() {
        let text = bootstrap_text(Some(&split_skill())).unwrap();
        for part in [
            "working-discipline",
            "working-together",
            "working-with-engram",
            "buzz-coder",
            "channel-discipline",
            "python-coding",
            "git-workflow",
        ] {
            let expected = format!(r#"load_skill(name: "buzz-coder/references/{part}.md")"#);
            assert!(text.contains(&expected), "missing {expected}\n---\n{text}");
        }
        assert!(text.contains("in 7 parts"));
    }

    #[test]
    fn the_two_measured_phrases_survive_any_future_edit() {
        // These are the parts the DEV measurement attributes the 6-of-6 to.
        // Dropping either is the failure this test exists to catch: without
        // "every reference it lists" the model loads the index and stops, and
        // without the explicit path form it does not reach the parts at all.
        let text = bootstrap_text(Some(&split_skill())).unwrap();
        assert!(
            text.contains("every reference it lists"),
            "the phrase that gets the parts loaded is gone"
        );
        assert!(
            text.contains("/references/"),
            "the explicit reference path form is gone"
        );
        assert!(
            text.starts_with("Before you do anything else"),
            "the instruction must come first in its own section"
        );
    }

    // ⚠️ There is deliberately NO test of `record_installed`, `installed_skill`
    // or `fetch_pending` here. One was written and it broke
    // `pool::model_switch_tests::session_new_sends_policy_specific_base_and_scope_specific_title`:
    // the slot is process-wide, the crate's tests share a process, and writing
    // it appended an `<assigned-skill>` section to an unrelated test's expected
    // system prompt. Isolating it is not worth it — the slot is three lines and
    // every decision it feeds is `bootstrap_text`, which is tested above. The
    // leak itself is the finding: a process-global that alters every system
    // prompt is hidden coupling, and the only reason it is acceptable here is
    // that the fetch is asynchronous and finishes AFTER the pool is built, so a
    // value captured at construction would be `None` forever.
    //
    // `fetch_pending` is the same story and one boolean: "a fetch started and
    // nothing has been recorded yet". A test would have to set a process-wide
    // flag that then stays set for every test that runs after it, and the only
    // thing it gates is a `tracing::warn!`. Not worth the fragility.
}

#[cfg(test)]
mod announcement_tests {
    use super::*;

    /// The two tests below drive the **process-wide** slots, and `cargo test`
    /// runs them on different threads in one binary.
    ///
    /// ⚠️ Naming distinct channels per test is NOT enough, which is how this
    /// was first written and how it failed: `TOLD` is per channel but
    /// `TROUBLE` is one global marker, so `record_trouble` in either test
    /// made the other's "healthy: silence" assertion flap. Serialise them.
    ///
    /// The lock itself is `super::STATE_TEST_LOCK`, shared with `lib.rs`'s
    /// gate test — a second lock here would serialise this module against
    /// itself and nothing else.
    fn state_guard() -> std::sync::MutexGuard<'static, ()> {
        state_test_guard()
    }

    /// `decide_announcement` takes a borrowed trouble, and a test that builds
    /// one inline would drop it at the end of the argument list.
    fn degraded(reason: &str) -> Option<SkillTrouble> {
        Some(SkillTrouble::Degraded {
            reason: reason.into(),
        })
    }

    fn refusing(reason: &str) -> Option<SkillTrouble> {
        Some(SkillTrouble::Refusing {
            reason: reason.into(),
        })
    }

    // Guard 1 and guard 2 share a branch, and that is the point: keying on an
    // explicit degraded marker makes "nothing assigned" and "still in flight"
    // both silent without either needing its own check.
    #[test]
    fn a_healthy_agent_says_nothing_however_often_it_is_woken() {
        assert_eq!(decide_announcement(None, None), None);
        assert_eq!(decide_announcement(None, None), None);
    }

    #[test]
    fn a_degraded_agent_tells_a_room_that_does_not_know() {
        assert_eq!(
            decide_announcement(degraded("engram unreachable").as_ref(), None),
            Some(Announcement::Degraded {
                reason: "engram unreachable".into()
            })
        );
    }

    #[test]
    fn it_does_not_tell_the_same_room_twice() {
        assert_eq!(
            decide_announcement(
                degraded("engram unreachable").as_ref(),
                Some(TroubleKind::Degraded)
            ),
            None
        );
    }

    #[test]
    fn recovery_is_announced_only_where_the_degradation_was() {
        assert_eq!(
            decide_announcement(None, Some(TroubleKind::Degraded)),
            Some(Announcement::Recovered)
        );
        assert_eq!(
            decide_announcement(None, Some(TroubleKind::Refusing)),
            Some(Announcement::Recovered),
            "a room told about a refusal is owed the same close"
        );
        // A room never told is never told of a recovery either -- otherwise the
        // first thing a quiet room hears is that something it never knew was
        // broken is fixed.
        assert_eq!(decide_announcement(None, None), None);
    }

    #[test]
    fn the_degraded_body_says_what_is_wrong_and_that_work_continues() {
        let body = announcement_body(&Announcement::Degraded {
            reason: "connection refused".into(),
        });
        // Option B's phrasing is load-bearing in three clauses.
        assert!(body.contains("without my assigned skill"), "{body}");
        assert!(
            body.contains("connection refused"),
            "the reason must reach the room: {body}"
        );
        assert!(
            body.contains("I will answer") && body.contains("automatically"),
            "it must say work continues and that recovery needs no human: {body}"
        );
        // Guard 4 is about tags, which the relay parses out of the BODY. An '@'
        // here would wake whoever it named.
        assert!(
            !body.contains('@'),
            "the announcement must mention nobody: {body}"
        );
    }

    #[test]
    fn the_recovery_body_claims_neither_a_skill_nor_a_cause() {
        let body = announcement_body(&Announcement::Recovered);
        assert!(body.contains("no longer applies"), "{body}");
        // ⚠️ It used to say "ENGRAM is reachable again", which is FALSE for the
        // recovery that matters most to item 5: a project that stops requiring
        // a skill while ENGRAM was reachable throughout.
        assert!(
            !body.to_lowercase().contains("reachable"),
            "the recovery must not name a cause it cannot know: {body}"
        );
        // The retry loop can recover into `no_skill_assigned`, where a claim
        // that the skill arrived would be false.
        assert!(
            !body.to_lowercase().contains("arrived"),
            "must not claim the skill itself is in place: {body}"
        );
        assert!(!body.contains('@'), "{body}");
    }

    // ── Item 5: the refusal (D20, seam v1.16 §7.1) ──────────────────────

    #[test]
    fn a_refusing_agent_tells_a_room_it_is_not_working_rather_than_that_it_is() {
        assert_eq!(
            decide_announcement(refusing("no skill is assigned").as_ref(), None),
            Some(Announcement::Refusing {
                reason: "no skill is assigned".into()
            })
        );
    }

    #[test]
    fn a_room_told_degraded_is_told_again_when_the_agent_starts_refusing() {
        // ⚠️ The case the old `bool` could not express. The retry loop can go
        // Degraded → RefuseTasks (ENGRAM comes back, the project requires a
        // skill, none is assigned). A room holding "I will answer from my
        // general instructions" would then get silence, which is the one
        // outcome that makes the earlier message a lie.
        assert_eq!(
            decide_announcement(
                refusing("still nothing assigned").as_ref(),
                Some(TroubleKind::Degraded)
            ),
            Some(Announcement::Refusing {
                reason: "still nothing assigned".into()
            })
        );
        // And the other direction, which is just as wrong to swallow: a room
        // told "I take no tasks" must hear that work has resumed, degraded.
        assert_eq!(
            decide_announcement(
                degraded("engram unreachable").as_ref(),
                Some(TroubleKind::Refusing)
            ),
            Some(Announcement::Degraded {
                reason: "engram unreachable".into()
            })
        );
    }

    #[test]
    fn the_same_kind_twice_is_still_silence_whatever_the_reason_says() {
        // A reason that changes between retries ("timed out" → "connection
        // refused") is not news to a room, and announcing it would turn the
        // retry loop into a ticker.
        assert_eq!(
            decide_announcement(refusing("reason A").as_ref(), Some(TroubleKind::Refusing)),
            None
        );
        assert_eq!(
            decide_announcement(degraded("reason B").as_ref(), Some(TroubleKind::Degraded)),
            None
        );
    }

    #[test]
    fn the_refusing_body_says_no_work_and_no_restart_and_names_no_one() {
        let body = announcement_body(&Announcement::Refusing {
            reason: "no skill is assigned and this project sets require_assigned_skill".into(),
        });
        assert!(body.contains("not taking tasks"), "{body}");
        assert!(
            body.contains("require_assigned_skill"),
            "the reason must reach the room: {body}"
        );
        assert!(
            body.contains("still running") && body.contains("nothing to restart"),
            "D20 is explicit that the agent stays up -- a room that reads this as a crash will \
             bounce the container: {body}"
        );
        // ⚠️ The sentence this must NOT contain. The degraded note promises work
        // continues; here it does not.
        assert!(
            !body.contains("I will answer"),
            "the refusal must not reuse the degraded promise: {body}"
        );
        assert!(
            !body.contains('@'),
            "guard 4: the announcement mentions nobody: {body}"
        );
    }

    #[test]
    fn refusing_reason_is_the_gate_and_degraded_does_not_open_it() {
        let _serialised = state_guard();
        clear_trouble();
        assert_eq!(refusing_reason(), None, "healthy: the gate is open");

        record_trouble(SkillTrouble::Degraded {
            reason: "engram unreachable".into(),
        });
        assert_eq!(
            refusing_reason(),
            None,
            "a degraded agent still works -- this is acceptance check 5, and a gate that \
             fires here would look like a working feature while being broken"
        );

        record_trouble(SkillTrouble::Refusing {
            reason: "no skill is assigned".into(),
        });
        assert_eq!(refusing_reason(), Some("no skill is assigned".to_string()));

        clear_trouble();
        assert_eq!(refusing_reason(), None, "and it reopens without a restart");
    }

    #[test]
    fn a_retry_that_resolves_without_installing_still_clears_the_marker() {
        // ⚠️ The bug `apply_outcome` exists to kill. The retry loop cleared the
        // marker only on `Installed`, so a retry that answered
        // `no_skill_assigned` -- ENGRAM back, nothing assigned, or the project
        // no longer requiring one -- exited with the agent still refusing. The
        // only route back was a restart, which is precisely what D20 says must
        // not be necessary.
        //
        // `StartedNormally` is chosen deliberately: it writes no `INSTALLED`,
        // so this test cannot leak a skill into another test's system prompt.
        let _serialised = state_guard();
        clear_trouble();
        record_trouble(SkillTrouble::Refusing {
            reason: "no skill is assigned".into(),
        });
        assert!(refusing_reason().is_some(), "precondition");

        apply_outcome(&StartupOutcome::StartedNormally, true);
        assert_eq!(
            refusing_reason(),
            None,
            "a resolved retry must reopen the gate with no restart"
        );
        assert_eq!(trouble(), None);
        clear_trouble();
    }

    #[test]
    fn a_trouble_knows_its_own_kind_and_reason() {
        let d = SkillTrouble::Degraded { reason: "x".into() };
        let r = SkillTrouble::Refusing { reason: "y".into() };
        assert_eq!(d.kind(), TroubleKind::Degraded);
        assert_eq!(r.kind(), TroubleKind::Refusing);
        assert_ne!(d.kind(), r.kind());
        assert_eq!(d.reason(), "x");
        assert_eq!(r.reason(), "y");
    }

    #[test]
    fn the_per_channel_state_round_trips() {
        let _serialised = state_guard();
        let ch = "round-trip-channel-a";
        clear_trouble();
        told_slot().lock().unwrap().remove(ch);

        assert_eq!(announcement_for(ch), None, "healthy: silence");

        record_trouble(SkillTrouble::Degraded {
            reason: "engram timed out".into(),
        });
        match announcement_for(ch) {
            Some(Announcement::Degraded { reason }) => assert_eq!(reason, "engram timed out"),
            other => panic!("expected a degraded announcement, got {other:?}"),
        }
        assert_eq!(announcement_for(ch), None, "and not a second time");

        clear_trouble();
        assert_eq!(
            announcement_for(ch),
            Some(Announcement::Recovered),
            "the room that was told is told of the recovery"
        );
        assert_eq!(announcement_for(ch), None, "once, then silence again");
    }

    #[test]
    fn one_room_being_told_does_not_speak_for_another() {
        let _serialised = state_guard();
        let a = "independent-channel-a";
        let b = "independent-channel-b";
        clear_trouble();
        {
            let mut told = told_slot().lock().unwrap();
            told.remove(a);
            told.remove(b);
        }
        record_trouble(SkillTrouble::Degraded {
            reason: "engram unreachable".into(),
        });
        assert!(matches!(
            announcement_for(a),
            Some(Announcement::Degraded { .. })
        ));
        // b has not been woken yet; it must still get its own announcement.
        assert!(
            matches!(announcement_for(b), Some(Announcement::Degraded { .. })),
            "an agent in five channels announces in each -- once"
        );
        clear_trouble();
        told_slot().lock().unwrap().remove(a);
        told_slot().lock().unwrap().remove(b);
    }
}
