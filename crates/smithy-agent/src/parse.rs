//! Turn a raw completion into an action for the loop.
//!
//! Order of trust: structured `tool_calls` is the hot path. coda stress-tested
//! it to 20 tools without a failure — with the caveat, which its own
//! post-mortem raised, that the endpoint parses the model's output into
//! `tool_calls` *before the client ever sees it*. So the measurement shows the
//! client reliably receives structured calls, not that the model reliably emits
//! them. Those diverge the moment the serving stack changes, which is exactly
//! why the XML fallback below is kept rather than deleted as dead code.

use serde_json::Value;

use smithy_tools::ToolCall;

use crate::provider::Completion;

#[derive(Debug)]
pub enum Action {
    /// One or more valid tool calls to execute.
    Calls(Vec<ToolCall>),
    /// No tool call — the model produced a final answer. Terminate the turn.
    Done(String),
    /// Something looked like a tool call but couldn't be parsed. The string is a
    /// precise error to feed back so the model can retry.
    Malformed(String),
}

pub fn parse(c: &Completion) -> Action {
    // 1. Structured path.
    if !c.tool_calls.is_empty() {
        let mut out = Vec::with_capacity(c.tool_calls.len());
        for tc in &c.tool_calls {
            if tc.name.trim().is_empty() {
                return Action::Malformed("a tool_call had an empty function name".into());
            }
            if let Err(e) = tc.parsed_arguments() {
                return Action::Malformed(e);
            }
            out.push(tc.clone());
        }
        return Action::Calls(out);
    }

    // 2. Fallback: scrape the content field.
    let content = repair_unclosed_think(&c.content);
    if content.contains("<tool_call>") || content.contains("<function=") {
        match scrape_xml(&content) {
            Ok(calls) if !calls.is_empty() => return Action::Calls(calls),
            Ok(_) => {
                return Action::Malformed(
                    "content contained a tool-call marker but no call could be extracted".into(),
                )
            }
            Err(e) => return Action::Malformed(e),
        }
    }

    // 3. No tool call anywhere → the model is answering.
    Action::Done(c.content.clone())
}

/// If a `<think>` block was left unclosed and a tool call appears after it,
/// inject the missing `</think>` so the scraper sees clean content. Cheap guard;
/// Phase 0 never saw this fire on LM Studio, but it costs nothing.
pub fn repair_unclosed_think(content: &str) -> String {
    let opens = content.matches("<think>").count();
    let closes = content.matches("</think>").count();
    if opens > closes {
        // Close it right before the first tool-call marker, else at the end.
        let marker = content
            .find("<tool_call>")
            .or_else(|| content.find("<function="));
        if let Some(idx) = marker {
            let mut s = String::with_capacity(content.len() + 10);
            s.push_str(&content[..idx]);
            s.push_str("</think>\n");
            s.push_str(&content[idx..]);
            return s;
        }
        return format!("{content}</think>");
    }
    content.to_string()
}

/// Extract tool calls from raw content. Handles two shapes:
///   (a) Hermes JSON:  <tool_call>{"name":"x","arguments":{...}}</tool_call>
///   (b) Qwen XML:     <function=x><parameter=k>v</parameter></function>
///       (optionally wrapped in <tool_call>…</tool_call>)
///
/// **Ids are assigned once, at the end.** They used to be minted inside each
/// scraper from that scraper's own local count, so two `<tool_call>` blocks each
/// holding a `<function=…>` both produced `xml_0` — and a duplicate id is
/// precisely the failure `Message::tool_result` carries an id to prevent. Two
/// results correlated to one call is worse than no correlation at all, because
/// it looks like it worked.
fn scrape_xml(content: &str) -> Result<Vec<ToolCall>, String> {
    let mut calls = Vec::new();

    // (a) Hermes JSON blocks inside <tool_call>…</tool_call>.
    for inner in blocks_between(content, "<tool_call>", "</tool_call>") {
        let trimmed = inner.trim();
        if trimmed.starts_with('{') {
            let v: Value = serde_json::from_str(trimmed)
                .map_err(|e| format!("<tool_call> JSON was malformed: {e}"))?;
            let name = v
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                return Err("<tool_call> JSON had no `name`".into());
            }
            let arguments = match v.get("arguments") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => "{}".to_string(),
            };
            calls.push(ToolCall::new(String::new(), name, arguments));
        } else if trimmed.contains("<function=") {
            // Qwen XML nested inside <tool_call>.
            calls.extend(scrape_function_tags(trimmed)?);
        }
    }

    // (b) Bare <function=…> tags not wrapped in <tool_call>.
    if calls.is_empty() {
        calls.extend(scrape_function_tags(content)?);
    }

    // Numbered here, across every shape, so no two calls can share an id
    // however the content mixed them.
    for (i, call) in calls.iter_mut().enumerate() {
        call.id = format!("xml_{i}");
    }

    Ok(calls)
}

fn scrape_function_tags(content: &str) -> Result<Vec<ToolCall>, String> {
    let mut calls = Vec::new();
    let mut rest = content;
    while let Some(start) = rest.find("<function=") {
        let after = &rest[start + "<function=".len()..];
        let name_end = after
            .find('>')
            .ok_or_else(|| "malformed <function=…> tag (no closing '>')".to_string())?;
        let name = after[..name_end]
            .trim()
            .trim_end_matches('/')
            .trim()
            .to_string();
        let body_start = name_end + 1;
        // Body runs to </function> if present, else to end.
        let (body, consumed) = match after[body_start..].find("</function>") {
            Some(e) => (
                &after[body_start..body_start + e],
                body_start + e + "</function>".len(),
            ),
            None => (&after[body_start..], after.len()),
        };
        let mut obj = serde_json::Map::new();
        for param in blocks_between_prefixed(body, "<parameter=", "</parameter>") {
            obj.insert(param.0, Value::String(param.1.trim().to_string()));
        }
        if name.is_empty() {
            return Err("a <function=…> tag had an empty name".into());
        }
        let arguments = Value::Object(obj).to_string();
        // Id left empty: `scrape_xml` numbers the whole set once it has them
        // all, which is the only place that can see them all.
        calls.push(ToolCall::new(String::new(), name, arguments));
        rest = &after[consumed..];
    }
    Ok(calls)
}

/// Collect the inner text of every `open…close` pair (non-nested).
fn blocks_between<'a>(s: &'a str, open: &str, close: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find(open) {
        let after = &rest[i + open.len()..];
        match after.find(close) {
            Some(j) => {
                out.push(&after[..j]);
                rest = &after[j + close.len()..];
            }
            None => break,
        }
    }
    out
}

/// Like `blocks_between` but the open tag is `<parameter=KEY>` — returns (key, value).
fn blocks_between_prefixed(s: &str, open_prefix: &str, close: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find(open_prefix) {
        let after = &rest[i + open_prefix.len()..];
        let key_end = match after.find('>') {
            Some(k) => k,
            None => break,
        };
        let key = after[..key_end].trim().to_string();
        let val_region = &after[key_end + 1..];
        match val_region.find(close) {
            Some(j) => {
                out.push((key, val_region[..j].to_string()));
                rest = &val_region[j + close.len()..];
            }
            None => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completion(content: &str) -> Completion {
        Completion {
            content: content.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn done_when_no_tool_call() {
        match parse(&completion("All finished, the file compiles.")) {
            Action::Done(t) => assert!(t.contains("finished")),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn scrapes_hermes_json() {
        let c = completion(
            "sure\n<tool_call>{\"name\":\"read\",\"arguments\":{\"path\":\"a.rs\"}}</tool_call>",
        );
        match parse(&c) {
            Action::Calls(v) => {
                assert_eq!(v[0].name, "read");
                assert!(v[0].arguments.contains("a.rs"));
            }
            other => panic!("expected Calls, got {other:?}"),
        }
    }

    #[test]
    fn scrapes_qwen_xml() {
        let c = completion("<tool_call><function=bash><parameter=command>ls -la</parameter></function></tool_call>");
        match parse(&c) {
            Action::Calls(v) => {
                assert_eq!(v[0].name, "bash");
                let args: Value = serde_json::from_str(&v[0].arguments).unwrap();
                assert_eq!(args["command"], "ls -la");
            }
            other => panic!("expected Calls, got {other:?}"),
        }
    }

    #[test]
    fn repairs_unclosed_think() {
        let repaired = repair_unclosed_think("<think>hmm let me read it<tool_call>{}</tool_call>");
        assert!(repaired.contains("</think>"));
        assert!(repaired.find("</think>").unwrap() < repaired.find("<tool_call>").unwrap());
    }

    /// **Every scraped call gets its own id.**
    ///
    /// Ids used to be minted inside each scraper from that scraper's own count,
    /// so two `<tool_call>` blocks each wrapping a `<function=…>` both came out
    /// as `xml_0`. `Message::tool_result` correlates results to calls by id
    /// precisely so parallel calls cannot be mismatched — two calls sharing one
    /// id defeats that silently, and the second result overwrites the first's
    /// meaning rather than failing.
    #[test]
    fn parallel_scraped_calls_never_share_an_id() {
        let c = completion(
            "<tool_call><function=read><parameter=path>a.rs</parameter></function></tool_call>\n\
             <tool_call><function=read><parameter=path>b.rs</parameter></function></tool_call>",
        );
        match parse(&c) {
            Action::Calls(v) => {
                assert_eq!(v.len(), 2, "both calls should be scraped");
                assert_ne!(
                    v[0].id, v[1].id,
                    "two calls sharing an id cannot be told apart by their results"
                );
            }
            other => panic!("expected Calls, got {other:?}"),
        }
    }

    /// The same, across the two shapes mixed in one response — which is where
    /// the counters were independent and so guaranteed to collide.
    #[test]
    fn a_hermes_block_and_an_xml_block_get_distinct_ids() {
        let c = completion(
            "<tool_call>{\"name\":\"ls\",\"arguments\":{}}</tool_call>\n\
             <tool_call><function=read><parameter=path>a.rs</parameter></function></tool_call>",
        );
        match parse(&c) {
            Action::Calls(v) => {
                assert_eq!(v.len(), 2);
                assert_ne!(v[0].id, v[1].id);
                let ids: std::collections::HashSet<&str> =
                    v.iter().map(|c| c.id.as_str()).collect();
                assert_eq!(ids.len(), v.len(), "ids must be unique: {ids:?}");
            }
            other => panic!("expected Calls, got {other:?}"),
        }
    }

    #[test]
    fn malformed_json_args() {
        let c = Completion {
            tool_calls: vec![ToolCall::new("1", "read", "{not json")],
            ..Default::default()
        };
        match parse(&c) {
            Action::Malformed(e) => assert!(e.contains("not valid JSON")),
            other => panic!("expected Malformed, got {other:?}"),
        }
    }
}
