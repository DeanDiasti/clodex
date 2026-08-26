//! Hierarchical compaction planning.
//!
//! Claude Code answers a rejected prompt by compacting, but the compaction
//! request carries the same oversized conversation, so it is rejected too and
//! the session cannot compact its way back under the limit. This module plans
//! a fold that replaces one oversized request with a series of rounds that
//! each fit by construction:
//!
//! ```text
//! S_0 = compact(chunk_0)
//! S_i = compact(S_{i-1} ++ chunk_i)   for i = 1..n
//! ```
//!
//! Only the planning lives here. The rounds themselves are issued by the
//! bridge, which owns the upstream connection.

use serde_json::Value;

/// The opening of the summary prompt Claude Code sends when it compacts. Used
/// to confirm a request the `PreCompact` hook already armed, so a fold never
/// runs against an ordinary request.
pub const COMPACTION_MARKER: &str =
    "Your task is to create a detailed summary of the conversation so far";

/// Headroom held back from the budget for framing the upstream request: role
/// wrappers, the summary prompt, and count drift between our estimate and the
/// upstream tokenizer.
const FRAMING_RESERVE: u64 = 8_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Round {
    /// Indices into the original message list, in order.
    pub messages: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub rounds: Vec<Round>,
}

impl Plan {
    pub fn round_count(&self) -> usize {
        self.rounds.len()
    }
}

/// What a single fold round must fit inside.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    /// The largest prompt the routed model accepts.
    pub ceiling: u64,
    /// Tokens consumed by the system prompt and tool definitions, which every
    /// round repeats.
    pub fixed_overhead: u64,
    /// The upper bound on a round's own output, which becomes the carry-in for
    /// the next round.
    pub max_output: u64,
}

impl Budget {
    /// Tokens available for prompt content in a single round, after the fixed
    /// overhead, the round's own output allowance, and framing.
    ///
    /// `max_output` is subtracted here to leave the model room to generate;
    /// a later round subtracts it a second time, separately, for the summary
    /// it carries in as input. The two reservations are not the same tokens.
    pub fn per_round(&self) -> Option<u64> {
        self.ceiling
            .checked_sub(self.fixed_overhead)?
            .checked_sub(self.max_output)?
            .checked_sub(FRAMING_RESERVE)
    }
}

/// Plans the fold.
///
/// `counts[i]` is the token count of message `i`. `can_open[i]` reports
/// whether a round may begin at message `i`; a round that began on a
/// `tool_result` would orphan it from its `tool_use` and be rejected outright.
///
/// Returns `None` when no safe plan exists, which the caller treats as "leave
/// the request alone" rather than as an error. That happens when the budget is
/// non-positive, when a single message exceeds a whole round, or when there is
/// no safe boundary to cut on.
pub fn plan(counts: &[u64], can_open: &[bool], budget: Budget) -> Option<Plan> {
    if counts.is_empty() || counts.len() != can_open.len() {
        return None;
    }
    let per_round = budget.per_round()?;
    if per_round == 0 {
        return None;
    }

    // The first round carries nothing; every later round carries the previous
    // summary, which is bounded by max_output.
    let first_capacity = per_round;
    let later_capacity = per_round.checked_sub(budget.max_output)?;
    if later_capacity == 0 {
        return None;
    }

    // A message larger than a whole round can never be placed.
    if counts.iter().any(|count| *count > later_capacity) {
        return None;
    }

    let mut rounds: Vec<Round> = Vec::new();
    let mut start = 0usize;

    while start < counts.len() {
        let capacity = if rounds.is_empty() {
            first_capacity
        } else {
            later_capacity
        };

        // Extend the round as far as the capacity allows, remembering the last
        // place a following round could legally open.
        let mut used = 0u64;
        let mut end = start;
        let mut last_safe: Option<usize> = None;
        while end < counts.len() {
            if used + counts[end] > capacity {
                break;
            }
            used += counts[end];
            end += 1;
            if end < counts.len() && can_open[end] {
                last_safe = Some(end);
            }
        }

        if end == counts.len() {
            rounds.push(Round {
                messages: (start..end).collect(),
            });
            break;
        }

        // The round is full. Cut where the next round can open: here if this
        // message is a safe boundary, otherwise back at the last one seen.
        // Cutting anywhere else would orphan a tool result from its tool use.
        let cut = if can_open[end] { end } else { last_safe? };
        if cut <= start {
            return None;
        }
        rounds.push(Round {
            messages: (start..cut).collect(),
        });
        start = cut;
    }

    if rounds.is_empty() {
        None
    } else {
        Some(Plan { rounds })
    }
}

/// Whether a round may begin at this message.
///
/// Splitting between a `tool_use` and its `tool_result` leaves the result
/// orphaned, which the API rejects, so only a user message that does not open
/// with a tool result is a safe boundary.
pub fn is_safe_boundary(message: &Value) -> bool {
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    match message.get("content") {
        Some(Value::String(_)) | None => true,
        Some(Value::Array(blocks)) => !blocks
            .iter()
            .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result")),
        Some(_) => false,
    }
}

/// Whether this request is the compaction Claude Code just announced.
///
/// The `PreCompact` hook arms a session; this confirms the request that
/// follows actually carries the summary prompt, so an ordinary request that
/// happens to arrive first is never folded.
pub fn carries_summary_prompt(messages: &[Value]) -> bool {
    messages
        .iter()
        .rev()
        .take(2)
        .any(|message| message_text(message).is_some_and(|text| text.contains(COMPACTION_MARKER)))
}

/// Flattens a message's text content for marker matching.
pub fn message_text(message: &Value) -> Option<String> {
    match message.get("content")? {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => {
            let mut text = String::new();
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("text")
                    && let Some(part) = block.get("text").and_then(Value::as_str)
                {
                    text.push_str(part);
                    text.push('\n');
                }
            }
            Some(text)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn budget(ceiling: u64) -> Budget {
        Budget {
            ceiling,
            fixed_overhead: 10_000,
            max_output: 16_000,
        }
    }

    #[test]
    fn a_conversation_that_already_fits_plans_a_single_round() {
        let counts = vec![1_000, 1_000, 1_000];
        let can_open = vec![true, true, true];

        let plan = plan(&counts, &can_open, budget(272_000)).unwrap();

        assert_eq!(plan.round_count(), 1);
        assert_eq!(plan.rounds[0].messages, vec![0, 1, 2]);
    }

    #[test]
    fn an_oversized_conversation_folds_into_rounds_that_each_fit() {
        // Each round can hold 100_000 - 16_000 = 84_000 tokens of content.
        let budget = Budget {
            ceiling: 124_000,
            fixed_overhead: 0,
            max_output: 16_000,
        };
        let counts = vec![40_000; 8];
        let can_open = vec![true; 8];

        let plan = plan(&counts, &can_open, budget).unwrap();

        assert!(plan.round_count() > 1, "expected a fold, got one round");
        let per_round = budget.per_round().unwrap();
        for (index, round) in plan.rounds.iter().enumerate() {
            let carried = if index == 0 { 0 } else { budget.max_output };
            let total: u64 = round.messages.iter().map(|i| counts[*i]).sum::<u64>() + carried;
            assert!(
                total <= per_round,
                "round {index} totals {total}, over the {per_round} budget"
            );
        }
    }

    #[test]
    fn every_message_appears_exactly_once_and_in_order() {
        let counts = vec![20_000; 12];
        let can_open = vec![true; 12];

        let plan = plan(&counts, &can_open, budget(80_000)).unwrap();

        let flattened: Vec<usize> = plan
            .rounds
            .iter()
            .flat_map(|round| round.messages.clone())
            .collect();
        assert_eq!(flattened, (0..12).collect::<Vec<_>>());
    }

    #[test]
    fn rounds_never_open_on_an_orphaned_tool_result() {
        let counts = vec![30_000; 6];
        // Assistant/tool-result pairs: only even indices can open a round.
        let can_open = vec![true, false, true, false, true, false];

        let plan = plan(&counts, &can_open, budget(120_000)).unwrap();

        assert!(plan.round_count() > 1, "expected a fold");

        for round in plan.rounds.iter().skip(1) {
            let first = round.messages[0];
            assert!(can_open[first], "round opened on unsafe boundary {first}");
        }
    }

    #[test]
    fn no_plan_when_a_single_message_exceeds_a_whole_round() {
        let counts = vec![1_000, 500_000, 1_000];
        let can_open = vec![true, true, true];

        assert_eq!(plan(&counts, &can_open, budget(272_000)), None);
    }

    #[test]
    fn no_plan_when_the_cut_would_orphan_a_tool_result() {
        let budget = Budget {
            ceiling: 60_000,
            fixed_overhead: 0,
            max_output: 16_000,
        };
        // Needs a cut, but nothing after index 0 can open a round.
        let counts = vec![30_000; 4];
        let can_open = vec![true, false, false, false];

        assert_eq!(plan(&counts, &can_open, budget), None);
    }

    #[test]
    fn no_plan_when_the_next_legal_cut_is_past_the_capacity() {
        // Safe boundaries every third message, but a round only holds two of
        // them. There is no legal cut, so the request is left alone.
        let counts = vec![30_000; 6];
        let can_open = vec![true, false, false, true, false, false];

        assert_eq!(plan(&counts, &can_open, budget(120_000)), None);
    }

    #[test]
    fn no_plan_when_overhead_consumes_the_window() {
        let budget = Budget {
            ceiling: 10_000,
            fixed_overhead: 9_000,
            max_output: 16_000,
        };
        assert_eq!(plan(&[100], &[true], budget), None);
    }

    #[test]
    fn the_fold_scales_to_very_large_conversations() {
        let counts = vec![10_000; 400];
        let can_open = vec![true; 400];

        let plan = plan(&counts, &can_open, budget(272_000)).unwrap();

        // 4M tokens of conversation against a 272k ceiling has to fold deeply.
        assert!(plan.round_count() >= 16, "{}", plan.round_count());
        let flattened: usize = plan.rounds.iter().map(|r| r.messages.len()).sum();
        assert_eq!(flattened, 400);
    }

    #[test]
    fn the_last_marker_is_the_split_point_not_the_first() {
        // A prior compaction summary carried in history also contains the
        // marker. Splitting on the first one would treat live conversation as
        // instruction tail and drop it from the fold.
        let quoted = serde_json::json!({
            "role": "user",
            "content": format!("Earlier summary: {COMPACTION_MARKER} ...")
        });
        let work = serde_json::json!({"role": "user", "content": "then we did more"});
        let request = serde_json::json!({
            "role": "user",
            "content": format!("{COMPACTION_MARKER}, paying close attention.")
        });
        let messages = vec![quoted, work, request];

        let first = messages
            .iter()
            .position(|m| message_text(m).is_some_and(|t| t.contains(COMPACTION_MARKER)));
        let last = messages
            .iter()
            .rposition(|m| message_text(m).is_some_and(|t| t.contains(COMPACTION_MARKER)));

        assert_eq!(first, Some(0));
        assert_eq!(last, Some(2));
        assert!(carries_summary_prompt(&messages));
    }

    #[test]
    fn tool_results_are_not_safe_boundaries() {
        let plain = serde_json::json!({"role": "user", "content": "hello"});
        let blocks = serde_json::json!({
            "role": "user",
            "content": [{"type": "text", "text": "hi"}]
        });
        let tool = serde_json::json!({
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "a", "content": "done"}]
        });
        let assistant = serde_json::json!({"role": "assistant", "content": "sure"});

        assert!(is_safe_boundary(&plain));
        assert!(is_safe_boundary(&blocks));
        assert!(!is_safe_boundary(&tool));
        assert!(!is_safe_boundary(&assistant));
    }

    #[test]
    fn the_summary_prompt_is_recognized_only_at_the_end() {
        let summary = serde_json::json!({
            "role": "user",
            "content": format!("{COMPACTION_MARKER}, paying close attention.")
        });
        let ordinary = serde_json::json!({"role": "user", "content": "keep going"});

        assert!(carries_summary_prompt(&[ordinary.clone(), summary.clone()]));
        assert!(!carries_summary_prompt(std::slice::from_ref(&ordinary)));
        // An old compaction prompt buried in history must not arm a fold.
        assert!(!carries_summary_prompt(&[
            summary,
            ordinary.clone(),
            ordinary.clone(),
            ordinary
        ]));
    }
}
