//! Build ENGRAM's turn record (`POST /api/agent-turns`) from one turn.
//!
//! Buzz v1.4.0 Track D1b. Input: what [`crate::turn_record`] captured, the
//! turn's [`TurnUsage`], and the context only the caller knows. Output: the
//! JSON body, **serialised once** into [`TurnRecordBytes`].
//!
//! ⚠️ **Serialise once, then never rebuild.** ENGRAM keys a turn on
//! `(principal, harness, session_id, turn_seq, trigger_event_id)` and stores a
//! digest of the first body. A retry with the same key and different bytes is
//! `409 [turn_key_conflict]`, terminal. So the sender (D1d) retries the bytes
//! made here, and nothing downstream may re-run [`build_record`] for a turn
//! (API note `f7f263de`, "Idempotency").
//!
//! Contract: API note KB `f7f263de`; design *Agent Skills Phase 1 —
//! Observation* KB `195da706` §3.

use crate::turn_record::TurnObservation;
use crate::usage::TurnUsage;

/// The harness name ENGRAM keys on. Part of the idempotency key, so it is a
/// constant, never configuration.
pub(crate) const HARNESS: &str = "buzz-acp";

/// ENGRAM's limit on `harness_version`.
const HARNESS_VERSION_MAX: usize = 64;

/// The harness's own turn status, **not task success** (design §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnOutcome {
    Answered,
    Refused,
    Degraded,
    Error,
}

impl TurnOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Answered => "answered",
            Self::Refused => "refused",
            Self::Degraded => "degraded",
            Self::Error => "error",
        }
    }
}

/// ENGRAM's mapping (API note `97f63175`), in precedence order:
///
/// 1. `failed` (the turn ended in a protocol or process failure) → `error`;
/// 2. ACP `refusal` → `refused`;
/// 3. the skill is degraded (`unreadable_parts`, or no skill), or the stop is
///    `max_tokens` / `max_turn_requests` → `degraded`;
/// 4. anything else, `end_turn`, `cancelled` and unknown values included →
///    `answered`.
///
/// ⚠️ Our own `require_assigned_skill` refusal never reaches this: it drops
/// the event before a turn exists (`lib.rs`, item 5), so there is no session
/// and no record. ENGRAM's `refused` for it is unreachable from a turn.
pub(crate) fn turn_outcome(
    stop_raw: Option<&str>,
    failed: bool,
    skill_degraded: bool,
) -> TurnOutcome {
    if failed {
        return TurnOutcome::Error;
    }
    match stop_raw {
        Some("refusal") => TurnOutcome::Refused,
        Some("max_tokens" | "max_turn_requests") => TurnOutcome::Degraded,
        _ if skill_degraded => TurnOutcome::Degraded,
        _ => TurnOutcome::Answered,
    }
}

/// What only the caller knows about a turn.
#[derive(Debug, Clone)]
pub(crate) struct TurnContext {
    /// From [`harness_version`]; never a literal.
    pub harness_version: String,
    pub acp_protocol_version: Option<u64>,
    pub session_id: String,
    /// The harness's own per-session counter: exists even when usage is null.
    pub turn_seq: u64,
    pub project_id: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub ended_at: chrono::DateTime<chrono::Utc>,
    pub episode_ref: Option<String>,
    pub trigger_event_id: Option<String>,
    pub channel_id: Option<String>,
    pub outcome: TurnOutcome,
}

/// A turn record serialised once. The sender holds these bytes until
/// acknowledged and never rebuilds them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnRecordBytes(Vec<u8>);

impl TurnRecordBytes {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// `<goose version>+buzz.<build>`, at most 64 characters (API note,
/// `harness_version`). An unknown half is written `unknown` rather than
/// omitted, so the string still says which half is missing.
pub(crate) fn harness_version(goose: Option<&str>, build: Option<&str>) -> String {
    let clean = |v: Option<&str>| {
        v.map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or("unknown")
            .to_string()
    };
    let mut s = format!("{}+buzz.{}", clean(goose), clean(build));
    if s.len() > HARNESS_VERSION_MAX {
        let mut cut = HARNESS_VERSION_MAX;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    s
}

fn rfc3339(t: chrono::DateTime<chrono::Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Build the record body. Pure: the same inputs give the same value.
///
/// - **Tokens** come from `usage` only when `delta_reliable`, and only when it
///   is the same session's; otherwise null (API note, `usage`). Never the
///   `session/prompt` response's `usage`, which is the last model call.
/// - **Cost** is the tracker's per-turn delta as is. It is null on a
///   session's first turn because Goose's first `usage_update` has no cost.
/// - **`stop_reason`** is the raw value from the recorder, verbatim.
/// - **Not built here:** `skill` (D2), `self_assessment` (D3), `delivered`
///   (D1h). Absent rather than null, since each is optional.
pub(crate) fn build_record(
    ctx: &TurnContext,
    obs: &TurnObservation,
    usage: Option<&TurnUsage>,
) -> serde_json::Value {
    use serde_json::{json, Value};

    let usage = usage.filter(|u| u.session_id == ctx.session_id);
    let reliable = usage.filter(|u| u.delta_reliable);
    let usage_obj = json!({
        "input_tokens": reliable.and_then(|u| u.turn_input_tokens),
        "output_tokens": reliable.and_then(|u| u.turn_output_tokens),
        "cost_usd": usage.and_then(|u| u.turn_cost_usd),
    });

    let tools: Vec<Value> = obs
        .tools()
        .into_iter()
        .map(|t| json!({"extension": t.extension, "name": t.name, "calls": t.calls, "errors": t.errors}))
        .collect();
    let tool_calls: Vec<Value> = obs
        .tool_calls
        .iter()
        .map(|c| json!({"extension": c.extension, "name": c.name, "status": c.status}))
        .collect();

    let model = obs
        .model
        .clone()
        .or_else(|| usage.and_then(|u| u.model.clone()));

    json!({
        "harness": HARNESS,
        "harness_version": ctx.harness_version,
        "acp_protocol_version": ctx.acp_protocol_version,
        "session_id": ctx.session_id,
        "turn_seq": ctx.turn_seq,
        "project_id": ctx.project_id,
        "started_at": rfc3339(ctx.started_at),
        "ended_at": rfc3339(ctx.ended_at),
        "episode_ref": ctx.episode_ref,
        "trigger_event_id": ctx.trigger_event_id,
        "channel_id": ctx.channel_id,
        "model": model,
        "provider": obs.provider,
        "loaded": obs.loaded,
        "tools": tools,
        "tool_calls": tool_calls,
        "usage": usage_obj,
        "outcome": ctx.outcome.as_str(),
        "stop_reason": obs.stop_reason,
        "raw": {
            "channel_text": obs.channel_text,
            "reasoning": obs.reasoning,
            "tool_results": obs.tool_results,
        },
    })
}

/// Serialise a record once. This is the only constructor of
/// [`TurnRecordBytes`], so held bytes always came from one build.
pub(crate) fn serialize_record(record: &serde_json::Value) -> TurnRecordBytes {
    // A `Value` always serialises; the expect documents that, not a risk.
    TurnRecordBytes(serde_json::to_vec(record).expect("a serde_json::Value always serialises"))
}

/// What the record needs from the batch, read before the batch is consumed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TurnFacts {
    pub trigger_event_id: Option<String>,
    pub episode_ref: Option<String>,
    pub channel_id: Option<String>,
}

/// What the harness knows about itself for this record.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HarnessFacts<'a> {
    pub project: &'a str,
    pub build: Option<&'a str>,
    pub acp_protocol_version: Option<u64>,
    /// Goose's `agentInfo.version` from `initialize`.
    pub agent_version: Option<&'a str>,
}

/// One turn's record, serialised once. The single call site is the pool's
/// end-of-turn helper; the bytes go straight to the sender.
pub(crate) fn record_for_turn(
    harness: &HarnessFacts<'_>,
    facts: &TurnFacts,
    obs: &TurnObservation,
    usage: Option<&TurnUsage>,
    outcome: TurnOutcome,
    ended_at: chrono::DateTime<chrono::Utc>,
) -> TurnRecordBytes {
    let ctx = TurnContext {
        harness_version: harness_version(harness.agent_version, harness.build),
        acp_protocol_version: harness.acp_protocol_version,
        session_id: obs.session_id.clone(),
        turn_seq: obs.turn_seq,
        project_id: harness.project.to_string(),
        started_at: obs.started_at.unwrap_or(ended_at),
        ended_at,
        episode_ref: facts.episode_ref.clone(),
        trigger_event_id: facts.trigger_event_id.clone(),
        channel_id: facts.channel_id.clone(),
        outcome,
    };
    serialize_record(&build_record(&ctx, obs, usage))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::turn_record::TurnRecorder;
    use serde_json::json;

    #[test]
    fn record_for_turn_takes_session_seq_and_start_from_the_observation() {
        let mut r = TurnRecorder::default();
        r.begin_turn("20261117_1");
        r.begin_turn("20261117_1");
        r.record_stop_reason("end_turn");
        let obs = r.take().unwrap();
        let ended = chrono::Utc::now();
        let facts = TurnFacts {
            trigger_event_id: Some("cd".repeat(32)),
            episode_ref: Some("ef".repeat(32)),
            channel_id: Some("7dfc1946-10d3-4497-8080-94db7575fc70".into()),
        };
        let harness = HarnessFacts {
            project: "project:b5cb944650f21b76",
            build: Some("1.4.0"),
            acp_protocol_version: Some(1),
            agent_version: Some("1.57.0"),
        };
        let bytes = record_for_turn(&harness, &facts, &obs, None, TurnOutcome::Answered, ended);
        let v: serde_json::Value = serde_json::from_slice(bytes.as_bytes()).unwrap();
        assert_eq!(v["session_id"], "20261117_1");
        assert_eq!(v["turn_seq"], 2);
        assert_eq!(v["harness_version"], "1.57.0+buzz.1.4.0");
        assert_eq!(v["project_id"], "project:b5cb944650f21b76");
        assert_eq!(v["trigger_event_id"], "cd".repeat(32));
        assert_eq!(v["episode_ref"], "ef".repeat(32));
        assert_eq!(v["outcome"], "answered");
        assert_eq!(v["stop_reason"], "end_turn");
        let started =
            chrono::DateTime::parse_from_rfc3339(v["started_at"].as_str().unwrap()).unwrap();
        let ended_s =
            chrono::DateTime::parse_from_rfc3339(v["ended_at"].as_str().unwrap()).unwrap();
        assert!(started <= ended_s);
        // Null tokens, not zero, with no usage at all.
        assert!(v["usage"]["input_tokens"].is_null());
    }

    #[test]
    fn outcome_follows_engrams_mapping_in_precedence_order() {
        use TurnOutcome::*;
        let cases: &[(Option<&str>, bool, bool, TurnOutcome)] = &[
            (Some("end_turn"), false, false, Answered),
            (Some("cancelled"), false, false, Answered),
            (Some("something_new"), false, false, Answered),
            (None, false, false, Answered),
            (Some("refusal"), false, false, Refused),
            (Some("refusal"), false, true, Refused),
            (Some("max_tokens"), false, false, Degraded),
            (Some("max_turn_requests"), false, false, Degraded),
            (Some("end_turn"), false, true, Degraded),
            (Some("end_turn"), true, false, Error),
            (Some("refusal"), true, true, Error),
            (None, true, false, Error),
        ];
        for (stop, failed, degraded, want) in cases {
            assert_eq!(
                turn_outcome(*stop, *failed, *degraded),
                *want,
                "{stop:?} {failed} {degraded}"
            );
        }
    }

    fn ctx() -> TurnContext {
        let t = |s: &str| {
            chrono::DateTime::parse_from_rfc3339(s)
                .unwrap()
                .with_timezone(&chrono::Utc)
        };
        TurnContext {
            harness_version: harness_version(Some("1.57.0"), Some("1.4.0")),
            acp_protocol_version: Some(1),
            session_id: "20261117_1".into(),
            turn_seq: 3,
            project_id: "project:b5cb944650f21b76".into(),
            started_at: t("2026-11-17T20:00:00Z"),
            ended_at: t("2026-11-17T20:00:41.250Z"),
            episode_ref: None,
            trigger_event_id: Some("ab".repeat(32)),
            channel_id: Some("7dfc1946-10d3-4497-8080-94db7575fc70".into()),
            outcome: TurnOutcome::Answered,
        }
    }

    fn usage(session: &str, reliable: bool) -> TurnUsage {
        TurnUsage {
            session_id: session.into(),
            turn_seq: 9,
            delta_reliable: reliable,
            turn_input_tokens: Some(464_872),
            turn_output_tokens: Some(1_528),
            turn_total_tokens: None,
            turn_cost_usd: Some(0.42),
            turn_cache_read_tokens: None,
            turn_cache_write_tokens: None,
            cumulative_input_tokens: None,
            cumulative_output_tokens: None,
            cumulative_total_tokens: None,
            cumulative_cost_usd: None,
            cumulative_cache_read_tokens: None,
            cumulative_cache_write_tokens: None,
            model: Some("from-usage".into()),
            pricing_identity: None,
        }
    }

    fn observed() -> TurnObservation {
        let mut r = TurnRecorder::default();
        r.begin_turn("20261117_1");
        let up = |u: serde_json::Value| json!({"method":"session/update","params":{"sessionId":"20261117_1","update":u}});
        r.observe(&up(
            json!({"sessionUpdate":"tool_call","toolCallId":"a","rawInput":{"name":"buzz-coder"},
            "_meta":{"goose":{"toolCall":{"toolName":"load_skill"}}}}),
        ));
        r.observe(&up(
            json!({"sessionUpdate":"tool_call_update","toolCallId":"a","status":"completed"}),
        ));
        r.observe(&up(json!({"sessionUpdate":"tool_call","toolCallId":"b",
            "_meta":{"goose":{"toolCall":{"toolName":"shell","extensionName":"developer"}}}})));
        r.observe(&up(
            json!({"sessionUpdate":"tool_call_update","toolCallId":"b","status":"in_progress"}),
        ));
        r.observe(&up(
            json!({"sessionUpdate":"tool_call_update","toolCallId":"b","status":"failed"}),
        ));
        r.observe(&up(json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"done"},
            "_meta":{"goose":{"inference":{"provider":"anthropic","requestedModel":"claude-sonnet-5"}}}})));
        r.record_stop_reason("end_turn");
        r.take().unwrap()
    }

    #[test]
    fn the_record_matches_the_api_note_shape() {
        let v = build_record(&ctx(), &observed(), Some(&usage("20261117_1", true)));
        assert_eq!(v["harness"], "buzz-acp");
        assert_eq!(v["harness_version"], "1.57.0+buzz.1.4.0");
        assert_eq!(v["acp_protocol_version"], 1);
        assert_eq!(v["turn_seq"], 3, "the harness's own counter, not usage's");
        assert_eq!(v["started_at"], "2026-11-17T20:00:00.000Z");
        assert_eq!(v["ended_at"], "2026-11-17T20:00:41.250Z");
        assert_eq!(v["outcome"], "answered");
        assert_eq!(v["stop_reason"], "end_turn");
        assert_eq!(
            v["model"], "claude-sonnet-5",
            "the stream's model beats usage's"
        );
        assert_eq!(v["provider"], "anthropic");
        assert_eq!(v["loaded"], json!(["buzz-coder"]));
        assert_eq!(
            v["tool_calls"],
            json!([{"extension":null,"name":"load_skill","status":"completed"},
                   {"extension":"developer","name":"shell","status":"failed"}])
        );
        assert_eq!(
            v["tools"],
            json!([{"extension":null,"name":"load_skill","calls":1,"errors":0},
                   {"extension":"developer","name":"shell","calls":1,"errors":1}])
        );
        assert_eq!(
            v["usage"],
            json!({"input_tokens":464872,"output_tokens":1528,"cost_usd":0.42})
        );
        assert_eq!(v["raw"]["channel_text"], "done");
        for absent in ["skill", "self_assessment", "delivered", "user_id"] {
            assert!(v.get(absent).is_none(), "{absent} must be absent in D1b");
        }
    }

    #[test]
    fn unreliable_usage_gives_null_tokens_never_a_guess() {
        let v = build_record(&ctx(), &observed(), Some(&usage("20261117_1", false)));
        assert_eq!(v["usage"]["input_tokens"], serde_json::Value::Null);
        assert_eq!(v["usage"]["output_tokens"], serde_json::Value::Null);
    }

    #[test]
    fn another_sessions_usage_is_not_attached() {
        let v = build_record(&ctx(), &observed(), Some(&usage("other-session", true)));
        assert_eq!(
            v["usage"],
            json!({"input_tokens":null,"output_tokens":null,"cost_usd":null})
        );
    }

    #[test]
    fn no_usage_still_makes_a_record() {
        let v = build_record(&ctx(), &observed(), None);
        assert_eq!(
            v["usage"],
            json!({"input_tokens":null,"output_tokens":null,"cost_usd":null})
        );
        assert_eq!(v["model"], "claude-sonnet-5");
    }

    #[test]
    fn harness_version_is_derived_and_bounded() {
        assert_eq!(
            harness_version(Some("1.57.0"), Some("1.4.0")),
            "1.57.0+buzz.1.4.0"
        );
        assert_eq!(harness_version(None, Some("1.4.0")), "unknown+buzz.1.4.0");
        assert_eq!(harness_version(Some(" "), None), "unknown+buzz.unknown");
        let long = harness_version(Some(&"9".repeat(80)), Some("1.4.0"));
        assert_eq!(long.len(), 64);
    }

    #[test]
    fn serialising_once_is_stable_for_the_same_value() {
        let v = build_record(&ctx(), &observed(), None);
        assert_eq!(serialize_record(&v), serialize_record(&v));
        let parsed: serde_json::Value =
            serde_json::from_slice(serialize_record(&v).as_bytes()).unwrap();
        assert_eq!(parsed, v);
    }
}
