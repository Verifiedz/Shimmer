//! `--first` (#222): queue a command ahead of everything waiting in its lane (CLAUDE.md §6.2).
//!
//! The daemon never does this silently. Asked with `"priority": "override"`, it answers
//! `confirmation_required` naming the tasks it would run ahead of and the lane's `queue_version`;
//! the client shows them, asks, and re-sends with `confirm` and that version (docs/protocol.md,
//! "Priority promotion"). If the lane changed in between, the daemon asks again with the fresh
//! list, and so does this. The running task is never displaced; only queued ones are.
//!
//! One helper for every command that queues work, so each one adds the option, not the round-trip.

use std::fmt::Write as _;

use serde_json::Value;
use shimmer_core::{Error, ErrorCode, Result};
use shimmer_proto::{QueueControl, QueuePriority};

use crate::client::Client;
use crate::workspaces::Prompt;

/// How many times the lane may change between showing it and confirming before giving up.
const TRIES: usize = 5;

/// Send `op` ahead of its lane. `Ok(None)`: the person said no, and nothing was queued. `yes`
/// confirms without asking; without it and without a terminal, it refuses.
pub async fn send(
    client: &mut Client,
    op: &str,
    params: Value,
    yes: bool,
    prompt: &mut dyn Prompt,
) -> Result<Option<Value>> {
    let mut queue = QueueControl { priority: QueuePriority::Override, confirm: false, queue_version: None };
    for _ in 0..TRIES {
        let e = match client.call_queued(op, params.clone(), Some(queue.clone())).await {
            Err(e) if e.code == ErrorCode::ConfirmationRequired => e,
            // Nothing was waiting, or it's confirmed: queued first. Or a real error.
            other => return other.map(Some),
        };
        let detail = e.detail.unwrap_or_default();
        let version = detail["queue_version"]
            .as_u64()
            .ok_or_else(|| Error::internal("confirmation_required without a queue_version"))?;
        if !yes {
            if !prompt.interactive() {
                return Err(Error::invalid_params(format!(
                    "refusing to run {op} ahead of the tasks waiting in its lane without confirmation; \
                     pass --yes to do it anyway"
                )));
            }
            eprint!("{}", displaced(&detail, queue.confirm));
            if !prompt.confirm("Run it first?") {
                return Ok(None);
            }
        }
        queue = QueueControl { priority: QueuePriority::Override, confirm: true, queue_version: Some(version) };
    }
    Err(Error::conflict(format!("the lane kept changing while asking to run {op} first; try again")))
}

/// What running first would put behind it, from `confirmation_required`'s detail.
/// `again`: the lane changed since the person was last shown it.
pub fn displaced(detail: &Value, again: bool) -> String {
    let tasks = detail["would_displace"].as_array().map(Vec::as_slice).unwrap_or_default();
    let lane = detail["lane"].as_str().unwrap_or("its");
    let n = tasks.len();
    let what = if n == 1 { "1 task".to_owned() } else { format!("{n} tasks") };
    let mut out = match again {
        false => format!("This runs ahead of {what} waiting in the {lane} lane:\n"),
        true => format!("The {lane} lane changed. Now it runs ahead of {what}:\n"),
    };
    let width = tasks.iter().map(|t| t["task_id"].as_str().unwrap_or_default().len()).max().unwrap_or(0);
    let op_width = tasks.iter().map(|t| t["op"].as_str().unwrap_or_default().len()).max().unwrap_or(0);
    for t in tasks {
        let id = t["task_id"].as_str().unwrap_or_default();
        let op = t["op"].as_str().unwrap_or_default();
        let _ = writeln!(out, "  {id:width$}  {op:op_width$}  ({})", came_from(&t["priority"]));
    }
    out
}

/// A waiting task's `priority`, in words: where it came from (§6.2).
fn came_from(priority: &Value) -> &'static str {
    match priority.as_str() {
        Some("scheduled") => "scheduled",
        Some("normal") => "queued",
        // `{"overridden": {…}}`: someone already ran it first.
        _ if priority.is_object() => "also run first",
        _ => "queued",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;
    use shimmer_proto::{decode_client, encode, ClientFrame, ServerFrame};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use super::*;

    /// Answers like the daemon's queue: an override is refused with `would` and the lane's
    /// version until it comes back confirmed with that version. Each request's `queue` block is
    /// recorded. `bump`: the version moves on once, after the first refusal, as if a task arrived.
    async fn lane(sock: &std::path::Path, would: usize, mut bump: bool) -> Arc<Mutex<Vec<Option<QueueControl>>>> {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let listener = tokio::net::UnixListener::bind(sock).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (rd, mut wr) = stream.into_split();
            let mut lines = BufReader::new(rd).lines();
            let mut version = 41;
            while let Ok(Some(line)) = lines.next_line().await {
                let frame = match decode_client(&line).unwrap() {
                    ClientFrame::Hello { .. } => ServerFrame::welcome("test", "s1"),
                    ClientFrame::Request { id, queue, .. } => {
                        log.lock().unwrap().push(queue.clone());
                        let q = queue.unwrap_or_default();
                        if would == 0 || (q.confirm && q.queue_version == Some(version)) {
                            ServerFrame::ok(
                                id,
                                json!({"queued": true, "task_id": "t-new", "lane": "workspaces", "position": 0}),
                            )
                        } else {
                            let list: Vec<Value> = (0..would)
                                .map(|i| json!({"task_id": format!("t{i}"), "op": "workspaces.activate", "priority": "scheduled"}))
                                .collect();
                            let e = Error::new(ErrorCode::ConfirmationRequired, "would displace").with_detail(
                                json!({"lane": "workspaces", "queue_version": version, "would_displace": list}),
                            );
                            // Once: a task arrived right after the person was shown the lane.
                            if std::mem::take(&mut bump) {
                                version += 1;
                            }
                            ServerFrame::err(id, e)
                        }
                    }
                    _ => continue,
                };
                wr.write_all(encode(&frame).unwrap().as_bytes()).await.unwrap();
            }
        });
        seen
    }

    struct Answers {
        interactive: bool,
        answers: Vec<bool>,
    }

    impl Prompt for Answers {
        fn interactive(&self) -> bool {
            self.interactive
        }
        fn confirm(&mut self, _: &str) -> bool {
            assert!(!self.answers.is_empty(), "asked more often than expected");
            self.answers.remove(0)
        }
    }

    async fn run(
        would: usize,
        bump: bool,
        yes: bool,
        prompt: &mut Answers,
    ) -> (Result<Option<Value>>, Vec<Option<QueueControl>>) {
        let dir = tempfile::TempDir::new().unwrap();
        let sock = dir.path().join("s.sock");
        let seen = lane(&sock, would, bump).await;
        let mut client = Client::connect(&sock).await.unwrap();
        let out = send(&mut client, "workspaces.activate", json!({"id": "x"}), yes, prompt).await;
        let seen = seen.lock().unwrap().clone();
        (out, seen)
    }

    fn asked(version: Option<u64>) -> Option<QueueControl> {
        Some(QueueControl { priority: QueuePriority::Override, confirm: version.is_some(), queue_version: version })
    }

    #[tokio::test]
    async fn nothing_waiting_is_queued_first_without_a_question() {
        let mut p = Answers { interactive: true, answers: vec![] };
        let (out, seen) = run(0, false, false, &mut p).await;
        assert_eq!(out.unwrap().unwrap()["position"], 0);
        assert_eq!(seen, [asked(None)]);
    }

    #[tokio::test]
    async fn it_asks_then_confirms_with_the_version_it_showed() {
        let mut p = Answers { interactive: true, answers: vec![true] };
        let (out, seen) = run(2, false, false, &mut p).await;
        assert_eq!(out.unwrap().unwrap()["task_id"], "t-new");
        assert_eq!(seen, [asked(None), asked(Some(41))]);
    }

    #[tokio::test]
    async fn no_sends_nothing_more() {
        let mut p = Answers { interactive: true, answers: vec![false] };
        let (out, seen) = run(2, false, false, &mut p).await;
        assert!(out.unwrap().is_none());
        assert_eq!(seen, [asked(None)], "declined: no second request");
    }

    #[tokio::test]
    async fn a_lane_that_changed_is_shown_and_asked_again() {
        // The version moves on after the first refusal, so the first confirmation is stale; the
        // person is asked again about the fresh list, and that confirmation carries its version.
        let mut p = Answers { interactive: true, answers: vec![true, true] };
        let (out, seen) = run(2, true, false, &mut p).await;
        assert!(out.unwrap().is_some());
        assert_eq!(seen, [asked(None), asked(Some(41)), asked(Some(42))]);
    }

    #[tokio::test]
    async fn without_a_terminal_it_refuses_unless_yes() {
        let mut script = Answers { interactive: false, answers: vec![] };
        let (out, seen) = run(2, false, false, &mut script).await;
        assert!(out.unwrap_err().message.contains("pass --yes"));
        assert_eq!(seen.len(), 1);

        let (out, seen) = run(2, false, true, &mut script).await;
        assert!(out.unwrap().is_some(), "--yes confirms without asking");
        assert_eq!(seen, [asked(None), asked(Some(41))]);
    }

    #[test]
    fn the_list_says_what_it_runs_ahead_of_and_where_each_came_from() {
        let detail = json!({"lane": "workspaces", "queue_version": 3, "would_displace": [
            {"task_id": "01M4AB", "op": "workspaces.activate", "priority": "scheduled"},
            {"task_id": "01M4C", "op": "workspaces.cleanup", "priority": "normal"},
            {"task_id": "01M4D", "op": "workspaces.stop", "priority": {"overridden": {"by": "user", "at": "x"}}}]});
        assert_eq!(
            displaced(&detail, false),
            "This runs ahead of 3 tasks waiting in the workspaces lane:\n  \
             01M4AB  workspaces.activate  (scheduled)\n  \
             01M4C   workspaces.cleanup   (queued)\n  \
             01M4D   workspaces.stop      (also run first)\n"
        );
        assert!(displaced(&detail, true).starts_with("The workspaces lane changed. Now it runs ahead of 3 tasks:"));
    }
}
