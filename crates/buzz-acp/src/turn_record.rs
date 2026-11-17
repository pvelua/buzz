//! Per-turn capture for ENGRAM's turn record (`POST /api/agent-turns`).
//!
//! Buzz v1.4.0 Track D1a. This module only **records**: it watches the ACP
//! `session/update` stream for one turn and keeps what the record needs.
//! Building the JSON body and sending it are later tasks (D1b, D1d).
//!
//! The contract is ENGRAM's API note `POST /api/agent-turns` (KB `ac3acaef`,
//! version 3). Three of its rules come from our own DEV capture, and each is
//! a place where the obvious reader is wrong:
//!
//! - **A tool is the pair `(extension, name)`.** `name` is Goose's bare
//!   `_meta.goose.toolCall.toolName` (`shell`, never `developer__shell`), and
//!   `extension` is `extensionName`, which is **absent for a builtin** such as
//!   `load_skill`. ACP's own `name` field wins when an agent sends it. The
//!   display `title` is never a key.
//! - **A call's status is its LAST TERMINAL update.** The first
//!   `tool_call_update` is `in_progress`, so a reader that takes the first
//!   match records no errors at all — a silent zero-error rate.
//! - **`tool_calls` is in order of FIRST SIGHT of each `toolCallId`, whichever
//!   variant arrives first.** ACP v2 drops `tool_call` and makes
//!   `tool_call_update` an upsert, so either may be the first we see.

use std::collections::HashMap;

/// A status after which a tool call cannot change again.
fn is_terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed")
}

/// Upper bound on each captured raw text field, in bytes. ENGRAM caps the
/// compressed blob at 2 MiB and marks a larger one truncated; this bound only
/// keeps a runaway turn from holding unbounded memory in the harness.
const RAW_FIELD_CAP: usize = 8 * 1024 * 1024;

/// One tool call, keyed by `toolCallId`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolCall {
    pub extension: Option<String>,
    /// `None` only if no update for this call ever named the tool.
    pub name: Option<String>,
    /// The last terminal status if one arrived, else the last status seen.
    pub status: Option<String>,
    terminal: bool,
}

/// One `(extension, name)` row of the record's `tools` aggregate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolAggregate {
    pub extension: Option<String>,
    pub name: Option<String>,
    pub calls: u64,
    pub errors: u64,
}

/// What one turn produced, drained by [`TurnRecorder::take`].
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct TurnObservation {
    pub session_id: String,
    /// The harness's own per-session counter, from 1. Exists even when Goose
    /// reports no usage, so it is not the usage tracker's `turn_seq`.
    pub turn_seq: u64,
    /// When `session/prompt` was about to be sent.
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    /// In order of first sight of each `toolCallId`.
    pub tool_calls: Vec<ToolCall>,
    /// What `load_skill` was asked for, verbatim and de-duplicated, in order.
    pub loaded: Vec<String>,
    /// The raw ACP `stopReason`, verbatim. `None` if no response arrived.
    pub stop_reason: Option<String>,
    /// `_meta.goose.inference.requestedModel` from the last message chunk.
    pub model: Option<String>,
    /// `_meta.goose.inference.provider` from the last message chunk.
    pub provider: Option<String>,
    pub channel_text: String,
    pub reasoning: String,
    pub tool_results: Vec<String>,
    /// True if any raw field hit [`RAW_FIELD_CAP`] in the harness.
    pub raw_capped: bool,
    /// Every `buzz messages send` the relay accepted, in completion order
    /// (D1h). The record's `delivered` is derived from this.
    pub sends: Vec<AcceptedSend>,
}

/// One accepted `buzz messages send`, read from the shell call's own command
/// and output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AcceptedSend {
    pub event_id: String,
    /// The `--channel` the command named, lowercased. `None` when it cannot be
    /// attributed (no `--channel`, or several sends in one call that do not
    /// pair up): such a send still counts as delivered, never as the reply.
    pub channel_id: Option<String>,
}

impl TurnObservation {
    /// The `tools` aggregate: one row per `(extension, name)`, in order of
    /// first sight. A call counts as an error when its status is `failed`.
    pub fn tools(&self) -> Vec<ToolAggregate> {
        let mut rows: Vec<ToolAggregate> = Vec::new();
        for call in &self.tool_calls {
            let errored = call.status.as_deref() == Some("failed");
            match rows
                .iter_mut()
                .find(|r| r.extension == call.extension && r.name == call.name)
            {
                Some(row) => {
                    row.calls += 1;
                    row.errors += u64::from(errored);
                }
                None => rows.push(ToolAggregate {
                    extension: call.extension.clone(),
                    name: call.name.clone(),
                    calls: 1,
                    errors: u64::from(errored),
                }),
            }
        }
        rows
    }
}

/// Accumulates one turn at a time. Same lifecycle as the usage trackers:
/// `begin_turn` before `session/prompt` is sent, `observe` for each update,
/// `take` when the turn ends.
#[derive(Debug, Default)]
pub(crate) struct TurnRecorder {
    current: Option<InFlight>,
    /// Last `turn_seq` handed out, per session id. A process restart starts
    /// again at 1; ENGRAM's key carries `trigger_event_id` for that reason.
    seqs: HashMap<String, u64>,
}

#[derive(Debug, Default)]
struct InFlight {
    obs: TurnObservation,
    index: HashMap<String, usize>,
    /// A shell call's `rawInput.command`, when it contains a send.
    send_commands: HashMap<usize, String>,
}

fn str_at<'a>(v: &'a serde_json::Value, pointer: &str) -> Option<&'a str> {
    v.pointer(pointer).and_then(serde_json::Value::as_str)
}

fn push_capped(buf: &mut String, text: &str, capped: &mut bool) {
    let room = RAW_FIELD_CAP.saturating_sub(buf.len());
    if text.len() <= room {
        buf.push_str(text);
    } else {
        // Cut on a char boundary at or below `room`.
        let mut cut = room;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        buf.push_str(&text[..cut]);
        *capped = true;
    }
}

impl TurnRecorder {
    /// Start a turn. Discards anything left from a turn never taken.
    pub fn begin_turn(&mut self, session_id: &str) {
        let seq = self.seqs.entry(session_id.to_string()).or_insert(0);
        *seq += 1;
        let turn_seq = *seq;
        self.current = Some(InFlight {
            obs: TurnObservation {
                session_id: session_id.to_string(),
                turn_seq,
                started_at: Some(chrono::Utc::now()),
                ..TurnObservation::default()
            },
            index: HashMap::new(),
            send_commands: HashMap::new(),
        });
    }

    /// Record the raw `stopReason` of the turn's `session/prompt` response.
    pub fn record_stop_reason(&mut self, raw: &str) {
        if let Some(cur) = self.current.as_mut() {
            cur.obs.stop_reason = Some(raw.to_string());
        }
    }

    /// Observe one `session/update` notification (the whole message). Updates
    /// for another session, or outside a turn, are ignored.
    pub fn observe(&mut self, msg: &serde_json::Value) {
        let Some(cur) = self.current.as_mut() else {
            return;
        };
        if let Some(sid) = str_at(msg, "/params/sessionId") {
            if sid != cur.obs.session_id {
                return;
            }
        }
        let update = &msg["params"]["update"];
        // The model is on message chunks AND on tool calls (design §3,
        // `usage` row), so a turn that only called tools still names it.
        let inference = update
            .pointer("/_meta/goose/inference")
            .or_else(|| update.pointer("/content/_meta/goose/inference"));
        if let Some(inference) = inference {
            if let Some(m) = str_at(inference, "/requestedModel") {
                cur.obs.model = Some(m.to_string());
            }
            if let Some(p) = str_at(inference, "/provider") {
                cur.obs.provider = Some(p.to_string());
            }
        }
        match str_at(update, "/sessionUpdate").unwrap_or("") {
            "agent_message_chunk" => {
                if let Some(text) = str_at(update, "/content/text") {
                    push_capped(&mut cur.obs.channel_text, text, &mut cur.obs.raw_capped);
                }
            }
            "agent_thought_chunk" => {
                if let Some(text) = str_at(update, "/content/text") {
                    push_capped(&mut cur.obs.reasoning, text, &mut cur.obs.raw_capped);
                }
            }
            "tool_call" | "tool_call_update" => {
                let Some(id) = str_at(update, "/toolCallId") else {
                    return;
                };
                let idx = match cur.index.get(id) {
                    Some(&i) => i,
                    None => {
                        cur.obs.tool_calls.push(ToolCall {
                            extension: None,
                            name: None,
                            status: None,
                            terminal: false,
                        });
                        let i = cur.obs.tool_calls.len() - 1;
                        cur.index.insert(id.to_string(), i);
                        i
                    }
                };
                let call = &mut cur.obs.tool_calls[idx];

                // Naming: ACP's `name` beats Goose's `toolName`; first value
                // seen sticks, so a later update cannot rename a call.
                let acp_name = str_at(update, "/name");
                let goose_name = str_at(update, "/_meta/goose/toolCall/toolName");
                if let Some(n) = acp_name {
                    call.name = Some(n.to_string());
                } else if call.name.is_none() {
                    call.name = goose_name.map(str::to_string);
                }
                if call.extension.is_none() {
                    call.extension =
                        str_at(update, "/_meta/goose/toolCall/extensionName").map(str::to_string);
                }

                let mut became_terminal = false;
                if let Some(status) = str_at(update, "/status") {
                    became_terminal = is_terminal(status) && !call.terminal;
                    if is_terminal(status) {
                        call.status = Some(status.to_string());
                        call.terminal = true;
                    } else if !call.terminal {
                        call.status = Some(status.to_string());
                    }
                    if is_terminal(status) {
                        if let Some(result) = tool_result_text(update) {
                            if cur.obs.tool_results.len() < 10_000 {
                                cur.obs.tool_results.push(result);
                            } else {
                                cur.obs.raw_capped = true;
                            }
                        }
                    }
                }

                // `loaded`: what `load_skill` was asked to load, verbatim.
                let is_load_skill =
                    call.extension.is_none() && call.name.as_deref() == Some("load_skill");
                if is_load_skill {
                    if let Some(what) = str_at(update, "/rawInput/name") {
                        if !cur.obs.loaded.iter().any(|l| l == what) {
                            cur.obs.loaded.push(what.to_string());
                        }
                    }
                }

                // D1h: our agents reply by running `buzz messages send` in a
                // shell, so the reply's event id is in that call's output.
                if let Some(command) = str_at(update, "/rawInput/command") {
                    if command.contains(SEND_VERB) {
                        cur.send_commands
                            .entry(idx)
                            .or_insert_with(|| command.to_string());
                    }
                }
                if became_terminal {
                    if let Some(command) = cur.send_commands.get(&idx) {
                        let output = tool_output_text(update).unwrap_or_default();
                        cur.obs.sends.extend(accepted_sends(command, &output));
                    }
                }
            }
            _ => {}
        }
    }

    /// End the turn and return what it produced. `None` outside a turn.
    pub fn take(&mut self) -> Option<TurnObservation> {
        self.current.take().map(|cur| cur.obs)
    }
}

/// The CLI verb a reply is sent with.
const SEND_VERB: &str = "buzz messages send";

/// What the shell printed: `rawOutput.stdout` (Goose 1.57.0's shell result is
/// `{stdout, stderr, exit_code}`), else the joined text content.
fn tool_output_text(update: &serde_json::Value) -> Option<String> {
    if let Some(stdout) = update.pointer("/rawOutput/stdout").and_then(|v| v.as_str()) {
        return Some(stdout.to_string());
    }
    if let Some(raw) = update.get("rawOutput").and_then(|v| v.as_str()) {
        return Some(raw.to_string());
    }
    let parts: Vec<&str> = update
        .get("content")?
        .as_array()?
        .iter()
        .filter_map(|c| {
            c.pointer("/content/text")
                .or_else(|| c.pointer("/text"))
                .and_then(serde_json::Value::as_str)
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

/// The `--channel` of each send in a command, in order. Accepts
/// `--channel X`, `--channel=X` and a quoted value; anything that is not a
/// UUID is `None`, never a guess.
pub(crate) fn send_channels(command: &str) -> Vec<Option<String>> {
    let starts: Vec<usize> = command.match_indices(SEND_VERB).map(|(i, _)| i).collect();
    starts
        .iter()
        .enumerate()
        .map(|(n, &start)| {
            let end = starts.get(n + 1).copied().unwrap_or(command.len());
            let segment = &command[start..end];
            let at = segment.find("--channel")? + "--channel".len();
            let rest = segment[at..].trim_start_matches([' ', '=', '\t']);
            let rest = rest.trim_start_matches(['"', '\'']);
            let candidate = rest.get(..36)?;
            uuid::Uuid::parse_str(candidate).ok().map(|u| u.to_string())
        })
        .collect()
}

/// The event id of each `{"accepted":true,"event_id":…}` line, in order. A
/// refused send (`accepted: false`) or a malformed id is not a send.
pub(crate) fn accepted_event_ids(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with('{'))
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v.get("accepted").and_then(|a| a.as_bool()) == Some(true))
        .filter_map(|v| {
            v.get("event_id")
                .and_then(|e| e.as_str())
                .map(str::to_string)
        })
        .filter(|id| id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(|id| id.to_ascii_lowercase())
        .collect()
}

/// Pair a call's sends with its accepted ids. One-to-one when the counts
/// match; one channel for all when every send named the same one; otherwise
/// unattributed, because pairing by position would be a guess.
fn accepted_sends(command: &str, output: &str) -> Vec<AcceptedSend> {
    let channels = send_channels(command);
    let ids = accepted_event_ids(output);
    let shared = channels
        .first()
        .cloned()
        .flatten()
        .filter(|c| channels.iter().all(|x| x.as_deref() == Some(c.as_str())));
    let paired = channels.len() == ids.len();
    ids.into_iter()
        .enumerate()
        .map(|(i, event_id)| AcceptedSend {
            event_id,
            channel_id: if paired {
                channels[i].clone()
            } else {
                shared.clone()
            },
        })
        .collect()
}

/// The text of a terminal tool update: `rawOutput` if present (it carries
/// `exit_code` for a shell call), else the joined text content.
fn tool_result_text(update: &serde_json::Value) -> Option<String> {
    if let Some(raw) = update.get("rawOutput").filter(|v| !v.is_null()) {
        return Some(raw.to_string());
    }
    let parts: Vec<&str> = update
        .get("content")?
        .as_array()?
        .iter()
        .filter_map(|c| {
            c.pointer("/content/text")
                .or_else(|| c.pointer("/text"))
                .and_then(serde_json::Value::as_str)
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── D1h: accepted sends ──────────────────────────────────────────────

    const OWN: &str = "7dfc1946-10d3-4497-8080-94db7575fc70";
    const OTHER: &str = "1d98c187-8686-4bd6-b88c-ffc4d410471e";

    fn hex(c: char) -> String {
        c.to_string().repeat(64)
    }

    /// A `shell` call as Goose 1.57.0 sends it: `tool_call` with `rawInput`,
    /// an `in_progress` update, then `completed` with
    /// `rawOutput {stdout, stderr, exit_code}` (v8 review §1.4).
    fn run_shell(r: &mut TurnRecorder, id: &str, command: &str, stdout: &str) {
        r.observe(&update(
            "s",
            json!({"sessionUpdate":"tool_call","toolCallId":id,
            "title":"Run","kind":"execute","status":"pending","rawInput":{"command":command},
            "_meta":{"goose":{"toolCall":{"toolName":"shell","extensionName":"developer"}}}}),
        ));
        r.observe(&update(
            "s",
            json!({"sessionUpdate":"tool_call_update","toolCallId":id,
            "status":"in_progress"}),
        ));
        r.observe(&update(
            "s",
            json!({"sessionUpdate":"tool_call_update","toolCallId":id,
            "status":"completed","rawOutput":{"stdout":stdout,"stderr":"","exit_code":0}}),
        ));
    }

    fn accepted(id: &str) -> String {
        format!(r#"{{"accepted":true,"event_id":"{id}","mention_pubkeys":[],"message":""}}"#)
    }

    #[test]
    fn a_turn_with_several_sends_records_each_with_its_channel_in_order() {
        let mut r = TurnRecorder::default();
        r.begin_turn("s");
        // The measured shape (DEV coder, event 6313597d…): a thread reply.
        run_shell(
            &mut r,
            "a",
            &format!(
                "buzz messages send --channel {OWN} --reply-to {} --content \"first\"",
                hex('c')
            ),
            &accepted(&hex('1')),
        );
        run_shell(
            &mut r,
            "b",
            &format!("buzz messages send --channel={OTHER} --content 'hand-off'"),
            &accepted(&hex('2')),
        );
        run_shell(
            &mut r,
            "c",
            &format!("cd /tmp && buzz messages send --channel \"{OWN}\" --content x"),
            &accepted(&hex('3')),
        );
        // Refused by the relay: not a send.
        run_shell(
            &mut r,
            "d",
            &format!("buzz messages send --channel {OWN} --content y"),
            r#"{"accepted":false,"event_id":null,"message":"rate limited"}"#,
        );
        // Not a send at all, though its output looks like one.
        run_shell(&mut r, "e", "cat /tmp/last-send.json", &accepted(&hex('4')));
        let obs = r.take().unwrap();
        let got: Vec<(String, Option<String>)> = obs
            .sends
            .iter()
            .map(|s| (s.event_id.clone(), s.channel_id.clone()))
            .collect();
        assert_eq!(
            got,
            vec![
                (hex('1'), Some(OWN.into())),
                (hex('2'), Some(OTHER.into())),
                (hex('3'), Some(OWN.into())),
            ]
        );
    }

    #[test]
    fn a_repeated_terminal_update_does_not_count_a_send_twice() {
        let mut r = TurnRecorder::default();
        r.begin_turn("s");
        let cmd = format!("buzz messages send --channel {OWN} --content x");
        run_shell(&mut r, "a", &cmd, &accepted(&hex('1')));
        r.observe(&update("s", json!({"sessionUpdate":"tool_call_update","toolCallId":"a",
            "status":"completed","rawOutput":{"stdout":accepted(&hex('1')),"stderr":"","exit_code":0}})));
        assert_eq!(r.take().unwrap().sends.len(), 1);
    }

    #[test]
    fn two_sends_in_one_call_pair_by_position_only_when_the_counts_match() {
        let two = format!(
            "buzz messages send --channel {OWN} --content a && buzz messages send --channel {OTHER} --content b");
        let out = format!("{}\n{}\n", accepted(&hex('1')), accepted(&hex('2')));
        let s = accepted_sends(&two, &out);
        assert_eq!(s[0].channel_id.as_deref(), Some(OWN));
        assert_eq!(s[1].channel_id.as_deref(), Some(OTHER));
        // One of the two was refused: positions no longer line up, and the
        // channels differ, so neither is attributed rather than guessed.
        let s = accepted_sends(&two, &accepted(&hex('2')));
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].channel_id, None);
        // Same count mismatch, but both named one channel: that one.
        let same = format!(
            "buzz messages send --channel {OWN} --content a; buzz messages send --channel {OWN} --content b");
        assert_eq!(
            accepted_sends(&same, &accepted(&hex('2')))[0]
                .channel_id
                .as_deref(),
            Some(OWN)
        );
    }

    #[test]
    fn send_channels_refuses_anything_that_is_not_a_uuid() {
        assert_eq!(send_channels("buzz messages send --content x"), vec![None]);
        assert_eq!(
            send_channels("buzz messages send --channel $CHANNEL --content x"),
            vec![None]
        );
        assert_eq!(
            send_channels(&format!(
                "buzz messages send --channel {}",
                OWN.to_uppercase()
            )),
            vec![Some(OWN.to_string())],
            "lowercased, so it compares with the turn's channel"
        );
        assert!(send_channels("echo hi").is_empty());
    }

    #[test]
    fn turn_seq_counts_per_session_and_survives_an_untaken_turn() {
        let mut r = TurnRecorder::default();
        r.begin_turn("a");
        assert_eq!(r.take().unwrap().turn_seq, 1);
        r.begin_turn("a");
        // Never taken: the next begin discards it, but the number is spent,
        // so a seq is never reused for two different turns.
        r.begin_turn("a");
        let third = r.take().unwrap();
        assert_eq!(third.turn_seq, 3);
        assert!(third.started_at.is_some());
        r.begin_turn("b");
        assert_eq!(
            r.take().unwrap().turn_seq,
            1,
            "another session counts on its own"
        );
    }

    fn update(session: &str, update: serde_json::Value) -> serde_json::Value {
        json!({"jsonrpc":"2.0","method":"session/update",
               "params":{"sessionId": session, "update": update}})
    }

    /// The shapes measured on DEV at Goose 1.57.0 (v8 review §1.2–§1.4).
    fn shell_call(id: &str) -> serde_json::Value {
        json!({"sessionUpdate":"tool_call","toolCallId":id,"title":"Run ls","kind":"execute",
               "status":"pending",
               "_meta":{"goose":{"toolCall":{"toolName":"shell","extensionName":"developer"}}}})
    }
    fn status(id: &str, s: &str) -> serde_json::Value {
        json!({"sessionUpdate":"tool_call_update","toolCallId":id,"status":s})
    }

    #[test]
    fn last_terminal_status_wins_not_the_first_update() {
        let mut r = TurnRecorder::default();
        r.begin_turn("s");
        r.observe(&update("s", shell_call("t1")));
        r.observe(&update("s", status("t1", "in_progress")));
        r.observe(&update("s", status("t1", "failed")));
        let obs = r.take().unwrap();
        assert_eq!(obs.tool_calls[0].status.as_deref(), Some("failed"));
        assert_eq!(
            obs.tools()[0].errors,
            1,
            "the in_progress update must not hide the error"
        );
    }

    #[test]
    fn a_non_terminal_update_after_a_terminal_one_does_not_overwrite_it() {
        let mut r = TurnRecorder::default();
        r.begin_turn("s");
        r.observe(&update("s", shell_call("t1")));
        r.observe(&update("s", status("t1", "completed")));
        r.observe(&update("s", status("t1", "in_progress")));
        assert_eq!(
            r.take().unwrap().tool_calls[0].status.as_deref(),
            Some("completed")
        );
    }

    #[test]
    fn a_tool_is_the_pair_and_a_builtin_has_no_extension() {
        let mut r = TurnRecorder::default();
        r.begin_turn("s");
        r.observe(&update(
            "s",
            json!({"sessionUpdate":"tool_call","toolCallId":"t0","title":"Load skill",
                   "rawInput":{"name":"channel-discipline"},
                   "_meta":{"goose":{"toolCall":{"toolName":"load_skill"}}}}),
        ));
        r.observe(&update("s", status("t0", "completed")));
        r.observe(&update("s", shell_call("t1")));
        r.observe(&update("s", status("t1", "completed")));
        let obs = r.take().unwrap();
        let tools = obs.tools();
        assert_eq!(tools[0].extension, None);
        assert_eq!(tools[0].name.as_deref(), Some("load_skill"));
        assert_eq!(tools[1].extension.as_deref(), Some("developer"));
        assert_eq!(
            tools[1].name.as_deref(),
            Some("shell"),
            "bare, never developer__shell"
        );
        assert_eq!(obs.loaded, vec!["channel-discipline".to_string()]);
    }

    #[test]
    fn order_is_first_sight_whichever_variant_arrives_first() {
        // ACP v2: no `tool_call`, the update is an upsert.
        let mut r = TurnRecorder::default();
        r.begin_turn("s");
        r.observe(&update("s", status("b", "in_progress")));
        r.observe(&update("s", shell_call("a")));
        r.observe(&update("s", status("b", "completed")));
        r.observe(&update("s", status("a", "completed")));
        let obs = r.take().unwrap();
        assert_eq!(obs.tool_calls.len(), 2);
        assert_eq!(obs.tool_calls[0].status.as_deref(), Some("completed"));
        assert_eq!(obs.tool_calls[0].name, None, "call b was never named");
        assert_eq!(obs.tool_calls[1].name.as_deref(), Some("shell"));
    }

    #[test]
    fn acp_name_beats_goose_tool_name_and_title_is_never_a_key() {
        let mut r = TurnRecorder::default();
        r.begin_turn("s");
        r.observe(&update(
            "s",
            json!({"sessionUpdate":"tool_call","toolCallId":"t","title":"Some title",
                   "name":"acp_name",
                   "_meta":{"goose":{"toolCall":{"toolName":"goose_name"}}}}),
        ));
        assert_eq!(
            r.take().unwrap().tool_calls[0].name.as_deref(),
            Some("acp_name")
        );
    }

    #[test]
    fn repeated_loads_are_listed_once_in_first_order() {
        let mut r = TurnRecorder::default();
        r.begin_turn("s");
        for (id, what) in [
            ("1", "buzz-coder"),
            ("2", "buzz-coder/references/a.md"),
            ("3", "buzz-coder"),
        ] {
            r.observe(&update(
                "s",
                json!({"sessionUpdate":"tool_call","toolCallId":id,"rawInput":{"name":what},
                       "_meta":{"goose":{"toolCall":{"toolName":"load_skill"}}}}),
            ));
        }
        assert_eq!(
            r.take().unwrap().loaded,
            vec![
                "buzz-coder".to_string(),
                "buzz-coder/references/a.md".to_string()
            ]
        );
    }

    #[test]
    fn an_extension_tool_named_load_skill_is_not_a_skill_load() {
        let mut r = TurnRecorder::default();
        r.begin_turn("s");
        r.observe(&update(
            "s",
            json!({"sessionUpdate":"tool_call","toolCallId":"t","rawInput":{"name":"x"},
                   "_meta":{"goose":{"toolCall":{"toolName":"load_skill","extensionName":"other"}}}}),
        ));
        assert!(r.take().unwrap().loaded.is_empty());
    }

    #[test]
    fn text_model_provider_and_stop_reason_are_kept() {
        let mut r = TurnRecorder::default();
        r.begin_turn("s");
        for t in ["Hel", "lo"] {
            r.observe(&update(
                "s",
                json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":t},
                       "_meta":{"goose":{"inference":{"provider":"anthropic",
                                                      "requestedModel":"claude-sonnet-5"}}}}),
            ));
        }
        r.observe(&update(
            "s",
            json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"hmm"}}),
        ));
        r.record_stop_reason("END_TURN");
        let obs = r.take().unwrap();
        assert_eq!(obs.channel_text, "Hello");
        assert_eq!(obs.reasoning, "hmm");
        assert_eq!(obs.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(obs.provider.as_deref(), Some("anthropic"));
        assert_eq!(
            obs.stop_reason.as_deref(),
            Some("END_TURN"),
            "verbatim, not case-folded"
        );
    }

    #[test]
    fn the_model_is_read_from_a_tool_call_too() {
        let mut r = TurnRecorder::default();
        r.begin_turn("s");
        r.observe(&update(
            "s",
            json!({"sessionUpdate":"tool_call","toolCallId":"t",
                   "_meta":{"goose":{"toolCall":{"toolName":"shell","extensionName":"developer"},
                                     "inference":{"provider":"google",
                                                  "requestedModel":"gemini-3.8-flash"}}}}),
        ));
        let obs = r.take().unwrap();
        assert_eq!(obs.model.as_deref(), Some("gemini-3.8-flash"));
        assert_eq!(obs.provider.as_deref(), Some("google"));
    }

    #[test]
    fn tool_results_prefer_raw_output() {
        let mut r = TurnRecorder::default();
        r.begin_turn("s");
        r.observe(&update("s", shell_call("t")));
        r.observe(&update(
            "s",
            json!({"sessionUpdate":"tool_call_update","toolCallId":"t","status":"completed",
                   "content":[{"type":"content","content":{"type":"text","text":"out"}}],
                   "rawOutput":{"stdout":"out","stderr":"","exit_code":0}}),
        ));
        let obs = r.take().unwrap();
        assert_eq!(obs.tool_results.len(), 1);
        assert!(obs.tool_results[0].contains("\"exit_code\":0"));
    }

    #[test]
    fn other_sessions_and_updates_outside_a_turn_are_ignored() {
        let mut r = TurnRecorder::default();
        r.observe(&update("s", shell_call("before")));
        r.begin_turn("s");
        r.observe(&update("other", shell_call("x")));
        r.observe(&update("s", shell_call("t")));
        let obs = r.take().unwrap();
        assert_eq!(obs.tool_calls.len(), 1);
        assert!(r.take().is_none(), "take drains");
    }

    #[test]
    fn raw_text_is_capped_on_a_char_boundary() {
        let mut buf = "a".repeat(RAW_FIELD_CAP - 1);
        let mut capped = false;
        push_capped(&mut buf, "é", &mut capped); // two bytes, one of room
        assert!(capped);
        assert_eq!(buf.len(), RAW_FIELD_CAP - 1);
    }
}
