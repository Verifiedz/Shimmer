//! Rule templates (ADR 0035 §9) against an in-memory `Ctx`: every template makes rules that
//! check, with and without its optional answers; preview writes nothing; a template whose ops
//! aren't installed says which module it needs.

use std::sync::Arc;

use serde_json::{json, Map, Value};
use shimmer_automations::Automations;
use shimmer_core::testing::TestEnv;
use shimmer_core::{Catalog, CommandSpec, ErrorCode, Execution, Module};

const OPS: [&str; 5] = ["notify.send", "workspaces.activate", "workspaces.new", "workspaces.stop", "records.update"];
const TOPICS: [&str; 5] = [
    "records.item.created",
    "records.item.updated",
    "records.item.completed",
    "queue.task.failed",
    "workspaces.workspace.created",
];

fn env(ops: &[&str]) -> TestEnv {
    let mut env = TestEnv::new("automations");
    let spec = |op: &&str| CommandSpec {
        op: op.to_string(),
        summary: String::new(),
        params_schema: json!({}),
        execution: Execution::Inline,
    };
    env.ctx.catalog = Arc::new(Catalog::new(ops.iter().map(spec), TOPICS.map(|t| (t.to_owned(), None))));
    env
}

/// An answer that fits each kind, for every question (optional ones only when `all`).
fn answers(t: &Value, all: bool) -> Map<String, Value> {
    let mut out = Map::new();
    for q in t["questions"].as_array().unwrap() {
        if q["required"] != true && !all {
            continue;
        }
        let a = match q["kind"].as_str().unwrap() {
            "name" => "jobs",
            "text" => "offer \"now\"",
            "number" => "3",
            "choice" => q["choices"][0].as_str().unwrap(),
            "list" => "offer, rejected",
            "pairs" => "COMPANY=title, JOB_URL=item.url",
            k => panic!("a kind with no sample: {k}"),
        };
        out.insert(q["name"].as_str().unwrap().to_owned(), json!(a));
    }
    out
}

#[tokio::test]
async fn every_template_makes_rules_that_check_with_and_without_optional_answers() {
    let env = env(&OPS);
    let m = Automations::default();
    m.init(&env.ctx).await.unwrap();
    let list = m.handle("automations.templates", json!({}), &env.ctx).await.unwrap();
    let templates = list["templates"].as_array().unwrap();
    assert_eq!(templates.len(), 9);
    for t in templates {
        let id = t["id"].as_str().unwrap();
        for all in [false, true] {
            let rule = format!("{id}-{all}");
            let params = json!({"id": rule, "template": id, "values": answers(t, all)});
            let made = m.handle("automations.new", params, &env.ctx).await;
            let made = made.unwrap_or_else(|e| panic!("{id} (all answers: {all}): {}", e.message));
            let rules = made["rules"].as_array().unwrap();
            assert_eq!(rules.len(), t["rules"].as_array().unwrap().len(), "{id}");
            assert!(rules.iter().all(|r| r["enabled"] == true), "{id}: made rules are on");
        }
    }
    let status = m.handle("automations.status", json!({}), &env.ctx).await.unwrap();
    assert_eq!(status["problems"], json!([]));
    // stage-creates-workspace makes three rules; the two built-ins on queue.task.failed are there too.
    assert_eq!(status["rules"], 2 * (9 + 2) + 2);
}

#[tokio::test]
async fn preview_shows_the_rules_and_makes_nothing() {
    let env = env(&OPS);
    let m = Automations::default();
    m.init(&env.ctx).await.unwrap();
    let values = json!({"COLLECTION": "jobs", "FIELD": "stage", "VALUE": "interviewing", "TEMPLATE": "interview-prep",
                        "CLOSED": "offer, rejected", "ANSWERS": "COMPANY=title"});
    let params = json!({"id": "prep", "template": "stage-creates-workspace", "values": values, "preview": true});
    let shown = m.handle("automations.new", params.clone(), &env.ctx).await.unwrap();
    let ids: Vec<&str> = shown["rules"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["prep-make", "prep-open", "prep-stop"]);
    let make = shown["rules"][0]["text"].as_str().unwrap();
    assert!(
        make.contains(
            r#"params = { id = "{id}-interview-prep", template = "interview-prep", values = { COMPANY = "{title}" } }"#
        ),
        "{make}"
    );
    let remove = shown["rules"][2]["text"].as_str().unwrap();
    assert!(remove.contains(r#""item.stage" = { in = ["offer", "rejected"] }"#), "{remove}");
    assert!(env.backend.events().is_empty(), "nothing written");

    // Made for real, then the same ids clash.
    let mut real = params.clone();
    real["preview"] = json!(false);
    m.handle("automations.new", real.clone(), &env.ctx).await.unwrap();
    assert_eq!(env.backend.events().len(), 3);
    let again = m.handle("automations.new", real, &env.ctx).await.unwrap_err();
    assert_eq!(again.code, ErrorCode::Conflict);
    assert!(again.message.contains("prep-make, prep-open, prep-stop"), "{}", again.message);
}

#[tokio::test]
async fn mistakes_and_missing_modules_are_explained() {
    let env = env(&["workspaces.activate"]);
    let m = Automations::default();
    m.init(&env.ctx).await.unwrap();
    let call = |p: Value| m.handle("automations.new", p, &env.ctx);

    let e = call(json!({"id": "x", "template": "notify-on-add", "values": {"COLLECTION": "jobs"}})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Unavailable);
    assert!(e.message.contains("needs notify.send from the notify module"), "{}", e.message);

    // Several ops from one module name it once (#254's review).
    let env2 = crate::env(&[]);
    let m2 = Automations::default();
    m2.init(&env2.ctx).await.unwrap();
    let e = m2
        .handle("automations.new", json!({"id": "x", "template": "stage-creates-workspace", "values": {}}), &env2.ctx)
        .await
        .unwrap_err();
    assert!(e.message.contains("from the workspaces module, which isn't"), "{}", e.message);

    let e = call(json!({"id": "x", "template": "stage-opens-workspace", "values": {"COLLECTION": "jobs"}}))
        .await
        .unwrap_err();
    assert!(e.message.contains("FIELD (Which field (e.g. stage)) is required"), "{}", e.message);
    let bad = json!({"COLLECTION": "Jobs", "FIELD": "stage", "VALUE": "x", "WORKSPACE": "prep"});
    let e = call(json!({"id": "x", "template": "stage-opens-workspace", "values": bad})).await.unwrap_err();
    assert!(e.message.contains("'Jobs' isn't an id"), "{}", e.message);
    let e = call(json!({"id": "x", "template": "nope"})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
    let e = call(json!({"id": "x", "template": "stage-opens-workspace", "text": "x"})).await.unwrap_err();
    assert!(e.message.contains("not both"), "{}", e.message);
}
