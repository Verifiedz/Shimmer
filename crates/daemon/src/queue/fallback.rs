//! Fallback rules (§11.3, ADR 0005) as plain functions over plain data. No runtime, no I/O
//! (§12 rule 10): the queue calls these, and a test can too.
//!
//! A fallback names a `notify.*` op and can never itself carry one, so the chain from a failed
//! task is at most one link and ends in `notify`'s own terminal, its inbox (ADR 0032).

use serde_json::{json, Value};
use shimmer_core::{EnqueueRequest, Error, ErrorCode, Fallback, ModuleId, Origin, Priority, Task};

const NOTIFY_PREFIX: &str = "notify.";

/// `notify.<verb>`, with a verb. The one place the queue looks at a namespace, and only as a
/// prefix: no module is named and no op is listed, so a sink or template op added to `notify`
/// later is automatically a valid fallback (§12 rule 7).
pub fn is_notify_op(op: &str) -> bool {
    op.strip_prefix(NOTIFY_PREFIX).is_some_and(|verb| !verb.is_empty())
}

/// Reject a fallback that breaks ADR 0005. `op` is the op the fallback is attached to.
///
/// Called at `scheduler.add`, at module trigger registration, when triggers are loaded, and by
/// the queue on every submit, so no path can put an invalid fallback in front of a lane.
pub fn validate(op: &str, fallback: Option<&Fallback>) -> Result<(), Error> {
    let Some(Fallback::Notify { fallback_op, .. }) = fallback else { return Ok(()) };
    if !is_notify_op(fallback_op) {
        return Err(Error::invalid_params(format!("fallback_op '{fallback_op}' must be a notify.* op")));
    }
    if is_notify_op(op) {
        return Err(Error::invalid_params(format!("'{op}' is a notify.* op and cannot declare a fallback")));
    }
    Ok(())
}

/// Failures the fallback exists for: the primary could not run, as opposed to running and
/// reporting a problem of its own. Decided by error code, not by op (ADR 0005).
pub fn is_structural(error: &Error) -> bool {
    matches!(error.code, ErrorCode::WorkspaceDirty | ErrorCode::Unavailable)
}

/// The task to enqueue when `failed` fails with `error`, if it has a fallback and the failure
/// is structural.
///
/// The fallback is enqueued as configured when the trigger was created, with two additions,
/// because the op only receives `params`. Into object params (null becomes an object) the queue
/// stamps, overwriting any key already there, the same way the scheduler stamps `scheduled_for`:
///
/// * `priority`: the fallback's `NotifyPriority`.
/// * `failure_reason`, `failure_code`, `failure_detail`: the message, code and `detail` of the
///   error, exactly as `queue.task.failed` reports them (`null` when there is no detail). For
///   `workspace_dirty`, `failure_detail` is the failed step and log path from `protocol.md`.
///
/// It carries no fallback of its own, and it enters the queue as `Normal` from the queue
/// itself, so it takes its ordinary turn in its lane and can never promote.
pub fn request(failed: &Task, error: &Error) -> Option<EnqueueRequest> {
    let Some(Fallback::Notify { fallback_op, params, priority }) = &failed.fallback else { return None };
    if !is_structural(error) {
        return None;
    }
    Some(EnqueueRequest {
        op: fallback_op.clone(),
        params: stamp(
            params,
            [
                ("priority", json!(priority)),
                ("failure_reason", json!(error.message)),
                ("failure_code", json!(error.code)),
                ("failure_detail", json!(error.detail)),
            ],
        ),
        lane: None,
        priority: Priority::Normal,
        origin: Origin::Module { id: ModuleId::new("queue") },
        fallback: None,
    })
}

/// Insert `fields` into object params, overwriting. Null becomes an object; anything else is
/// passed through untouched.
fn stamp<const N: usize>(params: &Value, fields: [(&str, Value); N]) -> Value {
    let mut m = match params {
        Value::Null => serde_json::Map::new(),
        Value::Object(m) => m.clone(),
        other => return other.clone(),
    };
    m.extend(fields.into_iter().map(|(k, v)| (k.to_owned(), v)));
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use shimmer_core::{NotifyPriority, TaskId};

    use super::*;

    fn notify(op: &str) -> Fallback {
        Fallback::Notify { fallback_op: op.into(), params: json!({"message": "m"}), priority: NotifyPriority::High }
    }

    fn task(fallback: Option<Fallback>) -> Task {
        Task {
            id: TaskId::new(),
            lane: "workspaces".into(),
            op: "workspaces.activate".into(),
            params: json!(null),
            priority: Priority::Scheduled,
            origin: Origin::User,
            fallback,
            enqueued_at: Utc::now(),
        }
    }

    #[test]
    fn only_notify_ops_with_a_verb_are_notify_ops() {
        assert!(is_notify_op("notify.send"));
        assert!(is_notify_op("notify.template.render"));
        for no in ["notify.", "notify", "notifications.send", "notify_x.send", "xnotify.send", "demo.notify.send", ""] {
            assert!(!is_notify_op(no), "{no}");
        }
    }

    #[test]
    fn validate_enforces_namespace_and_no_nesting() {
        assert!(validate("workspaces.activate", None).is_ok());
        assert!(validate("workspaces.activate", Some(&notify("notify.send"))).is_ok());
        assert!(validate("notify.send", None).is_ok(), "a notify op is fine as long as it declares no fallback");

        let e = validate("workspaces.activate", Some(&notify("records.list"))).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidParams);
        assert!(validate("workspaces.activate", Some(&notify("notify."))).is_err());
        assert!(validate("workspaces.activate", Some(&notify("notifications.send"))).is_err());

        let e = validate("notify.send", Some(&notify("notify.desktop"))).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidParams, "no nested fallbacks, even to another notify op");
    }

    #[test]
    fn only_structural_failures_produce_a_fallback() {
        let t = task(Some(notify("notify.send")));
        for code in [ErrorCode::WorkspaceDirty, ErrorCode::Unavailable] {
            assert!(request(&t, &Error::new(code, "x")).is_some(), "{code}");
        }
        for code in [ErrorCode::ModuleError, ErrorCode::Internal, ErrorCode::InvalidParams, ErrorCode::NotFound] {
            assert!(request(&t, &Error::new(code, "x")).is_none(), "{code}");
        }
        assert!(request(&task(None), &Error::unavailable("x")).is_none(), "no fallback declared, none invented");
    }

    #[test]
    fn the_fallback_request_is_the_configured_op_plus_what_the_queue_stamps() {
        let t = task(Some(notify("notify.send")));
        let r = request(&t, &Error::new(ErrorCode::WorkspaceDirty, "dirty")).unwrap();
        assert_eq!(r.op, "notify.send");
        assert_eq!(
            r.params,
            json!({"message": "m", "priority": "high", "failure_reason": "dirty",
                   "failure_code": "workspace_dirty", "failure_detail": null}),
            "configured params, plus what the queue stamps"
        );
        assert_eq!(r.lane, None, "the lane comes from the op's own declaration");
        assert_eq!(r.priority, Priority::Normal, "a fallback can never promote itself");
        assert_eq!(r.origin, Origin::Module { id: ModuleId::new("queue") });
        assert_eq!(r.fallback, None, "and never nests");
    }

    #[test]
    fn stamping_overwrites_and_reaches_only_object_or_null_params() {
        let with = |params: Value, error: Error| {
            let f = Fallback::Notify { fallback_op: "notify.send".into(), params, priority: NotifyPriority::Low };
            request(&task(Some(f)), &error).unwrap().params
        };
        let dirty = Error::new(ErrorCode::WorkspaceDirty, "step 3 failed")
            .with_detail(json!({"workspace": "deep-work", "log": "logs/deep-work.log"}));

        let stamped = with(json!(null), dirty.clone());
        assert_eq!(stamped["priority"], "low");
        assert_eq!(stamped["failure_reason"], "step 3 failed");
        assert_eq!(stamped["failure_code"], "workspace_dirty");
        assert_eq!(stamped["failure_detail"], json!({"workspace": "deep-work", "log": "logs/deep-work.log"}));

        // Overwrite-if-present, for every stamped key; unrelated keys survive.
        let mine = json!({"priority": "high", "failure_reason": "mine", "failure_code": "mine",
                          "failure_detail": "mine", "message": "keep me"});
        let stamped = with(mine, dirty);
        assert_eq!(stamped["priority"], "low");
        assert_eq!(stamped["failure_reason"], "step 3 failed");
        assert_eq!(stamped["failure_code"], "workspace_dirty");
        assert!(stamped["failure_detail"].is_object());
        assert_eq!(stamped["message"], "keep me");

        assert_eq!(with(json!("plain text"), Error::unavailable("x")), json!("plain text"));
        assert_eq!(with(json!([1, 2]), Error::unavailable("x")), json!([1, 2]));
    }
}
