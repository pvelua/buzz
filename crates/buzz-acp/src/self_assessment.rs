//! The model's own account of a turn, for ENGRAM's turn record (v1.4.1 D3).
//!
//! The API note (KB `03f40ec8`) adds one optional field to the record:
//!
//! ```text
//! "self_assessment": {"quality": 1-5 or null, "obstacles": [string], "suggestions": [string]}
//! ```
//!
//! It is sent only on a turn that **delivered**, and collected **at the end of
//! that turn** while the session's context is still fresh (plan §3.1b, Igor
//! 2026-11-12). This module owns the three pieces that are ours: the
//! instruction that asks for it, the parser that reads it, and the strip that
//! keeps it out of the raw blob.
//!
//! Three facts decide the shape, each measured or contracted:
//!
//! - **Where the answer goes.** On our agents the reply is posted by the model's
//!   own `buzz messages send` shell call, and the text the model writes after
//!   its last tool call reaches only the harness (measured on DEV 2026-11-19,
//!   turn `20261119_1#2`: final text *"Replied again naming my installed skill
//!   as `buzz-coder`."*, never posted). So the assessment is asked for at the
//!   end of that final message, and it costs no second model call.
//! - **Where it can still leak.** The harness cannot strip a shell command the
//!   model has already run, so if the model puts the block into a send, it is
//!   in the channel. The instruction says so; the recorder flags it
//!   ([`mentions_tag`]) and the turn logs one WARN. The DEV acceptance reads
//!   the posted message itself.
//! - **Never guess.** Anything that is not exactly the agreed shape is
//!   **omitted** ([`extract`] returns `None`). A guessed `quality` would be
//!   indistinguishable from a reported one, and the judge weighs this field
//!   against the trace.

/// The opening tag. Matched exactly when parsing, case-insensitively when
/// looking for a leak.
const OPEN: &str = "<self_assessment>";
const CLOSE: &str = "</self_assessment>";

/// At most this many entries in each list, each at most this many chars.
/// Over either, the whole assessment is omitted rather than cut.
const MAX_ITEMS: usize = 20;
const MAX_ITEM_CHARS: usize = 2000;

/// The instruction appended to the session prompt when observation is on.
///
/// ⚠️ The wording is load-bearing in the same way as the skill bootstrap
/// (`engram_skill::bootstrap_text`): it is pinned by a golden test, and
/// changing it means re-measuring on both providers that the block arrives and
/// that the posted reply does not carry it.
pub(crate) const INSTRUCTION: &str = "\
At the very end of each turn in which you sent a reply, assess that turn for the harness.

Write it as the last thing in your final message, the text you write after your last tool call. \
That text is read by the harness and is not posted to the channel. Use exactly this form, on its own lines:

<self_assessment>{\"quality\": <1-5 or null>, \"obstacles\": [\"...\"], \"suggestions\": [\"...\"]}</self_assessment>

- `quality`: an integer from 1 (poor) to 5 (excellent) for how well the reply you delivered met the \
request, or null if you cannot judge.
- `obstacles`: short strings naming what got in your way this turn, such as missing information, a \
failed tool or an unclear instruction. [] if nothing did.
- `suggestions`: short strings naming what would have made this turn go better, such as a change to \
your skill or your instructions. [] if none.

Never put the self-assessment in a message you send with `buzz messages send` or any other tool: it \
is for the harness only, and the channel must not see it. If you sent no reply this turn, leave it out.";

/// A parsed, validated self-assessment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelfAssessment {
    pub quality: Option<u8>,
    pub obstacles: Vec<String>,
    pub suggestions: Vec<String>,
}

impl SelfAssessment {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "quality": self.quality,
            "obstacles": self.obstacles,
            "suggestions": self.suggestions,
        })
    }
}

/// The LAST complete block in `text`, if it is exactly the agreed shape.
///
/// Strict on purpose: the three keys are all required and no others are
/// allowed; `quality` is an integer 1–5 or null (a `4.0` or `"4"` is
/// rejected, not coerced); each list holds only strings, within the caps.
pub(crate) fn extract(text: &str) -> Option<SelfAssessment> {
    let start = text.rfind(OPEN)?;
    let body_start = start + OPEN.len();
    let body_len = text[body_start..].find(CLOSE)?;
    let body = text[body_start..body_start + body_len].trim();
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let obj = value.as_object()?;
    if obj.len() != 3 {
        return None;
    }
    let quality = match obj.get("quality")? {
        serde_json::Value::Null => None,
        q => {
            let n = q.as_u64()?;
            if !(1..=5).contains(&n) {
                return None;
            }
            Some(n as u8)
        }
    };
    Some(SelfAssessment {
        quality,
        obstacles: string_list(obj.get("obstacles")?)?,
        suggestions: string_list(obj.get("suggestions")?)?,
    })
}

fn string_list(value: &serde_json::Value) -> Option<Vec<String>> {
    let items = value.as_array()?;
    if items.len() > MAX_ITEMS {
        return None;
    }
    items
        .iter()
        .map(|item| {
            let s = item.as_str()?;
            (s.chars().count() <= MAX_ITEM_CHARS).then(|| s.to_string())
        })
        .collect()
}

/// `text` with every complete block removed, so the raw blob does not hold
/// the model's account twice. An unclosed tag is left as it is.
pub(crate) fn strip(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        let after = &rest[start + OPEN.len()..];
        let Some(end) = after.find(CLOSE) else { break };
        out.push_str(rest[..start].trim_end());
        rest = &after[end + CLOSE.len()..];
    }
    out.push_str(rest);
    out.trim_end().to_string()
}

/// Whether `text` carries the tag at all, in any case. Used on the commands of
/// accepted sends: a hit means the assessment was posted to a channel.
pub(crate) fn mentions_tag(text: &str) -> bool {
    text.to_ascii_lowercase().contains("<self_assessment")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(json: &str) -> String {
        format!("Replied with the summary.\n\n<self_assessment>{json}</self_assessment>")
    }

    #[test]
    fn the_agreed_shape_parses() {
        let a = extract(&block(
            r#"{"quality": 4, "obstacles": ["no test runner"], "suggestions": ["add pytest"]}"#,
        ))
        .unwrap();
        assert_eq!(a.quality, Some(4));
        assert_eq!(a.obstacles, vec!["no test runner"]);
        assert_eq!(a.suggestions, vec!["add pytest"]);
        assert_eq!(
            a.to_json(),
            serde_json::json!({"quality": 4, "obstacles": ["no test runner"], "suggestions": ["add pytest"]})
        );
    }

    #[test]
    fn null_quality_is_a_real_answer_and_is_kept_as_null() {
        let a = extract(&block(
            r#"{"quality": null, "obstacles": [], "suggestions": []}"#,
        ))
        .unwrap();
        assert_eq!(a.quality, None);
        assert!(a.to_json()["quality"].is_null());
    }

    #[test]
    fn whitespace_and_newlines_inside_the_tags_are_fine() {
        let t = "done\n<self_assessment>\n  {\"quality\": 5,\n   \"obstacles\": [], \"suggestions\": []}\n</self_assessment>\n";
        assert_eq!(extract(t).unwrap().quality, Some(5));
    }

    #[test]
    fn the_last_block_wins() {
        let t = format!(
            "{}\n{}",
            block(r#"{"quality": 2, "obstacles": [], "suggestions": []}"#),
            block(r#"{"quality": 5, "obstacles": [], "suggestions": []}"#)
        );
        assert_eq!(extract(&t).unwrap().quality, Some(5));
    }

    /// Every one of these is OMITTED. Not one is coerced into a value.
    #[test]
    fn anything_off_shape_is_omitted_never_guessed() {
        let bad = [
            // No block, or an unclosed one.
            "Replied.".to_string(),
            "Replied. <self_assessment>{\"quality\": 4, \"obstacles\": [], \"suggestions\": []}"
                .into(),
            // Not JSON, or not an object.
            block("quality 4"),
            block("[4, [], []]"),
            // Out of range, or the wrong type for quality.
            block(r#"{"quality": 0, "obstacles": [], "suggestions": []}"#),
            block(r#"{"quality": 6, "obstacles": [], "suggestions": []}"#),
            block(r#"{"quality": -1, "obstacles": [], "suggestions": []}"#),
            block(r#"{"quality": 4.0, "obstacles": [], "suggestions": []}"#),
            block(r#"{"quality": 4.5, "obstacles": [], "suggestions": []}"#),
            block(r#"{"quality": "4", "obstacles": [], "suggestions": []}"#),
            // A key missing, or an extra one.
            block(r#"{"quality": 4, "obstacles": []}"#),
            block(r#"{"obstacles": [], "suggestions": []}"#),
            block(r#"{"quality": 4, "obstacles": [], "suggestions": [], "notes": "x"}"#),
            // A list that is not a list of strings.
            block(r#"{"quality": 4, "obstacles": "none", "suggestions": []}"#),
            block(r#"{"quality": 4, "obstacles": [1], "suggestions": []}"#),
            block(r#"{"quality": 4, "obstacles": [], "suggestions": null}"#),
            // A fenced body: the instruction asks for bare JSON.
            block("```json\n{\"quality\": 4, \"obstacles\": [], \"suggestions\": []}\n```"),
        ];
        for t in &bad {
            assert_eq!(extract(t), None, "must be omitted: {t}");
        }
    }

    #[test]
    fn over_the_caps_is_omitted_whole_not_cut() {
        let many: Vec<String> = (0..=MAX_ITEMS).map(|i| format!("\"o{i}\"")).collect();
        let t = block(&format!(
            r#"{{"quality": 3, "obstacles": [{}], "suggestions": []}}"#,
            many.join(",")
        ));
        assert_eq!(extract(&t), None);

        let long = "x".repeat(MAX_ITEM_CHARS + 1);
        let t = block(&format!(
            r#"{{"quality": 3, "obstacles": [], "suggestions": ["{long}"]}}"#
        ));
        assert_eq!(extract(&t), None);

        // Exactly at the caps is fine: the positive control for both.
        let at: Vec<String> = (0..MAX_ITEMS).map(|i| format!("\"o{i}\"")).collect();
        let ok = "y".repeat(MAX_ITEM_CHARS);
        let t = block(&format!(
            r#"{{"quality": 3, "obstacles": [{}], "suggestions": ["{ok}"]}}"#,
            at.join(",")
        ));
        assert!(extract(&t).is_some());
    }

    #[test]
    fn strip_removes_every_complete_block_and_keeps_the_rest() {
        let t = format!(
            "First.\n<self_assessment>{{}}</self_assessment>\nMiddle.\n\n{}\n",
            "<self_assessment>{\"quality\": 4, \"obstacles\": [], \"suggestions\": []}</self_assessment>"
        );
        assert_eq!(strip(&t), "First.\nMiddle.");
        assert_eq!(strip("No block here."), "No block here.");
        assert_eq!(strip(""), "");
        // An unclosed tag is not a block, so nothing is removed.
        assert_eq!(strip("a <self_assessment>{"), "a <self_assessment>{");
    }

    #[test]
    fn a_leak_is_seen_in_any_case_and_only_with_the_tag() {
        assert!(mentions_tag(
            r#"buzz messages send --channel c --content "ok <self_assessment>{}</self_assessment>""#
        ));
        assert!(mentions_tag("<SELF_ASSESSMENT>"));
        assert!(!mentions_tag(
            r#"buzz messages send --channel c --content "a self assessment""#
        ));
    }

    /// Golden: the instruction the D3 DEV acceptance measures. If this fails, re-measure on DEV
    /// (both providers) before updating it.
    #[test]
    fn the_instruction_is_the_measured_text() {
        assert_eq!(
            INSTRUCTION,
            include_str!("../tests/fixtures/self_assessment_instruction.txt").trim_end()
        );
        // Its example is a placeholder, not a value, so that it cannot anchor
        // the answer -- and a model that copies it verbatim is OMITTED rather
        // than recorded as a score it never gave.
        assert_eq!(extract(INSTRUCTION), None);
    }
}
