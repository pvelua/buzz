//! Send ENGRAM's turn record (`POST /api/agent-turns`) off the turn path.
//!
//! Buzz v1.4.0 Track D1c (configuration) and D1d (the sender). The record is
//! built and serialised once by [`crate::turn_payload`]; this module only
//! moves those bytes, and never rebuilds them.
//!
//! Contract: API note KB `03f40ec8` ("Transport", "Idempotency",
//! "Responses"). The response table, as implemented:
//!
//! | response | here |
//! |---|---|
//! | 200 `stored` / `duplicate` | delivered |
//! | network failure, 5xx, 429 | retry **the same bytes**, with backoff |
//! | 409 `[turn_key_conflict]` | **terminal**: the first write landed. Delivered, one warning |
//! | 404 `[observation_disabled]` | **stop sending** for the life of the process |
//! | any other 404 | misconfiguration: logged, not retried, does NOT stop the sender |
//! | 400, 401, 403, 422 | rejected: logged with ENGRAM's detail, not retried |
//!
//! ⚠️ **Only the 404 whose body says `[observation_disabled]` is a switch.**
//! A bare 404 is a wrong URL or a server too old for the route, and treating
//! it as "off" would hide a misconfiguration behind silence.
//!
//! ⚠️ **Logs use this module's own target (`buzz_acp::turn_sender`), never a
//! custom one.** The agents filter on `buzz_acp=info`, which matches targets
//! by prefix, so a line under `target: "turn_record::send"` never prints at
//! the default level. Measured on the DEV coder 2026-11-17: in three days of
//! log, zero lines from upstream's `pool::prompt` target, while every
//! `buzz_acp::engram_skill` line is there. The first cut of this sender used a
//! custom target, which would have made even "NOT started" silent.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::turn_payload::TurnRecordBytes;

/// How far one HTTP call may run.
pub(crate) const SEND_HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// Waits between attempts after a retryable failure. Six attempts in all,
/// about six and a half minutes, then the record is dropped with a warning:
/// a record is evidence, but holding a turn's bytes for ever would grow
/// without bound while ENGRAM is down.
pub(crate) const RETRY_BACKOFF: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(4),
    Duration::from_secs(15),
    Duration::from_secs(60),
    Duration::from_secs(300),
];

/// Records waiting to be sent. Beyond this a new record is dropped with a
/// warning rather than blocking the turn that made it.
const QUEUE_DEPTH: usize = 64;

// ── D1c: configuration ──────────────────────────────────────────────────────

/// Where and as whom to send. Read **independently of `ENGRAM_SKILL_FETCH`**:
/// observation must not require instruction fetch to be on (API note, "No
/// project, no record").
#[derive(Clone)]
pub(crate) struct ObservationConfig {
    pub endpoint: String,
    pat: String,
    pub project: String,
    /// `ENGRAM_TRIGGER_TEXT`: also send the trigger message's text as
    /// `raw.trigger_text`. Default OFF, and separate from `ENGRAM_OBSERVE`,
    /// because ENGRAM accepts the field only from v1.22.1 and an older ENGRAM
    /// drops it silently (API note `895b40ca`); it is also the one bounded
    /// copy of conversation into ENGRAM (seam contract v1.18 §1), so it is
    /// switched on per stack, deliberately.
    pub trigger_text: bool,
}

impl std::fmt::Debug for ObservationConfig {
    // The PAT is a credential: never in a log line, never in a panic.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObservationConfig")
            .field("endpoint", &self.endpoint)
            .field("pat", &"<redacted>")
            .field("project", &self.project)
            .field("trigger_text", &self.trigger_text)
            .finish()
    }
}

impl ObservationConfig {
    /// `Ok(None)`: the switch `ENGRAM_OBSERVE` is off, so do nothing and say
    /// nothing. `Err`: the switch is on and the configuration is incomplete;
    /// the caller reports it loudly and the agent still starts.
    pub fn from_env() -> Result<Option<Self>, String> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    pub fn from_lookup<F>(get: F) -> Result<Option<Self>, String>
    where
        F: Fn(&str) -> Option<String>,
    {
        let text = |name: &str| -> Option<String> {
            get(name)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let on = matches!(
            text("ENGRAM_OBSERVE")
                .unwrap_or_default()
                .to_ascii_lowercase()
                .as_str(),
            "1" | "true" | "yes" | "on"
        );
        if !on {
            return Ok(None);
        }
        let base = match text("ENGRAM_API_URL") {
            Some(base) => base,
            None => text("ENGRAM_MCP_URL").ok_or(
                "ENGRAM_OBSERVE is on but neither ENGRAM_API_URL nor ENGRAM_MCP_URL is set",
            )?,
        };
        let endpoint = endpoint_from(&base, text("ENGRAM_API_URL").is_some())?;
        let pat =
            text("ENGRAM_PAT").ok_or("ENGRAM_OBSERVE is on but ENGRAM_PAT is unset or empty")?;
        let project = text("ENGRAM_PROJECT").ok_or(
            "ENGRAM_OBSERVE is on but ENGRAM_PROJECT is unset or empty: no project, no record",
        )?;
        let trigger_text = matches!(
            text("ENGRAM_TRIGGER_TEXT")
                .unwrap_or_default()
                .to_ascii_lowercase()
                .as_str(),
            "1" | "true" | "yes" | "on"
        );
        Ok(Some(Self {
            endpoint,
            pat,
            project,
            trigger_text,
        }))
    }
}

/// `…/api/agent-turns` from a base. An explicit `ENGRAM_API_URL` keeps its
/// path as a prefix; an `ENGRAM_MCP_URL` contributes only its origin, since
/// its path (`/mcp/`) is the MCP route, not a prefix.
fn endpoint_from(base: &str, explicit: bool) -> Result<String, String> {
    let url =
        reqwest::Url::parse(base).map_err(|e| format!("cannot parse ENGRAM URL {base:?}: {e}"))?;
    if !matches!(url.scheme(), "https" | "http") {
        return Err(format!("ENGRAM URL {base:?} is not http(s)"));
    }
    let prefix = if explicit {
        url.path().trim_end_matches('/')
    } else {
        ""
    };
    let mut out = url.clone();
    out.set_path(&format!("{prefix}/api/agent-turns"));
    out.set_query(None);
    out.set_fragment(None);
    Ok(out.to_string())
}

// ── D1d: the response table ─────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Disposition {
    Stored,
    Duplicate,
    /// `409 [turn_key_conflict]`: terminal, counts as delivered.
    Conflict,
    /// `404 [observation_disabled]`: stop sending.
    Disabled,
    /// Any other 404: wrong URL or a server too old for the route.
    Misconfigured,
    /// 400/401/403/422 and other client errors: not retried.
    Rejected(u16),
    /// 5xx, 429, or a network failure: retry the same bytes.
    Retry,
}

/// Classify one HTTP response. Pure, so every row of the table is a test.
pub(crate) fn classify(status: u16, body: &str) -> Disposition {
    match status {
        200..=299 => {
            let outcome = serde_json::from_str::<serde_json::Value>(body)
                .ok()
                .and_then(|v| {
                    v.get("outcome")
                        .and_then(|o| o.as_str())
                        .map(str::to_string)
                });
            match outcome.as_deref() {
                Some("duplicate") => Disposition::Duplicate,
                // `stored`, or a 2xx we cannot read: ENGRAM accepted it either way.
                _ => Disposition::Stored,
            }
        }
        404 if body.contains("[observation_disabled]") => Disposition::Disabled,
        404 => Disposition::Misconfigured,
        409 if body.contains("[turn_key_conflict]") => Disposition::Conflict,
        429 | 500..=599 => Disposition::Retry,
        other => Disposition::Rejected(other),
    }
}

// ── D1d: the sender ─────────────────────────────────────────────────────────

/// A handle to the background sender. Cheap to clone; `submit` never blocks.
#[derive(Clone)]
pub(crate) struct TurnSender {
    tx: tokio::sync::mpsc::Sender<TurnRecordBytes>,
    disabled: Arc<AtomicBool>,
}

impl TurnSender {
    /// Start the background task. Must be called inside a Tokio runtime.
    pub fn spawn(config: ObservationConfig) -> Result<Self, String> {
        Self::spawn_with(config, SEND_HTTP_TIMEOUT, RETRY_BACKOFF.to_vec())
    }

    pub(crate) fn spawn_with(
        config: ObservationConfig,
        timeout: Duration,
        backoff: Vec<Duration>,
    ) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| format!("cannot build the turn-record HTTP client: {e}"))?;
        let (tx, rx) = tokio::sync::mpsc::channel(QUEUE_DEPTH);
        let disabled = Arc::new(AtomicBool::new(false));
        tokio::spawn(run(http, config, backoff, rx, disabled.clone()));
        Ok(Self { tx, disabled })
    }

    /// Queue a record. Returns at once; a full queue drops the record with a
    /// warning, and a disabled sender drops it silently.
    pub fn submit(&self, record: TurnRecordBytes) {
        if self.disabled.load(Ordering::Relaxed) {
            return;
        }
        match self.tx.try_send(record) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(r)) => tracing::warn!(
                turn = %describe(&r),
                "turn-record queue full ({QUEUE_DEPTH}); dropping this record"
            ),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                tracing::debug!("turn-record sender has stopped; record not queued")
            }
        }
    }

    /// True once ENGRAM has answered `[observation_disabled]`. Test-only:
    /// `submit` reads the flag itself, so nothing else needs to.
    #[cfg(test)]
    pub fn is_disabled(&self) -> bool {
        self.disabled.load(Ordering::Relaxed)
    }
}

/// What a finished turn needs to become a record, built once at startup and
/// shared by every prompt task through `PromptContext`.
#[derive(Clone)]
pub(crate) struct Observation {
    pub sender: TurnSender,
    pub project: String,
    /// The harness build, the `+buzz.<build>` half of `harness_version`.
    /// From `BUZZ_BUILD`, which the image bakes in (D1f); `None` is written
    /// `unknown`, never guessed.
    pub build: Option<String>,
    /// Send `raw.trigger_text` (see [`ObservationConfig::trigger_text`]).
    pub trigger_text: bool,
}

impl Observation {
    /// `None` when `ENGRAM_OBSERVE` is off (silently) or the configuration is
    /// incomplete (one ERROR line naming what is missing). Either way the agent
    /// starts: observation must never stop an agent answering. Must be called
    /// inside a Tokio runtime.
    pub fn start() -> Option<Self> {
        let config = match ObservationConfig::from_env() {
            Ok(None) => return None,
            Ok(Some(config)) => config,
            Err(e) => {
                tracing::error!("turn observation: NOT started: {e}");
                return None;
            }
        };
        let project = config.project.clone();
        let endpoint = config.endpoint.clone();
        let trigger_text = config.trigger_text;
        let sender = match TurnSender::spawn(config) {
            Ok(sender) => sender,
            Err(e) => {
                tracing::error!("turn observation: NOT started: {e}");
                return None;
            }
        };
        let build = std::env::var("BUZZ_BUILD")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        tracing::info!(
            "turn observation: enabled, endpoint={endpoint} project={project} build={} trigger_text={}",
            build.as_deref().unwrap_or("unknown"),
            if trigger_text { "on" } else { "off" }
        );
        Some(Self {
            sender,
            project,
            build,
            trigger_text,
        })
    }
}

/// `session_id#turn_seq` for log lines, read back from the bytes. Never the
/// raw material, never the PAT.
fn describe(record: &TurnRecordBytes) -> String {
    serde_json::from_slice::<serde_json::Value>(record.as_bytes())
        .ok()
        .map(|v| {
            format!(
                "{}#{}",
                v["session_id"].as_str().unwrap_or("?"),
                v["turn_seq"].as_u64().map_or("?".into(), |n| n.to_string())
            )
        })
        .unwrap_or_else(|| "?".into())
}

async fn run(
    http: reqwest::Client,
    config: ObservationConfig,
    backoff: Vec<Duration>,
    mut rx: tokio::sync::mpsc::Receiver<TurnRecordBytes>,
    disabled: Arc<AtomicBool>,
) {
    while let Some(record) = rx.recv().await {
        if disabled.load(Ordering::Relaxed) {
            continue;
        }
        let turn = describe(&record);
        let mut attempt = 0usize;
        loop {
            let result = http
                .post(&config.endpoint)
                .bearer_auth(&config.pat)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                // The same bytes on every attempt: `TurnRecordBytes` is only
                // ever made by `serialize_record`, once per turn.
                .body(record.as_bytes().to_vec())
                .send()
                .await;
            let disposition = match result {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let body = resp.text().await.unwrap_or_default();
                    let d = classify(status, &body);
                    log_disposition(&turn, &d, status, &body);
                    d
                }
                Err(e) => {
                    tracing::warn!(%turn, "send failed: {e}");
                    Disposition::Retry
                }
            };
            match disposition {
                Disposition::Retry => match backoff.get(attempt) {
                    Some(wait) => {
                        attempt += 1;
                        tokio::time::sleep(*wait).await;
                    }
                    None => {
                        tracing::warn!(
                            %turn,
                            attempts = attempt + 1,
                            "giving up on this turn record after retries"
                        );
                        break;
                    }
                },
                Disposition::Disabled => {
                    disabled.store(true, Ordering::Relaxed);
                    break;
                }
                _ => break,
            }
        }
    }
}

fn log_disposition(turn: &str, d: &Disposition, status: u16, body: &str) {
    // ENGRAM's errors are `{"detail": "…"}`; quote that, bounded.
    let detail: String = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["detail"].as_str().map(str::to_string))
        .unwrap_or_else(|| body.to_string())
        .chars()
        .take(300)
        .collect();
    match d {
        Disposition::Stored => {
            tracing::info!(%turn, "turn record stored")
        }
        Disposition::Duplicate => {
            tracing::info!(%turn, "turn record already held (duplicate)")
        }
        Disposition::Conflict => tracing::warn!(
            %turn,
            "409 turn_key_conflict: the first write stands. This is a SENDER bug -- a record was \
             rebuilt instead of resent. Not retried."
        ),
        Disposition::Disabled => tracing::info!(
            %turn,
            "ENGRAM says observation is disabled; no further turn records will be sent"
        ),
        Disposition::Misconfigured => tracing::error!(
            %turn,
            "404 without [observation_disabled]: wrong ENGRAM URL, or a server too old for \
             /api/agent-turns ({detail})"
        ),
        Disposition::Rejected(code) => tracing::warn!(
            %turn,
            "turn record rejected with {code}: {detail}"
        ),
        Disposition::Retry => tracing::warn!(
            %turn,
            "retryable {status}: {detail}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn off_unless_switched_on_even_with_everything_else_set() {
        let got = ObservationConfig::from_lookup(lookup(&[
            ("ENGRAM_MCP_URL", "https://engram-dev.example/mcp/"),
            ("ENGRAM_PAT", "pat"),
            ("ENGRAM_PROJECT", "project:x"),
        ]));
        assert!(got.unwrap().is_none());
    }

    #[test]
    fn independent_of_skill_fetch_and_derives_the_endpoint_from_the_mcp_origin() {
        let cfg = ObservationConfig::from_lookup(lookup(&[
            ("ENGRAM_OBSERVE", "1"),
            ("ENGRAM_SKILL_FETCH", "0"),
            ("ENGRAM_MCP_URL", "https://engram-dev.example/mcp/"),
            ("ENGRAM_PAT", "pat"),
            ("ENGRAM_PROJECT", "project:b5cb944650f21b76"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(cfg.endpoint, "https://engram-dev.example/api/agent-turns");
        assert_eq!(cfg.project, "project:b5cb944650f21b76");
    }

    /// `raw.trigger_text` is OFF unless asked for, whatever `ENGRAM_OBSERVE`
    /// says; on accepts the same spellings as `ENGRAM_OBSERVE`.
    #[test]
    fn trigger_text_is_off_by_default_and_on_only_when_asked() {
        let base = [
            ("ENGRAM_OBSERVE", "1"),
            ("ENGRAM_MCP_URL", "https://e.example/mcp/"),
            ("ENGRAM_PAT", "pat"),
            ("ENGRAM_PROJECT", "project:x"),
        ];
        let with = |v: Option<&str>| {
            let mut pairs = base.to_vec();
            if let Some(v) = v {
                pairs.push(("ENGRAM_TRIGGER_TEXT", v));
            }
            ObservationConfig::from_lookup(lookup(&pairs))
                .unwrap()
                .unwrap()
                .trigger_text
        };
        assert!(!with(None));
        assert!(!with(Some("")));
        assert!(!with(Some("0")));
        assert!(!with(Some("off")));
        assert!(with(Some("1")));
        assert!(with(Some("ON")));
    }

    #[test]
    fn an_explicit_api_url_keeps_its_path_prefix() {
        assert_eq!(
            endpoint_from("https://h.example/engram/", true).unwrap(),
            "https://h.example/engram/api/agent-turns"
        );
        assert_eq!(
            endpoint_from("https://h.example/mcp/?x=1", false).unwrap(),
            "https://h.example/api/agent-turns"
        );
        assert!(endpoint_from("ftp://h.example/", false).is_err());
    }

    #[test]
    fn switched_on_but_incomplete_is_an_error_naming_what_is_missing() {
        let no_project = ObservationConfig::from_lookup(lookup(&[
            ("ENGRAM_OBSERVE", "on"),
            ("ENGRAM_MCP_URL", "https://e.example/mcp/"),
            ("ENGRAM_PAT", "pat"),
        ]));
        assert!(no_project.unwrap_err().contains("ENGRAM_PROJECT"));
        let no_pat = ObservationConfig::from_lookup(lookup(&[
            ("ENGRAM_OBSERVE", "on"),
            ("ENGRAM_MCP_URL", "https://e.example/mcp/"),
            ("ENGRAM_PROJECT", "project:x"),
        ]));
        assert!(no_pat.unwrap_err().contains("ENGRAM_PAT"));
    }

    #[test]
    fn the_pat_never_appears_in_debug_output() {
        let cfg = ObservationConfig::from_lookup(lookup(&[
            ("ENGRAM_OBSERVE", "1"),
            ("ENGRAM_MCP_URL", "https://e.example/mcp/"),
            ("ENGRAM_PAT", "engram_pat_SECRET123"),
            ("ENGRAM_PROJECT", "project:x"),
        ]))
        .unwrap()
        .unwrap();
        assert!(!format!("{cfg:?}").contains("SECRET123"));
    }

    #[test]
    fn every_row_of_the_response_table() {
        assert_eq!(
            classify(200, r#"{"outcome":"stored","turn_key":"k"}"#),
            Disposition::Stored
        );
        assert_eq!(
            classify(200, r#"{"outcome":"duplicate","turn_key":"k"}"#),
            Disposition::Duplicate
        );
        assert_eq!(classify(200, "not json"), Disposition::Stored);
        assert_eq!(
            classify(
                404,
                r#"{"detail":"[observation_disabled] Agent observation is not enabled"}"#
            ),
            Disposition::Disabled
        );
        assert_eq!(
            classify(404, r#"{"detail":"Not Found"}"#),
            Disposition::Misconfigured
        );
        assert_eq!(
            classify(404, ""),
            Disposition::Misconfigured,
            "a bare 404 is not a switch"
        );
        assert_eq!(
            classify(409, r#"{"detail":"[turn_key_conflict] …"}"#),
            Disposition::Conflict
        );
        assert_eq!(
            classify(409, r#"{"detail":"other"}"#),
            Disposition::Rejected(409)
        );
        assert_eq!(classify(422, "{}"), Disposition::Rejected(422));
        assert_eq!(
            classify(403, r#"{"detail":"[not_in_project]"}"#),
            Disposition::Rejected(403)
        );
        assert_eq!(classify(401, ""), Disposition::Rejected(401));
        assert_eq!(
            classify(502, "Code Session Ingest unavailable"),
            Disposition::Retry
        );
        assert_eq!(classify(429, ""), Disposition::Retry);
    }

    /// The exact bodies ENGRAM DEV returned on 2026-11-17 (D1g route probe,
    /// session `d1g-probe-20261117T232037Z`, as the DEV coder `user_067`).
    /// If ENGRAM rewords a refusal, this is where it shows.
    #[test]
    fn classify_matches_engram_devs_live_responses() {
        let stored = r#"{"outcome":"stored","turn_key":"8923c316d77e19c3f4fb88a9c48b29242a680b964c35c338fe93a386e6e6fb43","blob_stored":true,"blob_truncated":false,"skill_unresolved":false}"#;
        let duplicate = r#"{"outcome":"duplicate","turn_key":"8923c316d77e19c3f4fb88a9c48b29242a680b964c35c338fe93a386e6e6fb43"}"#;
        let conflict = r#"{"detail":"[turn_key_conflict] A turn with this key was already stored with a different body; a retry must resend the same record"}"#;
        let not_found = r#"{"detail":"Not Found"}"#;
        assert_eq!(classify(200, stored), Disposition::Stored);
        assert_eq!(classify(200, duplicate), Disposition::Duplicate);
        assert_eq!(classify(409, conflict), Disposition::Conflict);
        assert_eq!(classify(404, not_found), Disposition::Misconfigured);
    }

    // ── against a real HTTP server ──────────────────────────────────────────

    struct Server {
        url: String,
        bodies: Arc<Mutex<Vec<Vec<u8>>>>,
        auth: Arc<Mutex<Vec<String>>>,
    }

    /// A server that answers each request with the next scripted
    /// `(status, body)`, repeating the last one.
    async fn server(script: Vec<(u16, &'static str)>) -> Server {
        use axum::{body::Bytes, http::HeaderMap, routing::post, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let auth = Arc::new(Mutex::new(Vec::new()));
        let (b, a) = (bodies.clone(), auth.clone());
        let script = Arc::new(script);
        let app = Router::new().route(
            "/api/agent-turns",
            post(move |headers: HeaderMap, body: Bytes| {
                let (b, a, script) = (b.clone(), a.clone(), script.clone());
                async move {
                    let n = {
                        let mut b = b.lock().unwrap();
                        b.push(body.to_vec());
                        b.len()
                    };
                    a.lock().unwrap().push(
                        headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string(),
                    );
                    let (status, text) = script[(n - 1).min(script.len() - 1)];
                    (axum::http::StatusCode::from_u16(status).unwrap(), text)
                }
            }),
        );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Server { url, bodies, auth }
    }

    fn config(url: &str) -> ObservationConfig {
        ObservationConfig {
            endpoint: format!("{url}/api/agent-turns"),
            pat: "engram_pat_test".into(),
            project: "project:x".into(),
            trigger_text: false,
        }
    }

    fn record(turn_seq: u64) -> TurnRecordBytes {
        crate::turn_payload::serialize_record(&serde_json::json!({
            "harness": "buzz-acp", "session_id": "s", "turn_seq": turn_seq
        }))
    }

    async fn wait_for(cond: impl Fn() -> bool) {
        for _ in 0..200 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition not reached in 2 s");
    }

    #[tokio::test]
    async fn a_5xx_is_retried_with_the_same_bytes_and_the_pat_as_bearer() {
        let srv = server(vec![
            (503, "busy"),
            (502, "Code Session Ingest unavailable"),
            (200, r#"{"outcome":"stored"}"#),
        ])
        .await;
        let sender = TurnSender::spawn_with(
            config(&srv.url),
            Duration::from_secs(5),
            vec![Duration::from_millis(5); 5],
        )
        .unwrap();
        let rec = record(1);
        sender.submit(rec.clone());
        wait_for(|| srv.bodies.lock().unwrap().len() == 3).await;
        let bodies = srv.bodies.lock().unwrap().clone();
        assert!(
            bodies.iter().all(|b| b == rec.as_bytes()),
            "every attempt sent the same bytes"
        );
        assert!(srv
            .auth
            .lock()
            .unwrap()
            .iter()
            .all(|a| a == "Bearer engram_pat_test"));
    }

    #[tokio::test]
    async fn a_409_conflict_is_terminal_and_not_retried() {
        let srv = server(vec![(409, r#"{"detail":"[turn_key_conflict] x"}"#)]).await;
        let sender = TurnSender::spawn_with(
            config(&srv.url),
            Duration::from_secs(5),
            vec![Duration::from_millis(5); 5],
        )
        .unwrap();
        sender.submit(record(1));
        wait_for(|| srv.bodies.lock().unwrap().len() == 1).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(srv.bodies.lock().unwrap().len(), 1, "no retry after a 409");
        assert!(!sender.is_disabled());
    }

    #[tokio::test]
    async fn observation_disabled_stops_the_sender_and_a_bare_404_does_not() {
        // A bare 404 first: logged as misconfiguration, sender keeps going.
        let srv = server(vec![
            (404, r#"{"detail":"Not Found"}"#),
            (404, r#"{"detail":"[observation_disabled] off"}"#),
        ])
        .await;
        let sender = TurnSender::spawn_with(
            config(&srv.url),
            Duration::from_secs(5),
            vec![Duration::from_millis(5); 5],
        )
        .unwrap();
        sender.submit(record(1));
        wait_for(|| srv.bodies.lock().unwrap().len() == 1).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!sender.is_disabled(), "a bare 404 is not a switch");

        sender.submit(record(2));
        wait_for(|| sender.is_disabled()).await;
        sender.submit(record(3));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            srv.bodies.lock().unwrap().len(),
            2,
            "nothing sent once disabled"
        );
    }

    #[tokio::test]
    async fn retries_are_bounded() {
        let srv = server(vec![(503, "down")]).await;
        let sender = TurnSender::spawn_with(
            config(&srv.url),
            Duration::from_secs(5),
            vec![Duration::from_millis(5); 2],
        )
        .unwrap();
        sender.submit(record(1));
        wait_for(|| srv.bodies.lock().unwrap().len() == 3).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            srv.bodies.lock().unwrap().len(),
            3,
            "1 attempt + 2 retries, then give up"
        );
    }
}
