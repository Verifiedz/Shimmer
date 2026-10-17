//! The built-in alerts (ADR 0032 §4): on for everyone by default, so a fresh install still says
//! when a workspace went dirty (CLAUDE.md §10.3) or a scheduled task failed. Pure: an event in,
//! the `notify.send` params it calls for out.
//!
//! One message per failure, never two: a failed task says in `fallback` whether its own fallback
//! is being sent, and then that fallback is the message. An alert never fires for notify's own
//! work (a failed `notify.*` task): the inbox already has that message, as `failed`.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use shimmer_core::Event;

/// `[builtin_alerts]` in the settings: each alert on unless turned off.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Alerts {
    #[serde(default = "on")]
    pub workspace_dirty: bool,
    #[serde(default = "on")]
    pub scheduled_task_failed: bool,
}

fn on() -> bool {
    true
}

impl Default for Alerts {
    fn default() -> Self {
        Self { workspace_dirty: true, scheduled_task_failed: true }
    }
}

impl Alerts {
    /// Switches one alert by name.
    pub fn set(&mut self, name: &str, on: bool) -> Result<(), String> {
        match name.replace('-', "_").as_str() {
            "workspace_dirty" => self.workspace_dirty = on,
            "scheduled_task_failed" => self.scheduled_task_failed = on,
            _ => return Err(format!("no built-in alert '{name}': there's workspace-dirty and scheduled-task-failed")),
        }
        Ok(())
    }
}

/// Every topic [`alert`] can react to, so `on_event` reads settings only for these.
pub const TOPICS: [&str; 2] = ["queue.task.failed", "workspaces.session.dirty"];

/// The message `ev` calls for, as `notify.send` params, if any.
pub fn alert(ev: &Event, alerts: &Alerts) -> Option<Value> {
    let p = &ev.payload;
    let text = |v: &Value| v.as_str().unwrap_or_default().to_owned();
    match ev.topic.as_str() {
        "queue.task.failed" => {
            let op = text(&p["op"]);
            // `fallback` is absent before the daemon reports it (ADR 0032 §4): read as false.
            if op.starts_with("notify.") || p["fallback"] == true {
                return None;
            }
            if p["code"] == "workspace_dirty" {
                if !alerts.workspace_dirty {
                    return None;
                }
                let d = &p["detail"];
                let ws = text(&d["workspace"]);
                let fix = if d["has_cleanup_script"] == true { "cleanup" } else { "reset" };
                return Some(message(
                    "workspace_dirty",
                    format!("{} couldn't launch", if ws.is_empty() { "A workspace".into() } else { ws.clone() }),
                    format!(
                        "Step {} failed. Log: {}. Then: shimmer workspaces {fix} {ws}",
                        text(&d["failed_step"]),
                        text(&d["log"])
                    ),
                ));
            }
            if p["origin"]["type"] != "scheduler" || !alerts.scheduled_task_failed {
                return None;
            }
            Some(message("scheduled_task_failed", format!("Scheduled {op} failed"), text(&p["error"])))
        }
        // A launch the daemon never finished (it stopped mid-launch): no task failed to say so.
        "workspaces.session.dirty" if p["recovered"] == true && alerts.workspace_dirty => {
            let ws = text(&p["workspace"]);
            Some(message(
                "workspace_dirty",
                format!("{ws} was left half-launched"),
                format!(
                    "Shimmer stopped during step {}. Log: {}. Then: shimmer workspaces cleanup {ws}",
                    text(&p["failed_step"]),
                    text(&p["log"])
                ),
            ))
        }
        _ => None,
    }
}

fn message(alert: &str, title: String, body: String) -> Value {
    json!({"title": title, "body": body, "priority": "high", "alert": alert})
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;
    use shimmer_core::ModuleId;

    use super::*;

    fn ev(topic: &str, payload: Value) -> Event {
        Event::new(ModuleId::new(topic.split('.').next().unwrap()), topic, payload, DateTime::UNIX_EPOCH)
    }

    fn dirty_failure(fallback: Option<bool>) -> Event {
        let mut p = json!({"task_id": "t1", "lane": "workspaces", "op": "workspaces.activate",
            "origin": {"type": "scheduler", "trigger_id": "usr-1"}, "error": "workspace 'site' failed", "code": "workspace_dirty",
            "detail": {"workspace": "site", "failed_step": "2/5 install", "log": "logs/site-01.log", "has_cleanup_script": true}});
        if let Some(f) = fallback {
            p["fallback"] = json!(f);
        }
        ev("queue.task.failed", p)
    }

    #[test]
    fn an_alert_is_switched_by_name() {
        let mut a = Alerts::default();
        a.set("workspace-dirty", false).unwrap();
        assert!(!a.workspace_dirty && a.scheduled_task_failed);
        a.set("scheduled_task_failed", false).unwrap();
        assert!(!a.scheduled_task_failed);
        assert!(a.set("disk-full", true).unwrap_err().contains("workspace-dirty and scheduled-task-failed"));
    }

    #[test]
    fn a_dirty_launch_is_one_high_message_unless_its_fallback_says_it() {
        let m = alert(&dirty_failure(Some(false)), &Alerts::default()).unwrap();
        assert_eq!(m["title"], "site couldn't launch");
        assert_eq!(m["body"], "Step 2/5 install failed. Log: logs/site-01.log. Then: shimmer workspaces cleanup site");
        assert_eq!((m["priority"].as_str(), m["alert"].as_str()), (Some("high"), Some("workspace_dirty")));
        assert_eq!(alert(&dirty_failure(None), &Alerts::default()).unwrap()["alert"], "workspace_dirty");
        assert_eq!(alert(&dirty_failure(Some(true)), &Alerts::default()), None, "the fallback is the message");
        let off = Alerts { workspace_dirty: false, ..Alerts::default() };
        assert_eq!(alert(&dirty_failure(Some(false)), &off), None);
    }

    #[test]
    fn a_launch_left_half_done_by_a_restart_is_reported_once() {
        let p =
            json!({"workspace": "site", "reason": "the daemon stopped", "failed_step": "3/5 db", "log": "logs/x.log"});
        assert_eq!(alert(&ev("workspaces.session.dirty", p.clone()), &Alerts::default()), None, "a task reports it");
        let mut recovered = p;
        recovered["recovered"] = json!(true);
        let m = alert(&ev("workspaces.session.dirty", recovered), &Alerts::default()).unwrap();
        assert_eq!(m["title"], "site was left half-launched");
    }

    #[test]
    fn only_scheduled_failures_and_never_notifys_own() {
        let failed = |op: &str, origin: Value| {
            ev(
                "queue.task.failed",
                json!({"op": op, "origin": origin, "error": "boom", "code": "module_error", "fallback": false}),
            )
        };
        let sched = json!({"type": "scheduler", "trigger_id": "usr-1"});
        let m = alert(&failed("records.complete", sched.clone()), &Alerts::default()).unwrap();
        assert_eq!(
            (m["title"].as_str(), m["body"].as_str()),
            (Some("Scheduled records.complete failed"), Some("boom"))
        );
        assert_eq!(alert(&failed("records.complete", json!({"type": "user"})), &Alerts::default()), None);
        assert_eq!(alert(&failed("notify.send", sched.clone()), &Alerts::default()), None);
        let off = Alerts { scheduled_task_failed: false, ..Alerts::default() };
        assert_eq!(alert(&failed("records.complete", sched), &off), None);
        assert_eq!(alert(&ev("records.item.completed", json!({})), &Alerts::default()), None);
    }

    /// `on_event` reads settings only for `TOPICS`, so no other topic may ever make an alert.
    #[test]
    fn only_its_topics_ever_make_an_alert() {
        let p = json!({"origin": {"type": "scheduler"}, "code": "workspace_dirty", "recovered": true,
                       "fallback": false, "op": "demo.op", "error": "boom", "workspace": "site"});
        for topic in
            ["queue.task.finished", "queue.task.cancelled", "workspaces.session.launched", "records.item.created"]
        {
            assert!(!TOPICS.contains(&topic));
            assert_eq!(alert(&ev(topic, p.clone()), &Alerts::default()), None, "{topic}");
        }
    }
}
