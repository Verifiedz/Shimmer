//! Built-in rules (ADR 0035 §10): shipped with Shimmer, on unless switched off (`[builtin]` in the
//! settings), never removable, written in the same rule language as yours. One whose op no
//! installed module has (notify's, before notify is there) is simply not loaded.
//!
//! Notify's own work is never alerted on: its tasks run in the `notify` lane. A failed task whose
//! own fallback is being sent (`fallback`) is left to that fallback: one message, never two.

/// `(rule id, its [builtin] key, its text)`.
pub const RULES: [(&str, &str, &str); 4] = [
    (
        "workspace-dirty",
        "workspace_dirty",
        r#"# Built in: switch it off with 'shimmer automations disable workspace-dirty'.
[rule]
label = "Tell me when a workspace couldn't launch"
on = "queue.task.failed"
when = { code = "workspace_dirty", fallback = { ne = true }, lane = { ne = "notify" } }

[[do]]
op = "notify.send"
params = { title = "{detail.workspace} couldn't launch", body = "Step {detail.failed_step} failed. Log: {detail.log}. Then: {detail.fix}", priority = "high", from = "automation:workspace-dirty" }
"#,
    ),
    (
        "workspace-half-launched",
        "workspace_half_launched",
        r#"# Built in: switch it off with 'shimmer automations disable workspace-half-launched'.
[rule]
label = "Tell me when a workspace was left half-launched"
on = "workspaces.session.dirty"
when = { recovered = true }

[[do]]
op = "notify.send"
params = { title = "{workspace} was left half-launched", body = "Shimmer stopped during step {failed_step}. Log: {log}. Then: shimmer workspaces cleanup {workspace}", priority = "high", from = "automation:workspace-half-launched" }
"#,
    ),
    (
        "scheduled-task-failed",
        "scheduled_task_failed",
        r#"# Built in: switch it off with 'shimmer automations disable scheduled-task-failed'.
[rule]
label = "Tell me when a scheduled task fails"
on = "queue.task.failed"
when = { "origin.type" = "scheduler", code = { ne = "workspace_dirty" }, fallback = { ne = true }, lane = { ne = "notify" } }

[[do]]
op = "notify.send"
params = { title = "Scheduled {op} failed", body = "{error}", priority = "high", from = "automation:scheduled-task-failed" }
"#,
    ),
    (
        "loop-stopped",
        "loop_stopped",
        r#"# Built in: switch it off with 'shimmer automations disable loop-stopped'.
[rule]
label = "Tell me when a loop of rules was stopped"
on = "automations.chain.stopped"

[[do]]
op = "notify.send"
params = { title = "Automation {rule} was stopped", body = "It would have set itself off again ({reason}). What happened: shimmer automations history {rule}", priority = "high", from = "automation:loop-stopped" }
"#,
    ),
];

/// The built-in rule `id`'s `[builtin]` key and text.
pub fn find(id: &str) -> Option<(&'static str, &'static str)> {
    RULES.iter().find(|(r, _, _)| *r == id).map(|(_, key, text)| (*key, *text))
}
