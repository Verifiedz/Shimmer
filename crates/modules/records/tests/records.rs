//! The records module through its only entry points, `init` and `handle`, against the in-memory
//! `Ctx` (CLAUDE.md §12 rule 13): files and events are inspected on the fake backend.

use std::time::Duration;

use serde_json::{json, Value};
use shimmer_core::testing::TestEnv;
use shimmer_core::{ErrorCode, Module, Result, StoreBackend};
use shimmer_records::Records;

/// A LeetCode-style collection most tests use, written in by the test itself: a fresh install
/// has no collections (ADR 0023).
const LEETCODE: &str = include_str!("fixtures/leetcode.toml");

async fn setup() -> (Records, TestEnv) {
    let env = TestEnv::new("records");
    let records = Records::default();
    records.init(&env.ctx).await.unwrap();
    env.ctx.store.write("collections/leetcode.toml", LEETCODE).unwrap();
    (records, env)
}

async fn call(r: &Records, env: &TestEnv, op: &str, params: Value) -> Result<Value> {
    r.handle(op, params, &env.ctx).await
}

fn file(env: &TestEnv, path: &str) -> Option<String> {
    env.backend.read("records", path).unwrap().map(|b| String::from_utf8(b).unwrap())
}

fn topics(env: &TestEnv) -> Vec<String> {
    env.backend.events().into_iter().map(|e| e.topic).collect()
}

async fn add_two_sum(r: &Records, env: &TestEnv) -> Value {
    call(
        r,
        env,
        "records.add",
        json!({"collection": "leetcode", "id": "two-sum",
        "fields": {"title": "Two Sum", "difficulty": "easy"}}),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn a_fresh_install_has_no_collections() {
    // ADR 0023: nothing is created on first start; the person picks templates.
    let env = TestEnv::new("records");
    let records = Records::default();
    records.init(&env.ctx).await.unwrap();
    records.init(&env.ctx).await.unwrap();
    assert!(env.backend.events().is_empty(), "init writes nothing");
    let data = call(&records, &env, "records.collections", json!({})).await.unwrap();
    assert_eq!(data["collections"], json!([]));
}

#[tokio::test]
async fn add_writes_one_file_and_one_event() {
    let (r, env) = setup().await;
    let item = add_two_sum(&r, &env).await;
    assert_eq!(
        item,
        json!({"id": "two-sum", "status": "todo", "title": "Two Sum", "difficulty": "easy",
               "url": null, "last_solved": null})
    );
    let text = file(&env, "items/leetcode/two-sum.toml").unwrap();
    assert!(text.contains("status = \"todo\"") && text.contains("title = \"Two Sum\""), "{text}");

    let ev = env.backend.events().pop().unwrap();
    assert_eq!(ev.topic, "records.item.created");
    assert_eq!(
        ev.payload,
        json!({"collection": "leetcode", "id": "two-sum", "title": "Two Sum", "deadlines": {}, "item": item})
    );
}

#[tokio::test]
async fn add_rejects_bad_input_and_changes_nothing() {
    let (r, env) = setup().await;
    add_two_sum(&r, &env).await;
    let before = topics(&env).len();

    let cases = [
        (json!({"collection": "leetcode", "id": "two-sum", "fields": {"title": "Again"}}), ErrorCode::Conflict),
        (json!({"collection": "nope", "id": "x", "fields": {}}), ErrorCode::NotFound),
        (json!({"collection": "leetcode", "id": "Bad Id", "fields": {"title": "x"}}), ErrorCode::InvalidParams),
        (json!({"collection": "leetcode", "id": "x", "fields": {}}), ErrorCode::InvalidParams),
        (
            json!({"collection": "leetcode", "id": "x", "fields": {"title": "x", "difficulty": "trivial"}}),
            ErrorCode::InvalidParams,
        ),
        (
            json!({"collection": "leetcode", "id": "x", "fields": {"title": "x", "colour": "red"}}),
            ErrorCode::InvalidParams,
        ),
        (json!({"collection": "leetcode", "id": "x", "fields": {"title": "x", "url": null}}), ErrorCode::InvalidParams),
        (json!({"collection": "leetcode"}), ErrorCode::InvalidParams),
    ];
    for (params, code) in cases {
        let e = call(&r, &env, "records.add", params.clone()).await.unwrap_err();
        assert_eq!(e.code, code, "{params}: {}", e.message);
    }
    assert_eq!(topics(&env).len(), before, "a failed request emits nothing (all or nothing)");
    assert!(file(&env, "items/leetcode/x.toml").is_none());
}

#[tokio::test]
async fn unknown_collection_matches_the_mock_fixture() {
    let (r, env) = setup().await;
    let e = call(&r, &env, "records.list", json!({"collection": "nope"})).await.unwrap_err();
    assert_eq!((e.code, e.message.as_str()), (ErrorCode::NotFound, "no collection 'nope'"));
}

#[tokio::test]
async fn list_filters_orders_and_pages() {
    let (r, env) = setup().await;
    for (id, difficulty) in [("c-hard", "hard"), ("a-easy", "easy"), ("b-easy", "easy")] {
        call(
            &r,
            &env,
            "records.add",
            json!({"collection": "leetcode", "id": id,
            "fields": {"title": id, "difficulty": difficulty}}),
        )
        .await
        .unwrap();
    }
    let ids = |v: &Value| {
        v["items"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap().to_owned()).collect::<Vec<_>>()
    };

    let all = call(&r, &env, "records.list", json!({"collection": "leetcode"})).await.unwrap();
    assert_eq!((ids(&all), all["total"].clone()), (vec!["a-easy".into(), "b-easy".into(), "c-hard".into()], json!(3)));

    let easy = call(&r, &env, "records.list", json!({"collection": "leetcode", "filter": {"difficulty": "easy"}}))
        .await
        .unwrap();
    assert_eq!(ids(&easy), ["a-easy", "b-easy"]);

    let page =
        call(&r, &env, "records.list", json!({"collection": "leetcode", "limit": 1, "offset": 1})).await.unwrap();
    assert_eq!((ids(&page), page["total"].clone()), (vec!["b-easy".into()], json!(3)), "total counts before paging");

    let unset =
        call(&r, &env, "records.list", json!({"collection": "leetcode", "filter": {"url": null}})).await.unwrap();
    assert_eq!(unset["total"], 3);

    for bad in [json!({"filter": {"dificulty": "easy"}}), json!({"limit": 0}), json!({"limit": 501})] {
        let mut params = bad.clone();
        params["collection"] = json!("leetcode");
        let e = call(&r, &env, "records.list", params).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidParams, "{bad}");
    }
}

#[tokio::test]
async fn update_sets_and_unsets_but_keeps_required_fields() {
    let (r, env) = setup().await;
    add_two_sum(&r, &env).await;
    let item = call(
        &r,
        &env,
        "records.update",
        json!({"collection": "leetcode", "id": "two-sum",
        "fields": {"url": "https://leetcode.com/problems/two-sum", "difficulty": null}}),
    )
    .await
    .unwrap();
    assert_eq!(item["url"], "https://leetcode.com/problems/two-sum");
    assert_eq!(item["difficulty"], Value::Null);
    assert!(!file(&env, "items/leetcode/two-sum.toml").unwrap().contains("difficulty"));
    let ev = env.backend.events().pop().unwrap();
    assert_eq!(
        (ev.topic.as_str(), ev.payload["changed"].clone()),
        ("records.item.updated", json!(["difficulty", "url"]))
    );

    let e =
        call(&r, &env, "records.update", json!({"collection": "leetcode", "id": "two-sum", "fields": {"title": null}}))
            .await
            .unwrap_err();
    assert!(e.message.contains("'title' is required"));
    let e = call(&r, &env, "records.update", json!({"collection": "leetcode", "id": "ghost", "fields": {}}))
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
}

#[tokio::test]
async fn complete_marks_done_and_stamps_today_each_time() {
    let (r, env) = setup().await;
    add_two_sum(&r, &env).await;
    let target = json!({"collection": "leetcode", "id": "two-sum"});

    let today = env.clock.now().date_naive().to_string();
    let item = call(&r, &env, "records.complete", target.clone()).await.unwrap();
    assert_eq!((item["status"].clone(), item["last_solved"].clone()), (json!("done"), json!(today)));

    // Solving it again on another day is another completion.
    env.clock.advance(Duration::from_secs(86_400));
    let tomorrow = env.clock.now().date_naive().to_string();
    let item = call(&r, &env, "records.complete", target.clone()).await.unwrap();
    assert_eq!(item["last_solved"], json!(tomorrow));
    let completed = topics(&env).iter().filter(|t| *t == "records.item.completed").count();
    assert_eq!(completed, 2);

    let got = call(&r, &env, "records.get", target).await.unwrap();
    assert_eq!(got, item);
}

#[tokio::test]
async fn complete_stamps_the_local_date_not_the_utc_date() {
    let (r, mut env) = setup().await;
    add_two_sum(&r, &env).await;
    // TestEnv's fake clock lands on an exact UTC midnight, so a timezone west of UTC is
    // still "yesterday" locally — exactly the ADR 0009 bug: stamping the UTC date instead
    // of the user's local date.
    env.ctx.local_tz = shimmer_core::LocalTimezone::parse("America/Los_Angeles").unwrap();
    let utc_today = env.clock.now().date_naive();
    let local_yesterday = utc_today.pred_opt().unwrap();

    let target = json!({"collection": "leetcode", "id": "two-sum"});
    let item = call(&r, &env, "records.complete", target).await.unwrap();
    assert_eq!(item["last_solved"], json!(local_yesterday.to_string()));
    assert_ne!(item["last_solved"], json!(utc_today.to_string()));
}

#[tokio::test]
async fn remove_deletes_the_file() {
    let (r, env) = setup().await;
    add_two_sum(&r, &env).await;
    let target = json!({"collection": "leetcode", "id": "two-sum"});
    assert_eq!(call(&r, &env, "records.remove", target.clone()).await.unwrap(), json!({"removed": true}));
    assert!(file(&env, "items/leetcode/two-sum.toml").is_none());
    assert_eq!(topics(&env).last().unwrap(), "records.item.removed");
    assert_eq!(call(&r, &env, "records.get", target).await.unwrap_err().code, ErrorCode::NotFound);
}

#[tokio::test]
async fn hand_edits_are_read_and_a_broken_file_does_not_hide_the_rest() {
    let (r, env) = setup().await;
    add_two_sum(&r, &env).await;
    env.ctx.store.write("items/leetcode/edited.toml", "title = \"Edited\"\nlast_solved = 2026-09-18\n").unwrap();
    env.ctx.store.write("items/leetcode/broken.toml", "status = \"maybe\"").unwrap();

    let all = call(&r, &env, "records.list", json!({"collection": "leetcode"})).await.unwrap();
    assert_eq!(all["total"], 2);
    assert_eq!(all["items"][0]["last_solved"], "2026-09-18");
    assert!(all["skipped"][0].as_str().unwrap().contains("'broken.toml'"));
}

#[tokio::test]
async fn a_broken_collection_file_fails_only_the_requests_that_use_it() {
    // ADR 0008. Issue #29: one bad file used to fail `records.collections` as a whole, and the
    // CLI asks for it before every command, so every collection became unusable.
    let (_, env) = setup().await;
    env.ctx.store.write("collections/broken.toml", "not [valid toml").unwrap();
    env.ctx.store.write("collections/Bad Name.toml", "[collection]\nid = \"x\"\nlabel = \"x\"\n").unwrap();

    let restarted = Records::default();
    restarted.init(&env.ctx).await.expect("a malformed collection must not fail init");

    let data = call(&restarted, &env, "records.collections", json!({})).await.unwrap();
    let ids: Vec<&str> = data["collections"].as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["leetcode"]);
    let skipped: Vec<&str> = data["skipped"].as_array().unwrap().iter().map(|s| s.as_str().unwrap()).collect();
    assert_eq!(skipped.len(), 2, "{skipped:?}");
    assert!(skipped.iter().any(|s| s.starts_with("collection file 'broken.toml': ")), "{skipped:?}");
    assert!(skipped.iter().any(|s| s.starts_with("collection file 'Bad Name.toml': the file name")), "{skipped:?}");

    // The healthy collection keeps working.
    add_two_sum(&restarted, &env).await;
    let list = call(&restarted, &env, "records.list", json!({"collection": "leetcode"})).await.unwrap();
    assert_eq!(list["total"], 1);

    // A request that uses the broken one still fails, naming the file.
    let e = call(&restarted, &env, "records.list", json!({"collection": "broken"})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams);
    assert!(e.message.contains("'broken.toml'"), "{}", e.message);
}

#[tokio::test]
async fn collections_has_no_skipped_key_when_every_file_is_fine() {
    let (r, env) = setup().await;
    let data = call(&r, &env, "records.collections", json!({})).await.unwrap();
    assert!(data.get("skipped").is_none(), "{data}");
}

#[tokio::test]
async fn a_new_module_instance_sees_everything() {
    // What a daemon restart looks like to the module: fresh state, same files.
    let (r, env) = setup().await;
    add_two_sum(&r, &env).await;
    call(&r, &env, "records.complete", json!({"collection": "leetcode", "id": "two-sum"})).await.unwrap();

    let restarted = Records::default();
    restarted.init(&env.ctx).await.unwrap();
    let got = call(&restarted, &env, "records.get", json!({"collection": "leetcode", "id": "two-sum"})).await.unwrap();
    assert_eq!(got["status"], "done");
}

#[tokio::test]
async fn a_user_defined_collection_needs_no_code() {
    let (r, env) = setup().await;
    env.ctx
        .store
        .write(
            "collections/jobs.toml",
            r#"
            [collection]
            id = "jobs"
            label = "Job applications"
            stamp_on_complete = "applied_on"

            [[field]]
            name = "company"
            type = "string"
            required = true

            [[field]]
            name = "applied_on"
            type = "date"
            "#,
        )
        .unwrap();
    call(&r, &env, "records.add", json!({"collection": "jobs", "id": "acme", "fields": {"company": "Acme"}}))
        .await
        .unwrap();
    let item = call(&r, &env, "records.complete", json!({"collection": "jobs", "id": "acme"})).await.unwrap();
    assert_eq!(item["applied_on"], json!(env.clock.now().date_naive().to_string()));
}

// ---------------------------------------------------------------- ADR 0016: completion lifecycle

/// A job tracker that refuses a second completion, as `job-applications` will.
fn add_jobs_collection(env: &TestEnv) {
    env.ctx
        .store
        .write(
            "collections/jobs.toml",
            r#"
            [collection]
            id = "jobs"
            label = "Job applications"
            stamp_on_complete = "applied_on"
            repeat_complete = "refuse"

            [[field]]
            name = "company"
            type = "string"
            required = true

            [[field]]
            name = "stage"
            type = "enum"
            values = ["oa", "interview"]

            [[field]]
            name = "applied_on"
            type = "date"
            "#,
        )
        .unwrap();
}

#[tokio::test]
async fn reopen_writes_todo_and_its_own_event() {
    let (r, env) = setup().await;
    add_two_sum(&r, &env).await;
    let target = json!({"collection": "leetcode", "id": "two-sum"});
    let solved = call(&r, &env, "records.complete", target.clone()).await.unwrap();

    let item = call(&r, &env, "records.reopen", target.clone()).await.unwrap();
    assert_eq!(item["status"], "todo");
    assert_eq!(item["last_solved"], solved["last_solved"], "the stamp is kept by default");
    assert!(file(&env, "items/leetcode/two-sum.toml").unwrap().contains("status = \"todo\""));
    let ev = env.backend.events().pop().unwrap();
    assert_eq!(ev.topic, "records.item.reopened");
    assert_eq!(
        ev.payload,
        json!({"collection": "leetcode", "id": "two-sum", "title": "Two Sum", "deadlines": {}, "item": item})
    );

    // Reopening again: not done, so conflict, and nothing written.
    let before = topics(&env).len();
    let e = call(&r, &env, "records.reopen", target).await.unwrap_err();
    assert_eq!((e.code, e.message.as_str()), (ErrorCode::Conflict, "'two-sum' in 'leetcode' is not done"));
    assert_eq!(topics(&env).len(), before);
}

#[tokio::test]
async fn reopen_can_clear_the_stamp() {
    let (r, env) = setup().await;
    add_two_sum(&r, &env).await;
    call(&r, &env, "records.complete", json!({"collection": "leetcode", "id": "two-sum"})).await.unwrap();
    let item =
        call(&r, &env, "records.reopen", json!({"collection": "leetcode", "id": "two-sum", "clear_stamp": true}))
            .await
            .unwrap();
    assert_eq!(item["last_solved"], Value::Null);
    assert!(!file(&env, "items/leetcode/two-sum.toml").unwrap().contains("last_solved"));
}

#[tokio::test]
async fn complete_with_fields_is_one_write_and_one_event() {
    let (r, env) = setup().await;
    add_jobs_collection(&env);
    call(&r, &env, "records.add", json!({"collection": "jobs", "id": "acme", "fields": {"company": "Acme"}}))
        .await
        .unwrap();
    let before = topics(&env).len();

    let item = call(
        &r,
        &env,
        "records.complete",
        json!({"collection": "jobs", "id": "acme", "fields": {"stage": "oa", "applied_on": "2026-10-03"}}),
    )
    .await
    .unwrap();
    assert_eq!(
        (item["status"].clone(), item["stage"].clone(), item["applied_on"].clone()),
        (json!("done"), json!("oa"), json!("2026-10-03")),
        "fields set and the stamp back-dated"
    );
    assert_eq!(topics(&env)[before..], ["records.item.completed"], "one event, no separate update");

    // Bad fields: nothing written.
    call(&r, &env, "records.reopen", json!({"collection": "jobs", "id": "acme"})).await.unwrap();
    let before = topics(&env).len();
    let e =
        call(&r, &env, "records.complete", json!({"collection": "jobs", "id": "acme", "fields": {"stage": "offer"}}))
            .await
            .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams);
    assert_eq!(topics(&env).len(), before);
}

#[tokio::test]
async fn refuse_makes_a_second_completion_a_conflict_until_reopened() {
    let (r, env) = setup().await;
    add_jobs_collection(&env);
    let target = json!({"collection": "jobs", "id": "acme"});
    call(&r, &env, "records.add", json!({"collection": "jobs", "id": "acme", "fields": {"company": "Acme"}}))
        .await
        .unwrap();
    let first = call(&r, &env, "records.complete", target.clone()).await.unwrap();
    let file_after_first = file(&env, "items/jobs/acme.toml");
    let before = topics(&env).len();

    env.clock.advance(Duration::from_secs(86_400));
    let e = call(&r, &env, "records.complete", target.clone()).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict);
    assert_eq!(
        e.message,
        format!("'acme' in 'jobs' was already completed on {}", first["applied_on"].as_str().unwrap())
    );
    assert_eq!((topics(&env).len(), file(&env, "items/jobs/acme.toml")), (before, file_after_first));

    call(&r, &env, "records.reopen", target.clone()).await.unwrap();
    let again = call(&r, &env, "records.complete", target).await.unwrap();
    assert_ne!(again["applied_on"], first["applied_on"], "reopened on purpose, so it stamps the new day");
}

#[tokio::test]
async fn collections_always_say_what_a_repeat_completion_does() {
    let (r, env) = setup().await;
    add_jobs_collection(&env);
    let data = call(&r, &env, "records.collections", json!({})).await.unwrap();
    let repeat = |id: &str| {
        data["collections"].as_array().unwrap().iter().find(|c| c["id"] == id).unwrap()["repeat_complete"].clone()
    };
    assert_eq!(repeat("leetcode"), "restamp", "the default is filled in");
    assert_eq!(repeat("jobs"), "refuse");
}

// ---------------------------------------------------------------- ADR 0017: integration metadata

/// What a job-applications template will look like: a title, a unique URL, deadlines.
fn add_postings_collection(env: &TestEnv) {
    env.ctx
        .store
        .write(
            "collections/postings.toml",
            r#"
            [collection]
            id = "postings"
            label = "Postings"
            title = "{company}: {position}"
            stamp_on_complete = "applied_on"

            [[field]]
            name = "company"
            type = "string"
            required = true

            [[field]]
            name = "position"
            type = "string"

            [[field]]
            name = "url"
            type = "string"
            role = "url"
            unique = true

            [[field]]
            name = "stage"
            type = "enum"
            values = ["oa", "interview"]

            [[field]]
            name = "oa_deadline"
            type = "date"
            role = "deadline"

            [[field]]
            name = "applied_on"
            type = "date"

            [extra.calendar]
            lead_days = [3, 1]
            "#,
        )
        .unwrap();
}

async fn add_posting(r: &Records, env: &TestEnv, id: &str, fields: Value) -> Result<Value> {
    call(r, env, "records.add", json!({"collection": "postings", "id": id, "fields": fields})).await
}

#[tokio::test]
async fn events_carry_the_title_and_the_deadlines() {
    let (r, env) = setup().await;
    add_postings_collection(&env);
    add_posting(
        &r,
        &env,
        "amazon",
        json!({"company": "Amazon", "position": "SDE Intern", "oa_deadline": "2026-10-20", "applied_on": "2026-10-04"}),
    )
    .await
    .unwrap();
    let ev = env.backend.events().pop().unwrap();
    assert_eq!(ev.payload["title"], "Amazon: SDE Intern");
    assert_eq!(ev.payload["deadlines"], json!({"oa_deadline": "2026-10-20"}), "applied_on has no role: history");

    // Every item event says so, not just created.
    for (op, extra) in [("records.update", json!({"fields": {"stage": "oa"}})), ("records.complete", json!({}))] {
        let mut params = json!({"collection": "postings", "id": "amazon"});
        params.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        call(&r, &env, op, params).await.unwrap();
        let ev = env.backend.events().pop().unwrap();
        assert_eq!(
            (ev.payload["title"].clone(), ev.payload["deadlines"]["oa_deadline"].clone()),
            (json!("Amazon: SDE Intern"), json!("2026-10-20")),
            "{op}"
        );
    }
}

#[tokio::test]
async fn a_unique_value_can_be_taken_only_once() {
    let (r, env) = setup().await;
    add_postings_collection(&env);
    let url = "https://amazon.jobs/123";
    add_posting(&r, &env, "amazon", json!({"company": "Amazon", "url": url})).await.unwrap();
    let before = topics(&env).len();

    let want = format!("url '{url}' is already used by 'amazon' in 'postings'");
    let e = add_posting(&r, &env, "dup", json!({"company": "Amazon", "url": url})).await.unwrap_err();
    assert_eq!((e.code, e.message.as_str()), (ErrorCode::Conflict, want.as_str()));
    assert!(file(&env, "items/postings/dup.toml").is_none());

    add_posting(&r, &env, "other", json!({"company": "Other"})).await.unwrap();
    let e = call(&r, &env, "records.update", json!({"collection": "postings", "id": "other", "fields": {"url": url}}))
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict, "update");
    let e =
        call(&r, &env, "records.complete", json!({"collection": "postings", "id": "other", "fields": {"url": url}}))
            .await
            .unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict, "complete with fields");
    assert_eq!(topics(&env).len(), before + 1, "only 'other' was added; the refused writes emitted nothing");

    // A record keeps its own value: updating it with the same URL is fine.
    call(&r, &env, "records.update", json!({"collection": "postings", "id": "amazon", "fields": {"url": url}}))
        .await
        .unwrap();
    // Unset and "" never count: many records may have no URL.
    add_posting(&r, &env, "no-url-1", json!({"company": "A", "url": ""})).await.unwrap();
    add_posting(&r, &env, "no-url-2", json!({"company": "B", "url": ""})).await.unwrap();
}

#[tokio::test]
async fn hand_edited_duplicates_stay_readable_and_editable() {
    let (r, env) = setup().await;
    add_postings_collection(&env);
    for id in ["a", "b"] {
        env.ctx.store.write(&format!("items/postings/{id}.toml"), "company = \"X\"\nurl = \"https://same\"\n").unwrap();
    }
    let list = call(&r, &env, "records.list", json!({"collection": "postings"})).await.unwrap();
    assert_eq!(list["total"], 2);
    // Changing another field of a duplicate still works; only taking a used value fails.
    call(&r, &env, "records.update", json!({"collection": "postings", "id": "a", "fields": {"stage": "oa"}}))
        .await
        .unwrap();
}

#[tokio::test]
async fn changed_lists_only_fields_whose_value_changed() {
    let (r, env) = setup().await;
    add_postings_collection(&env);
    add_posting(&r, &env, "amazon", json!({"company": "Amazon", "stage": "oa"})).await.unwrap();
    let changed = |env: &TestEnv| env.backend.events().pop().unwrap().payload["changed"].clone();

    call(
        &r,
        &env,
        "records.update",
        json!({"collection": "postings", "id": "amazon", "fields": {"stage": "interview", "company": "Amazon"}}),
    )
    .await
    .unwrap();
    assert_eq!(changed(&env), json!(["stage"]), "company was sent but didn't change");

    call(&r, &env, "records.update", json!({"collection": "postings", "id": "amazon", "fields": {"stage": null}}))
        .await
        .unwrap();
    assert_eq!(changed(&env), json!(["stage"]), "unsetting is a change");

    call(&r, &env, "records.update", json!({"collection": "postings", "id": "amazon", "fields": {"stage": null}}))
        .await
        .unwrap();
    assert_eq!(changed(&env), json!([]), "nothing changed, but the request is still answered with an event");
}

#[tokio::test]
async fn collections_return_the_integration_keys() {
    let (r, env) = setup().await;
    add_postings_collection(&env);
    let data = call(&r, &env, "records.collections", json!({})).await.unwrap();
    let c = data["collections"].as_array().unwrap().iter().find(|c| c["id"] == "postings").unwrap().clone();
    assert_eq!(c["title"], "{company}: {position}");
    assert_eq!(c["extra"], json!({"calendar": {"lead_days": [3, 1]}}));
    let url = c["fields"].as_array().unwrap().iter().find(|f| f["name"] == "url").unwrap().clone();
    assert_eq!((url["role"].clone(), url["unique"].clone()), (json!("url"), json!(true)));
}

// ---------------------------------------------------------------- ADR 0018: ids

#[tokio::test]
async fn add_without_an_id_names_the_record_from_its_title() {
    let (r, env) = setup().await;
    let add =
        |title: &str| call(&r, &env, "records.add", json!({"collection": "leetcode", "fields": {"title": title}}));
    assert_eq!(add("Two Sum").await.unwrap()["id"], "two-sum");
    assert_eq!(add("Two Sum").await.unwrap()["id"], "two-sum-2", "never clashes");
    assert_eq!(add("Two Sum").await.unwrap()["id"], "two-sum-3");
    assert!(file(&env, "items/leetcode/two-sum-2.toml").is_some());
    let ev = env.backend.events().pop().unwrap();
    assert_eq!((ev.topic.as_str(), ev.payload["id"].clone()), ("records.item.created", json!("two-sum-3")));
}

#[tokio::test]
async fn add_without_a_usable_title_uses_todays_date() {
    let (r, env) = setup().await;
    let today = env.clock.now().date_naive().to_string();
    // A title with nothing Latin in it leaves no id: fall back to the date.
    let item =
        call(&r, &env, "records.add", json!({"collection": "leetcode", "fields": {"title": "شركة"}})).await.unwrap();
    assert_eq!(item["id"], format!("{today}-1"));

    // A collection with no title at all: the date too, counting up.
    env.ctx
        .store
        .write(
            "collections/notes.toml",
            "[collection]\nid = \"notes\"\nlabel = \"Notes\"\n[[field]]\nname = \"text\"\ntype = \"string\"\n",
        )
        .unwrap();
    for n in 1..=2 {
        let item =
            call(&r, &env, "records.add", json!({"collection": "notes", "fields": {"text": "hi"}})).await.unwrap();
        assert_eq!(item["id"], format!("{today}-{n}"));
    }
}

#[tokio::test]
async fn an_explicit_id_is_used_as_given() {
    let (r, env) = setup().await;
    add_two_sum(&r, &env).await;
    let e = call(
        &r,
        &env,
        "records.add",
        json!({"collection": "leetcode", "id": "two-sum", "fields": {"title": "Two Sum"}}),
    )
    .await
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict, "an explicit id is never suffixed");
}

#[tokio::test]
async fn rename_moves_the_record_in_one_step() {
    let (r, env) = setup().await;
    add_two_sum(&r, &env).await;
    call(&r, &env, "records.complete", json!({"collection": "leetcode", "id": "two-sum"})).await.unwrap();

    let item =
        call(&r, &env, "records.rename", json!({"collection": "leetcode", "id": "two-sum", "new_id": "1-two-sum"}))
            .await
            .unwrap();
    assert_eq!((item["id"].clone(), item["status"].clone()), (json!("1-two-sum"), json!("done")), "values untouched");
    assert!(file(&env, "items/leetcode/two-sum.toml").is_none());
    assert!(file(&env, "items/leetcode/1-two-sum.toml").is_some());
    let ev = env.backend.events().pop().unwrap();
    assert_eq!(ev.topic, "records.item.renamed");
    assert_eq!(
        (ev.payload["id"].clone(), ev.payload["new_id"].clone(), ev.payload["title"].clone()),
        (json!("two-sum"), json!("1-two-sum"), json!("Two Sum"))
    );
    assert_eq!(ev.payload["item"], item);
}

#[tokio::test]
async fn rename_refuses_bad_or_taken_ids_and_changes_nothing() {
    let (r, env) = setup().await;
    add_two_sum(&r, &env).await;
    call(&r, &env, "records.add", json!({"collection": "leetcode", "id": "lru", "fields": {"title": "LRU"}}))
        .await
        .unwrap();
    let before = topics(&env).len();
    let rename = |id: &str, new_id: &str| {
        call(&r, &env, "records.rename", json!({"collection": "leetcode", "id": id, "new_id": new_id}))
    };
    for (id, new_id, code) in [
        ("two-sum", "lru", ErrorCode::Conflict),
        ("two-sum", "Bad Id", ErrorCode::InvalidParams),
        ("two-sum", "two-sum", ErrorCode::InvalidParams),
        ("ghost", "anything", ErrorCode::NotFound),
    ] {
        assert_eq!(rename(id, new_id).await.unwrap_err().code, code, "{id} -> {new_id}");
    }
    assert_eq!(topics(&env).len(), before);
    assert!(file(&env, "items/leetcode/two-sum.toml").is_some());
}

// ---------------------------------------------------------------- ADR 0019: queries

#[tokio::test]
async fn list_filters_searches_and_sorts_through_the_op() {
    let (r, env) = setup().await;
    for (id, title, difficulty) in [
        ("two-sum", "Two Sum", "easy"),
        ("two-sum-again", "Two Sum Again", "hard"),
        ("lru-cache", "LRU Cache", "medium"),
    ] {
        call(
            &r,
            &env,
            "records.add",
            json!({"collection": "leetcode", "id": id, "fields": {"title": title, "difficulty": difficulty}}),
        )
        .await
        .unwrap();
    }
    let list = |params: Value| {
        let mut p = json!({"collection": "leetcode"});
        p.as_object_mut().unwrap().extend(params.as_object().unwrap().clone());
        call(&r, &env, "records.list", p)
    };
    let ids = |v: &Value| {
        v["items"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap().to_owned()).collect::<Vec<_>>()
    };

    // By id, truly: not by file path ('-' < '.').
    assert_eq!(ids(&list(json!({})).await.unwrap()), ["lru-cache", "two-sum", "two-sum-again"]);
    // Enum order easy < medium < hard, descending.
    assert_eq!(ids(&list(json!({"sort": ["-difficulty"]})).await.unwrap()), ["two-sum-again", "lru-cache", "two-sum"]);
    assert_eq!(ids(&list(json!({"filter": {"difficulty": {"ne": "hard"}}})).await.unwrap()), ["lru-cache", "two-sum"]);
    let found = list(json!({"search": "two", "limit": 1})).await.unwrap();
    assert_eq!(
        (ids(&found), found["total"].clone()),
        (vec!["two-sum".to_owned()], json!(2)),
        "total counts before paging"
    );

    let e = list(json!({"filter": {"difficulty": {"lt": "hard"}}, "sort": ["nope"]})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams);
}

// ---------------------------------------------------------------- ADR 0020: trash and restore

#[tokio::test]
async fn remove_moves_the_record_to_the_trash_and_says_what_went() {
    let (r, env) = setup().await;
    add_two_sum(&r, &env).await;
    call(&r, &env, "records.complete", json!({"collection": "leetcode", "id": "two-sum"})).await.unwrap();
    let before = file(&env, "items/leetcode/two-sum.toml").unwrap();

    call(&r, &env, "records.remove", json!({"collection": "leetcode", "id": "two-sum"})).await.unwrap();
    assert!(file(&env, "items/leetcode/two-sum.toml").is_none());
    assert_eq!(file(&env, "trash/leetcode/two-sum.toml").unwrap(), before, "trashed exactly as it was");
    let ev = env.backend.events().pop().unwrap();
    assert_eq!(ev.topic, "records.item.removed");
    assert_eq!((ev.payload["title"].clone(), ev.payload["item"]["status"].clone()), (json!("Two Sum"), json!("done")));

    let list = call(&r, &env, "records.list", json!({"collection": "leetcode"})).await.unwrap();
    assert_eq!(list["total"], 0, "the trash isn't part of the collection");
    let trash = call(&r, &env, "records.trash", json!({"collection": "leetcode"})).await.unwrap();
    assert_eq!(trash["items"][0]["id"], "two-sum");
}

#[tokio::test]
async fn restore_brings_it_back_exactly() {
    let (r, env) = setup().await;
    add_two_sum(&r, &env).await;
    call(&r, &env, "records.complete", json!({"collection": "leetcode", "id": "two-sum"})).await.unwrap();
    let target = json!({"collection": "leetcode", "id": "two-sum"});
    let original = call(&r, &env, "records.get", target.clone()).await.unwrap();
    call(&r, &env, "records.remove", target.clone()).await.unwrap();

    let restored = call(&r, &env, "records.restore", target.clone()).await.unwrap();
    assert_eq!(restored, original, "same status, values and stamp");
    assert!(file(&env, "trash/leetcode/two-sum.toml").is_none());
    let ev = env.backend.events().pop().unwrap();
    assert_eq!((ev.topic.as_str(), ev.payload["item"].clone()), ("records.item.restored", original));

    let e = call(&r, &env, "records.restore", target).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound, "nothing left in the trash");
}

#[tokio::test]
async fn restore_refuses_when_the_id_or_a_unique_value_is_taken_again() {
    let (r, env) = setup().await;
    add_two_sum(&r, &env).await;
    let target = json!({"collection": "leetcode", "id": "two-sum"});
    call(&r, &env, "records.remove", target.clone()).await.unwrap();
    add_two_sum(&r, &env).await;
    let before = topics(&env).len();
    let e = call(&r, &env, "records.restore", target).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict);
    assert!(e.message.contains("rename one of them"), "{}", e.message);
    assert_eq!(topics(&env).len(), before);

    add_postings_collection(&env);
    let url = "https://amazon.jobs/1";
    add_posting(&r, &env, "old", json!({"company": "Amazon", "url": url})).await.unwrap();
    call(&r, &env, "records.remove", json!({"collection": "postings", "id": "old"})).await.unwrap();
    add_posting(&r, &env, "new", json!({"company": "Amazon", "url": url})).await.unwrap();
    let e = call(&r, &env, "records.restore", json!({"collection": "postings", "id": "old"})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict);
    assert!(e.message.contains("already used by 'new'"), "{}", e.message);
    assert!(file(&env, "trash/postings/old.toml").is_some(), "still in the trash");
}

// ---------------------------------------------------------------- ADR 0021: collections over time

fn add_education_collection(env: &TestEnv) {
    env.ctx
        .store
        .write(
            "collections/education.toml",
            "[collection]\nid = \"education\"\nlabel = \"Education\"\ncompletable = false\n\n[[field]]\nname = \"school\"\ntype = \"string\"\nrequired = true\n",
        )
        .unwrap();
}

#[tokio::test]
async fn a_reference_collection_has_no_status_and_no_completion() {
    let (r, env) = setup().await;
    add_education_collection(&env);
    let item =
        call(&r, &env, "records.add", json!({"collection": "education", "id": "uni", "fields": {"school": "Uni"}}))
            .await
            .unwrap();
    assert_eq!(item, json!({"id": "uni", "school": "Uni"}), "no status on the wire");
    assert!(!file(&env, "items/education/uni.toml").unwrap().contains("status"), "nor in the file");

    let target = json!({"collection": "education", "id": "uni"});
    for op in ["records.complete", "records.reopen"] {
        let e = call(&r, &env, op, target.clone()).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidParams, "{op}");
        assert!(e.message.contains("has no completion"), "{op}: {}", e.message);
    }
    for params in [json!({"filter": {"status": "todo"}}), json!({"sort": ["status"]})] {
        let mut p = params.clone();
        p["collection"] = json!("education");
        let e = call(&r, &env, "records.list", p).await.unwrap_err();
        assert!(e.message.contains("has no status"), "{params}: {}", e.message);
    }
    let data = call(&r, &env, "records.collections", json!({})).await.unwrap();
    let completable = |id: &str| {
        data["collections"].as_array().unwrap().iter().find(|c| c["id"] == id).unwrap()["completable"].clone()
    };
    assert_eq!((completable("education"), completable("leetcode")), (json!(false), json!(true)));

    // A status line already in a file is kept as written, never rewritten away.
    env.ctx.store.write("items/education/old.toml", "status = \"done\"\nschool = \"Old\"\n").unwrap();
    call(&r, &env, "records.update", json!({"collection": "education", "id": "old", "fields": {"school": "Older"}}))
        .await
        .unwrap();
    assert!(file(&env, "items/education/old.toml").unwrap().contains("status = \"done\""));
}

#[tokio::test]
async fn check_reports_what_does_not_fit_and_changes_nothing() {
    let (r, env) = setup().await;
    add_two_sum(&r, &env).await;
    env.ctx.store.write("items/leetcode/bad.toml", "difficulty = \"trivial\"\ncolour = \"red\"\n").unwrap();
    env.ctx.store.write("items/leetcode/broken.toml", "status = \"maybe\"").unwrap();
    let before = topics(&env).len();
    let report = call(&r, &env, "records.check", json!({"collection": "leetcode"})).await.unwrap();
    assert_eq!(report["checked"], 3);
    let problems = report["problems"].as_array().unwrap();
    assert_eq!(problems.len(), 2, "{report}");
    assert_eq!(problems[0]["id"], "bad");
    assert_eq!(problems[0]["problems"].as_array().unwrap().len(), 3, "missing title, bad difficulty, unknown key");
    assert_eq!(problems[1]["id"], "broken");
    assert_eq!(topics(&env).len(), before, "nothing written");
}

#[tokio::test]
async fn rename_field_moves_values_everywhere_in_one_step() {
    let (r, env) = setup().await;
    add_two_sum(&r, &env).await;
    call(
        &r,
        &env,
        "records.add",
        json!({"collection": "leetcode", "id": "lru", "fields": {"title": "LRU", "difficulty": "medium"}}),
    )
    .await
    .unwrap();
    call(&r, &env, "records.remove", json!({"collection": "leetcode", "id": "lru"})).await.unwrap();

    let out =
        call(&r, &env, "records.rename_field", json!({"collection": "leetcode", "from": "difficulty", "to": "level"}))
            .await
            .unwrap();
    assert_eq!(out["updated"], 2, "the live record and the trashed one");
    let collection = file(&env, "collections/leetcode.toml").unwrap();
    assert!(
        collection.contains("name = \"level\"") && collection.contains("# Test fixture"),
        "comments kept: {collection}"
    );
    let item = call(&r, &env, "records.get", json!({"collection": "leetcode", "id": "two-sum"})).await.unwrap();
    assert_eq!((item["level"].clone(), item.get("difficulty").cloned()), (json!("easy"), None));
    assert!(file(&env, "trash/leetcode/lru.toml").unwrap().contains("level = \"medium\""));
    assert_eq!(env.backend.events().pop().unwrap().topic, "records.field.renamed");

    for (params, code) in [
        (json!({"from": "nope", "to": "x"}), ErrorCode::InvalidParams),
        (json!({"from": "url", "to": "title"}), ErrorCode::Conflict),
        (json!({"from": "url", "to": "status"}), ErrorCode::InvalidParams),
    ] {
        let mut p = params.clone();
        p["collection"] = json!("leetcode");
        assert_eq!(call(&r, &env, "records.rename_field", p).await.unwrap_err().code, code, "{params}");
    }
}

#[tokio::test]
async fn rename_collection_moves_everything() {
    let (r, env) = setup().await;
    add_two_sum(&r, &env).await;
    let out =
        call(&r, &env, "records.rename_collection", json!({"id": "leetcode", "new_id": "problems"})).await.unwrap();
    assert_eq!(out["id"], "problems");
    assert!(file(&env, "collections/leetcode.toml").is_none());
    assert!(file(&env, "collections/problems.toml").unwrap().contains("id = \"problems\""));
    let item = call(&r, &env, "records.get", json!({"collection": "problems", "id": "two-sum"})).await.unwrap();
    assert_eq!(item["title"], "Two Sum");
    assert_eq!(env.backend.events().pop().unwrap().topic, "records.collection.renamed");
    add_education_collection(&env);
    let e = call(&r, &env, "records.rename_collection", json!({"id": "problems", "new_id": "education"}))
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict);
}

#[tokio::test]
async fn remove_collection_asks_first_then_restore_brings_it_all_back() {
    let (r, env) = setup().await;
    add_two_sum(&r, &env).await;
    let before = topics(&env).len();

    let e = call(&r, &env, "records.remove_collection", json!({"id": "leetcode"})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::ConfirmationRequired);
    assert_eq!(e.detail, Some(json!({"records": 1})));
    let stale = call(&r, &env, "records.remove_collection", json!({"id": "leetcode", "confirm": {"records": 5}}))
        .await
        .unwrap_err();
    assert_eq!(stale.code, ErrorCode::ConfirmationRequired, "a count that doesn't match asks again");
    assert_eq!(topics(&env).len(), before, "nothing removed without the right confirmation");

    let out = call(&r, &env, "records.remove_collection", json!({"id": "leetcode", "confirm": {"records": 1}}))
        .await
        .unwrap();
    assert_eq!(out, json!({"removed": true, "records": 1}));
    assert!(file(&env, "collections/leetcode.toml").is_none() && file(&env, "items/leetcode/two-sum.toml").is_none());
    assert!(file(&env, "removed-collections/leetcode/items/two-sum.toml").is_some());
    assert_eq!(env.backend.events().pop().unwrap().topic, "records.collection.removed");

    call(&r, &env, "records.restore_collection", json!({"id": "leetcode"})).await.unwrap();
    let item = call(&r, &env, "records.get", json!({"collection": "leetcode", "id": "two-sum"})).await.unwrap();
    assert_eq!(item["title"], "Two Sum");
    assert_eq!(env.backend.events().pop().unwrap().topic, "records.collection.restored");
    let e = call(&r, &env, "records.restore_collection", json!({"id": "leetcode"})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
}

// ---------------------------------------------------------------- ADR 0022: templates

#[tokio::test]
async fn every_template_is_listed_and_creates_a_valid_collection() {
    let (r, env) = setup().await;
    let data = call(&r, &env, "records.templates", json!({})).await.unwrap();
    let ids: Vec<String> =
        data["templates"].as_array().unwrap().iter().map(|t| t["id"].as_str().unwrap().to_owned()).collect();
    assert_eq!(
        ids,
        [
            "addresses",
            "certifications",
            "charity",
            "documents",
            "education",
            "employment",
            "interview-questions",
            "interviews",
            "job-applications",
            "leetcode",
            "networking-events",
            "offers",
            "outreach",
            "projects",
            "stories",
            "subscriptions"
        ]
    );

    for template in &ids {
        let id = format!("my-{template}");
        let c = call(&r, &env, "records.create_collection", json!({"id": id, "template": template})).await.unwrap();
        assert_eq!(c["id"], id.as_str(), "{template}");
        let text = file(&env, &format!("collections/{id}.toml")).unwrap();
        assert!(text.contains(&format!("id = \"{id}\"")), "{template}");
        assert!(text.lines().next().unwrap().starts_with('#'), "{template}: the comments come along");
        let ev = env.backend.events().pop().unwrap();
        assert_eq!(
            (ev.topic.as_str(), ev.payload["template"].as_str()),
            ("records.collection.created", Some(template.as_str()))
        );
    }
    // And each one is usable right away.
    let check = call(&r, &env, "records.check", json!({"collection": "my-job-applications"})).await.unwrap();
    assert_eq!(check["checked"], 0);
}

#[tokio::test]
async fn create_collection_never_overwrites_and_says_what_exists() {
    let (r, env) = setup().await;
    let before = topics(&env).len();
    for (params, code, says) in [
        (
            json!({"id": "leetcode", "template": "job-applications"}),
            ErrorCode::Conflict,
            "already a collection 'leetcode'",
        ),
        (
            json!({"id": "jobs", "template": "jobs"}),
            ErrorCode::NotFound,
            "there are: addresses, certifications, charity, documents, education, employment, interview-questions, interviews, job-applications, leetcode, networking-events, offers, outreach, projects, stories, subscriptions",
        ),
        (json!({"id": "Jobs!", "template": "job-applications"}), ErrorCode::InvalidParams, "not a valid collection id"),
        (json!({"id": "jobs", "template": "job-applications", "label": " "}), ErrorCode::InvalidParams, "label"),
    ] {
        let e = call(&r, &env, "records.create_collection", params.clone()).await.unwrap_err();
        assert_eq!(e.code, code, "{params}");
        assert!(e.message.contains(says), "{params}: {}", e.message);
    }
    assert_eq!(topics(&env).len(), before);
    assert!(file(&env, "collections/leetcode.toml").unwrap().contains("label = \"LeetCode\""), "untouched");

    let c = call(
        &r,
        &env,
        "records.create_collection",
        json!({"id": "jobs", "template": "job-applications", "label": "Internships 2027"}),
    )
    .await
    .unwrap();
    assert_eq!(c["label"], "Internships 2027");
}

#[tokio::test]
async fn a_job_hunt_with_the_templates() {
    let (r, env) = setup().await;
    for (id, template) in [("jobs", "job-applications"), ("rounds", "interviews")] {
        call(&r, &env, "records.create_collection", json!({"id": id, "template": template})).await.unwrap();
    }
    // Saved, with a generated id, a unique URL and a deadline the calendar will see.
    let job = call(
        &r,
        &env,
        "records.add",
        json!({"collection": "jobs", "fields": {
        "position": "SDE Intern", "company": "Amazon", "url": "https://amazon.jobs/1", "oa_deadline": "2026-10-20"}}),
    )
    .await
    .unwrap();
    assert_eq!((job["id"].clone(), job["status"].clone()), (json!("amazon-sde-intern"), json!("todo")));
    let ev = env.backend.events().pop().unwrap();
    assert_eq!(
        (ev.payload["title"].clone(), ev.payload["deadlines"].clone()),
        (json!("Amazon: SDE Intern"), json!({"oa_deadline": "2026-10-20"}))
    );
    let dup = call(
        &r,
        &env,
        "records.add",
        json!({"collection": "jobs", "fields": {"position": "X", "company": "Amazon", "url": "https://amazon.jobs/1"}}),
    )
    .await
    .unwrap_err();
    assert_eq!(dup.code, ErrorCode::Conflict);

    // Applied once; a second "applied" is refused.
    let target = json!({"collection": "jobs", "id": "amazon-sde-intern"});
    let applied = call(&r, &env, "records.complete", target.clone()).await.unwrap();
    assert_eq!(applied["status"], "done");
    assert_eq!(call(&r, &env, "records.complete", target).await.unwrap_err().code, ErrorCode::Conflict);
    call(
        &r,
        &env,
        "records.update",
        json!({"collection": "jobs", "id": "amazon-sde-intern", "fields": {"stage": "interviewing"}}),
    )
    .await
    .unwrap();

    // A round, done and passed in one step.
    let round = call(&r, &env, "records.add", json!({"collection": "rounds", "fields": {"application": "amazon-sde-intern", "kind": "technical", "round": 1}}))
        .await
        .unwrap();
    assert_eq!(round["id"], "amazon-sde-intern-technical");
    let done = call(
        &r,
        &env,
        "records.complete",
        json!({"collection": "rounds", "id": "amazon-sde-intern-technical", "fields": {"outcome": "passed"}}),
    )
    .await
    .unwrap();
    assert_eq!((done["status"].clone(), done["outcome"].clone()), (json!("done"), json!("passed")));

    let active =
        call(&r, &env, "records.list", json!({"collection": "jobs", "filter": {"stage": {"gte": "interviewing"}}}))
            .await
            .unwrap();
    assert_eq!(active["total"], 1);
}

#[tokio::test]
async fn practising_a_question_counts_every_time() {
    let (r, env) = setup().await;
    call(&r, &env, "records.create_collection", json!({"id": "questions", "template": "interview-questions"}))
        .await
        .unwrap();
    let q = call(
        &r,
        &env,
        "records.add",
        json!({"collection": "questions", "fields": {
        "question": "Tell me about a conflict", "kind": "behavioral", "confidence": "shaky"}}),
    )
    .await
    .unwrap();
    assert_eq!(q["id"], "tell-me-about-a-conflict");
    let target = json!({"collection": "questions", "id": "tell-me-about-a-conflict"});
    let mut params = target.clone();
    params["fields"] = json!({"confidence": "okay", "next_review": "2026-10-12"});
    let first = call(&r, &env, "records.complete", params).await.unwrap();
    assert_eq!((first["confidence"].clone(), first["next_review"].clone()), (json!("okay"), json!("2026-10-12")));
    let ev = env.backend.events().pop().unwrap();
    assert_eq!(ev.payload["deadlines"], json!({"next_review": "2026-10-12"}), "the calendar can remind you");

    env.clock.advance(Duration::from_secs(86_400));
    let again = call(&r, &env, "records.complete", target).await.unwrap();
    assert_ne!(again["last_practiced"], first["last_practiced"], "practising again is another practice");
}

#[tokio::test]
async fn background_history_is_a_reference_list() {
    let (r, env) = setup().await;
    call(&r, &env, "records.create_collection", json!({"id": "jobs-held", "template": "employment"})).await.unwrap();
    let job = call(&r, &env, "records.add", json!({"collection": "jobs-held", "fields": {
        "company": "Acme", "job_title": "SWE Intern", "start_on": "2025-05-12", "end_on": "2025-08-15", "may_contact": true}}))
        .await
        .unwrap();
    assert_eq!(job["id"], "acme-swe-intern");
    assert!(job.get("status").is_none(), "history has no status");
    let e = call(&r, &env, "records.complete", json!({"collection": "jobs-held", "id": "acme-swe-intern"}))
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams);
    call(&r, &env, "records.add", json!({"collection": "jobs-held", "fields": {"company": "Initech", "job_title": "Engineer", "start_on": "2025-09-01"}}))
        .await
        .unwrap();
    let current =
        call(&r, &env, "records.list", json!({"collection": "jobs-held", "filter": {"end_on": {"set": false}}}))
            .await
            .unwrap();
    assert_eq!(current["items"][0]["id"], "initech-engineer", "no end date = your current job");
}

#[tokio::test]
async fn a_planned_gift_is_given_once() {
    let (r, env) = setup().await;
    call(&r, &env, "records.create_collection", json!({"id": "charity", "template": "charity"})).await.unwrap();
    let gift = call(
        &r,
        &env,
        "records.add",
        json!({"collection": "charity", "fields": {
        "recipient": "Islamic Relief", "kind": "zakat", "amount": 400, "currency": "CAD", "due_on": "2027-03-01"}}),
    )
    .await
    .unwrap();
    assert_eq!((gift["id"].clone(), gift["status"].clone()), (json!("islamic-relief-zakat"), json!("todo")));
    assert_eq!(env.backend.events().pop().unwrap().payload["deadlines"], json!({"due_on": "2027-03-01"}));
    let target = json!({"collection": "charity", "id": "islamic-relief-zakat", "fields": {"method": "online"}});
    let given = call(&r, &env, "records.complete", target.clone()).await.unwrap();
    assert_eq!(given["status"], "done");
    assert_eq!(call(&r, &env, "records.complete", target).await.unwrap_err().code, ErrorCode::Conflict, "given once");
    // Any charity, not only Islamic kinds of giving.
    let general = call(
        &r,
        &env,
        "records.add",
        json!({"collection": "charity", "fields": {"recipient": "Food bank", "kind": "general"}}),
    )
    .await
    .unwrap();
    assert_eq!(general["id"], "food-bank-general");
}

#[tokio::test]
async fn a_question_points_at_the_story_that_answers_it() {
    let (r, env) = setup().await;
    for (id, template) in [("stories", "stories"), ("questions", "interview-questions")] {
        call(&r, &env, "records.create_collection", json!({"id": id, "template": template})).await.unwrap();
    }
    let story = call(
        &r,
        &env,
        "records.add",
        json!({"collection": "stories", "fields": {
        "name": "Billing migration crunch", "primary_theme": "ownership", "told_to": "Amazon"}}),
    )
    .await
    .unwrap();
    assert_eq!(story["id"], "billing-migration-crunch");
    for q in ["Tell me about a time you owned something", "Tell me about a tight deadline"] {
        call(
            &r,
            &env,
            "records.add",
            json!({"collection": "questions", "fields": {
            "question": q, "kind": "behavioral", "story": "billing-migration-crunch"}}),
        )
        .await
        .unwrap();
    }
    let answered = call(
        &r,
        &env,
        "records.list",
        json!({"collection": "questions", "filter": {"story": "billing-migration-crunch"}}),
    )
    .await
    .unwrap();
    assert_eq!(answered["total"], 2, "one story answers many questions");
    let told =
        call(&r, &env, "records.list", json!({"collection": "stories", "filter": {"told_to": {"contains": "amazon"}}}))
            .await
            .unwrap();
    assert_eq!(told["total"], 1);
}

#[tokio::test]
async fn a_project_is_shipped_once_and_sorts_by_stars() {
    let (r, env) = setup().await;
    call(&r, &env, "records.create_collection", json!({"id": "projects", "template": "projects"})).await.unwrap();
    for (name, stars, url) in
        [("Shimmer", 40, "https://github.com/x/shimmer"), ("Todo app", 3, "https://github.com/x/todo")]
    {
        call(
            &r,
            &env,
            "records.add",
            json!({"collection": "projects", "fields": {
            "name": name, "stars": stars, "repo_url": url, "stage": "building", "on_resume": true}}),
        )
        .await
        .unwrap();
    }
    let dup = call(
        &r,
        &env,
        "records.add",
        json!({"collection": "projects", "fields": {"name": "Copy", "repo_url": "https://github.com/x/shimmer"}}),
    )
    .await
    .unwrap_err();
    assert_eq!(dup.code, ErrorCode::Conflict, "the same repo can't be added twice");
    let target = json!({"collection": "projects", "id": "shimmer", "fields": {"stage": "launched", "users": 200}});
    let shipped = call(&r, &env, "records.complete", target.clone()).await.unwrap();
    assert_eq!((shipped["status"].clone(), shipped["users"].clone()), (json!("done"), json!(200)));
    assert_eq!(call(&r, &env, "records.complete", target).await.unwrap_err().code, ErrorCode::Conflict);
    let resume = call(
        &r,
        &env,
        "records.list",
        json!({"collection": "projects", "filter": {"on_resume": true}, "sort": ["-stars"]}),
    )
    .await
    .unwrap();
    let ids: Vec<&str> = resume["items"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["shimmer", "todo-app"]);
}

#[tokio::test]
async fn offers_compare_by_total_comp_and_are_decided_once() {
    let (r, env) = setup().await;
    call(&r, &env, "records.create_collection", json!({"id": "offers", "template": "offers"})).await.unwrap();
    for (company, total, deadline) in [("Amazon", 95_000, "2026-11-01"), ("Google", 110_000, "2026-10-20")] {
        call(
            &r,
            &env,
            "records.add",
            json!({"collection": "offers", "fields": {
            "company": company, "position": "SWE", "total_comp": total, "currency": "CAD",
            "decision_deadline": deadline, "decision": "considering"}}),
        )
        .await
        .unwrap();
    }
    let ev = env.backend.events().pop().unwrap();
    assert_eq!(ev.payload["deadlines"], json!({"decision_deadline": "2026-10-20"}));
    let best = call(&r, &env, "records.list", json!({"collection": "offers", "sort": ["-total_comp"]})).await.unwrap();
    assert_eq!(best["items"][0]["id"], "google-swe");
    let target = json!({"collection": "offers", "id": "google-swe", "fields": {"decision": "accepted"}});
    let decided = call(&r, &env, "records.complete", target.clone()).await.unwrap();
    assert_eq!((decided["status"].clone(), decided["decision"].clone()), (json!("done"), json!("accepted")));
    assert_eq!(call(&r, &env, "records.complete", target).await.unwrap_err().code, ErrorCode::Conflict);
    let open = call(
        &r,
        &env,
        "records.list",
        json!({"collection": "offers", "filter": {"status": "todo"}, "sort": ["decision_deadline"]}),
    )
    .await
    .unwrap();
    assert_eq!(open["items"][0]["id"], "amazon-swe", "still to answer");
}

#[tokio::test]
async fn a_certification_is_earned_then_renewed() {
    let (r, env) = setup().await;
    call(&r, &env, "records.create_collection", json!({"id": "certs", "template": "certifications"})).await.unwrap();
    call(&r, &env, "records.add", json!({"collection": "certs", "fields": {
        "name": "AWS Solutions Architect - Associate", "provider": "AWS", "stage": "studying", "exam_at": "2026-12-10 09:00"}}))
        .await
        .unwrap();
    let id = "aws-solutions-architect-associate";
    let mut params =
        json!({"collection": "certs", "id": id, "fields": {"stage": "passed", "expires_on": "2029-12-10"}});
    let earned = call(&r, &env, "records.complete", params.clone()).await.unwrap();
    assert_eq!(earned["status"], "done");
    assert_eq!(
        env.backend.events().pop().unwrap().payload["deadlines"],
        json!({"exam_at": "2026-12-10T09:00:00Z", "expires_on": "2029-12-10"}),
        "a local time is stored as UTC (the test timezone is UTC)"
    );
    // Renewing three years on is another completion, with a new expiry.
    env.clock.advance(Duration::from_secs(3 * 365 * 86_400));
    params["fields"] = json!({"expires_on": "2032-12-10"});
    let renewed = call(&r, &env, "records.complete", params).await.unwrap();
    assert_ne!(renewed["earned_on"], earned["earned_on"]);
    assert_eq!(renewed["expires_on"], "2032-12-10");
}

#[tokio::test]
async fn an_event_is_attended_then_followed_up() {
    let (r, env) = setup().await;
    call(&r, &env, "records.create_collection", json!({"id": "events", "template": "networking-events"}))
        .await
        .unwrap();
    call(
        &r,
        &env,
        "records.add",
        json!({"collection": "events", "fields": {
        "name": "Fall Career Fair 2026", "kind": "career-fair", "starts_at": "2026-10-22 18:00",
        "companies": ["Shopify", "RBC"], "registration_deadline": "2026-10-15", "rsvp": "registered"}}),
    )
    .await
    .unwrap();
    assert_eq!(
        env.backend.events().pop().unwrap().payload["deadlines"],
        json!({"starts_at": "2026-10-22T18:00:00Z", "registration_deadline": "2026-10-15"})
    );
    let shopify = json!({"collection": "events", "filter": {"companies": {"has": "shopify"}}});
    assert_eq!(call(&r, &env, "records.list", shopify).await.unwrap()["total"], 1);
    let attended = call(
        &r,
        &env,
        "records.complete",
        json!({"collection": "events", "id": "fall-career-fair-2026",
        "fields": {"contacts_made": 4, "follow_up_on": "2026-10-24", "worth_it": "great"}}),
    )
    .await
    .unwrap();
    assert_eq!((attended["status"].clone(), attended["contacts_made"].clone()), (json!("done"), json!(4)));
    let owed = call(&r, &env, "records.list", json!({"collection": "events", "filter": {"followed_up": {"ne": true}}}))
        .await
        .unwrap();
    assert_eq!(owed["total"], 1, "followed_up not set yet");
}

#[tokio::test]
async fn every_follow_up_counts_and_people_link_to_the_event() {
    let (r, env) = setup().await;
    call(&r, &env, "records.create_collection", json!({"id": "outreach", "template": "outreach"})).await.unwrap();
    for name in ["Priya Shah", "Ben Lee"] {
        call(
            &r,
            &env,
            "records.add",
            json!({"collection": "outreach", "fields": {
            "name": name, "goal": "referral", "event": "fall-career-fair-2026"}}),
        )
        .await
        .unwrap();
    }
    let target = json!({"collection": "outreach", "id": "priya-shah", "fields": {"follow_up_on": "2026-10-29"}});
    let first = call(&r, &env, "records.complete", target.clone()).await.unwrap();
    assert_eq!(env.backend.events().pop().unwrap().payload["deadlines"], json!({"follow_up_on": "2026-10-29"}));
    env.clock.advance(Duration::from_secs(7 * 86_400));
    let again = call(&r, &env, "records.complete", target).await.unwrap();
    assert_ne!(again["last_contacted_on"], first["last_contacted_on"], "a follow-up counts again");
    let met =
        call(&r, &env, "records.list", json!({"collection": "outreach", "filter": {"event": "fall-career-fair-2026"}}))
            .await
            .unwrap();
    assert_eq!(met["total"], 2);
}

#[tokio::test]
async fn subscriptions_are_on_or_off_and_warn_before_charges() {
    let (r, env) = setup().await;
    call(&r, &env, "records.create_collection", json!({"id": "subs", "template": "subscriptions"})).await.unwrap();
    let sub = call(&r, &env, "records.add", json!({"collection": "subs", "fields": {
        "name": "Cloud storage", "state": "trial", "cost": 13, "trial_ends_on": "2026-10-12", "renews_on": "2026-11-12"}}))
        .await
        .unwrap();
    assert!(sub.get("status").is_none(), "on or off, not finished");
    assert_eq!(
        env.backend.events().pop().unwrap().payload["deadlines"],
        json!({"renews_on": "2026-11-12", "trial_ends_on": "2026-10-12"})
    );
    for kind in ["music", "gambling"] {
        let e = call(&r, &env, "records.add", json!({"collection": "subs", "fields": {"name": "x", "category": kind}}))
            .await
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidParams, "{kind}");
    }
}

#[tokio::test]
async fn documents_warn_before_they_expire() {
    let (r, env) = setup().await;
    call(&r, &env, "records.create_collection", json!({"id": "documents", "template": "documents"})).await.unwrap();
    for (name, expires) in [("Passport", "2031-03-01"), ("Study permit", "2027-01-15")] {
        call(
            &r,
            &env,
            "records.add",
            json!({"collection": "documents", "fields": {
            "name": name, "state": "valid", "expires_on": expires}}),
        )
        .await
        .unwrap();
    }
    assert_eq!(env.backend.events().pop().unwrap().payload["deadlines"], json!({"expires_on": "2027-01-15"}));
    let soon = call(
        &r,
        &env,
        "records.list",
        json!({"collection": "documents",
        "filter": {"expires_on": {"lte": "2027-06-01"}}, "sort": ["expires_on"]}),
    )
    .await
    .unwrap();
    assert_eq!((soon["total"].clone(), soon["items"][0]["id"].clone()), (json!(1), json!("study-permit")));
}

fn add_rounds_collection(env: &TestEnv) {
    env.ctx
        .store
        .write(
            "collections/rounds.toml",
            r#"
            [collection]
            id = "rounds"
            label = "Rounds"
            stamp_on_complete = "done_at"

            [[field]]
            name = "at"
            type = "datetime"
            role = "deadline"

            [[field]]
            name = "done_at"
            type = "datetime"
            "#,
        )
        .unwrap();
}

#[tokio::test]
async fn datetimes_are_read_in_the_local_timezone_and_stored_as_utc() {
    // ADR 0024 §1.
    let (r, mut env) = setup().await;
    add_rounds_collection(&env);
    env.ctx.local_tz = shimmer_core::LocalTimezone::parse("America/Los_Angeles").unwrap();
    let add = |id: &str, at: &str| json!({"collection": "rounds", "id": id, "fields": {"at": at}});

    let item = call(&r, &env, "records.add", add("oa", "2026-10-20 23:59")).await.unwrap();
    assert_eq!(item["at"], "2026-10-21T06:59:00Z", "PDT is UTC-7");
    assert!(file(&env, "items/rounds/oa.toml").unwrap().contains("at = \"2026-10-21T06:59:00Z\""));
    let ev = env.backend.events().pop().unwrap();
    assert_eq!(ev.payload["deadlines"], json!({"at": "2026-10-21T06:59:00Z"}));

    call(&r, &env, "records.add", add("screen", "2026-10-20")).await.unwrap();
    call(&r, &env, "records.add", add("onsite", "2026-10-20T09:00:00-04:00")).await.unwrap();
    let e = call(&r, &env, "records.add", add("bad", "tomorrow at 5")).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams);
    let e = call(&r, &env, "records.add", add("gap", "2026-03-08 02:30")).await.unwrap_err();
    assert!(e.message.contains("doesn't exist"), "{}", e.message);

    // A plain date is the start of its local day; the sort uses instants, not text.
    let list = call(&r, &env, "records.list", json!({"collection": "rounds", "sort": ["at"]})).await.unwrap();
    let ids: Vec<_> = list["items"].as_array().unwrap().iter().map(|i| i["id"].clone()).collect();
    assert_eq!(ids, [json!("screen"), json!("onsite"), json!("oa")]);
    let filter = json!({"collection": "rounds", "filter": {"at": {"gt": "2026-10-20 12:00"}}});
    let list = call(&r, &env, "records.list", filter).await.unwrap();
    assert_eq!(list["items"].as_array().unwrap().len(), 1, "only the OA is after local noon");

    let item = call(&r, &env, "records.complete", json!({"collection": "rounds", "id": "oa"})).await.unwrap();
    assert_eq!(item["done_at"], json!(env.clock.now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)));
}

fn add_projects_collection(env: &TestEnv) {
    env.ctx
        .store
        .write(
            "collections/projects.toml",
            r#"
            [collection]
            id = "projects"
            label = "Projects"
            title = "{name}: {stack}"

            [[field]]
            name = "name"
            type = "string"
            required = true

            [[field]]
            name = "stack"
            type = "list"
            of = "string"
            "#,
        )
        .unwrap();
}

#[tokio::test]
async fn list_fields_are_arrays_changed_whole_or_by_item() {
    // ADR 0024 §2.
    let (r, env) = setup().await;
    add_projects_collection(&env);
    let add = json!({"collection": "projects", "id": "shimmer", "fields": {"name": "Shimmer", "stack": ["Rust", " rust ", "TOML"]}});
    let item = call(&r, &env, "records.add", add).await.unwrap();
    assert_eq!(item["stack"], json!(["Rust", "TOML"]), "tidied: trimmed, repeats dropped");
    assert!(file(&env, "items/projects/shimmer.toml").unwrap().contains("stack = [\"Rust\", \"TOML\"]"));
    assert_eq!(env.backend.events().pop().unwrap().payload["title"], "Shimmer: Rust, TOML");

    let target = |fields: Value| json!({"collection": "projects", "id": "shimmer", "fields": fields});
    let item = call(&r, &env, "records.update", target(json!({"stack": {"add": ["SQLite"], "remove": ["toml"]}})))
        .await
        .unwrap();
    assert_eq!(item["stack"], json!(["Rust", "SQLite"]));
    assert_eq!(env.backend.events().pop().unwrap().payload["changed"], json!(["stack"]));

    let item = call(&r, &env, "records.update", target(json!({"stack": []}))).await.unwrap();
    assert_eq!(item["stack"], Value::Null, "an empty list is unset");
    assert!(!file(&env, "items/projects/shimmer.toml").unwrap().contains("stack"));

    let e = call(&r, &env, "records.update", target(json!({"stack": "Rust"}))).await.unwrap_err();
    assert!(e.message.contains("must be a list"), "{}", e.message);
    let e = call(&r, &env, "records.update", target(json!({"stack": {"put": ["x"]}}))).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams);

    call(&r, &env, "records.update", target(json!({"stack": ["Go"]}))).await.unwrap();
    let list = json!({"collection": "projects", "filter": {"stack": {"has": "go"}}});
    let out = call(&r, &env, "records.list", list).await.unwrap();
    assert_eq!(out["total"], 1);
    assert_eq!(out["titles"], json!({"shimmer": "Shimmer: Go"}), "list says what to call each record");
}

fn add_linked_collections(env: &TestEnv) {
    env.ctx
        .store
        .write(
            "collections/jobs.toml",
            "[collection]\nid = \"jobs\"\nlabel = \"Jobs\"\n[[field]]\nname = \"company\"\ntype = \"string\"\n",
        )
        .unwrap();
    env.ctx
        .store
        .write(
            "collections/rounds.toml",
            r#"
            [collection]
            id = "rounds"
            label = "Rounds"
            title = "{application}: {kind}"

            [[field]]
            name = "application"
            type = "ref"
            collection = "jobs"   # the job this round is for

            [[field]]
            name = "kind"
            type = "string"

            [[field]]
            name = "also_for"
            type = "list"
            of = "ref"
            collection = "jobs"
            "#,
        )
        .unwrap();
}

#[tokio::test]
async fn references_are_checked_followed_on_rename_and_guard_removal() {
    // ADR 0024 §3.
    let (r, env) = setup().await;
    add_linked_collections(&env);
    let add = |c: &str, id: &str, fields: Value| json!({"collection": c, "id": id, "fields": fields});
    call(&r, &env, "records.add", add("jobs", "acme", json!({"company": "Acme"}))).await.unwrap();
    call(&r, &env, "records.add", add("jobs", "globex", json!({"company": "Globex"}))).await.unwrap();

    let e = call(&r, &env, "records.add", add("rounds", "r0", json!({"application": "nope"}))).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams);
    assert!(e.message.contains("no record 'nope' in 'jobs'"), "{}", e.message);
    let e =
        call(&r, &env, "records.add", add("rounds", "r0", json!({"also_for": ["acme", "nope"]}))).await.unwrap_err();
    assert!(e.message.contains("'nope'"), "{}", e.message);

    let round = add("rounds", "r1", json!({"application": "acme", "kind": "oa", "also_for": ["globex", "acme"]}));
    call(&r, &env, "records.add", round).await.unwrap();
    let list = json!({"collection": "rounds", "filter": {"application": "acme", "also_for": {"has": "globex"}}});
    assert_eq!(call(&r, &env, "records.list", list).await.unwrap()["total"], 1);

    // Removing a job a round points at is refused, unless forced.
    let e = call(&r, &env, "records.remove", json!({"collection": "jobs", "id": "globex"})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict);
    assert!(e.message.contains("rounds/r1"), "{}", e.message);
    assert_eq!(e.detail.unwrap()["referred_by"], json!(["rounds/r1"]));

    // Renaming a job rewrites every reference in the same transaction.
    let rename = json!({"collection": "jobs", "id": "acme", "new_id": "acme-corp"});
    call(&r, &env, "records.rename", rename).await.unwrap();
    assert_eq!(env.backend.events().pop().unwrap().payload["references"], 2);
    let round = call(&r, &env, "records.get", json!({"collection": "rounds", "id": "r1"})).await.unwrap();
    assert_eq!(round["application"], "acme-corp");
    assert_eq!(round["also_for"], json!(["globex", "acme-corp"]));

    // Renaming the collection rewrites `collection = …`, comments kept.
    call(&r, &env, "records.rename_collection", json!({"id": "jobs", "new_id": "applications"})).await.unwrap();
    let text = file(&env, "collections/rounds.toml").unwrap();
    assert!(text.contains("collection = \"applications\"   # the job this round is for"), "{text}");
    let check = call(&r, &env, "records.check", json!({"collection": "rounds"})).await.unwrap();
    assert_eq!(check["problems"], json!([]));

    // Forced, the reference is left dangling and `check` reports it.
    let force = json!({"collection": "applications", "id": "globex", "force": true});
    call(&r, &env, "records.remove", force).await.unwrap();
    let check = call(&r, &env, "records.check", json!({"collection": "rounds"})).await.unwrap();
    let problems = check["problems"][0]["problems"].to_string();
    assert!(problems.contains("no record 'globex' in 'applications'"), "{problems}");
}

#[tokio::test]
async fn import_checks_every_row_and_writes_all_or_nothing() {
    // ADR 0024 §4.
    let (r, env) = setup().await;
    add_postings_collection(&env);
    add_posting(&r, &env, "old", json!({"company": "Old", "url": "https://old"})).await.unwrap();
    let rows = json!([
        {"company": "Acme", "position": "SWE"},
        {"id": "globex", "company": "Globex", "status": "done", "applied_on": "2026-10-01"},
        {"company": "Again", "url": "https://old"},
        {"id": "old", "company": "Old"},
        {"company": "Acme", "position": "SWE", "stage": ""},
        {"position": "no company"}
    ]);
    let import = |extra: Value| {
        let mut p = json!({"collection": "postings", "rows": rows.clone()});
        p.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        p
    };
    let before = env.backend.events().len();

    // One invalid row: nothing is written, and the report says what would happen.
    let out = call(&r, &env, "records.import", import(json!({}))).await.unwrap();
    assert_eq!(out["added"], json!(["acme-swe", "globex", "acme-swe-2"]));
    assert_eq!(out["skipped"].as_array().unwrap().len(), 2, "{out}");
    assert_eq!(out["skipped"][0]["row"], 3);
    assert_eq!(out["invalid"], json!([{"row": 6, "error": "field 'company' is required"}]));
    assert_eq!(out["written"], false);
    assert_eq!(env.backend.events().len(), before);

    let out = call(&r, &env, "records.import", import(json!({"skip_invalid": true, "dry_run": true}))).await.unwrap();
    assert_eq!(out["written"], false, "a dry run never writes");

    let out = call(&r, &env, "records.import", import(json!({"skip_invalid": true}))).await.unwrap();
    assert_eq!(out["written"], true);
    assert_eq!(topics(&env)[before..], ["records.item.created"; 3]);
    let globex = call(&r, &env, "records.get", json!({"collection": "postings", "id": "globex"})).await.unwrap();
    assert_eq!((globex["status"].clone(), globex["applied_on"].clone()), (json!("done"), json!("2026-10-01")));

    // Importing the same rows again adds nothing new by id.
    let again = json!({"collection": "postings", "rows": [{"id": "globex", "company": "Globex"}]});
    assert_eq!(call(&r, &env, "records.import", again).await.unwrap()["added"], json!([]));
    let too_many = json!({"collection": "postings", "rows": vec![json!({"company": "x"}); 5001]});
    assert_eq!(call(&r, &env, "records.import", too_many).await.unwrap_err().code, ErrorCode::InvalidParams);
}

#[tokio::test]
async fn purge_deletes_trashed_records_only_after_confirmation() {
    // ADR 0024 §5.
    let (r, env) = setup().await;
    for id in ["a", "b", "c"] {
        call(&r, &env, "records.add", json!({"collection": "leetcode", "id": id, "fields": {"title": id}}))
            .await
            .unwrap();
        call(&r, &env, "records.remove", json!({"collection": "leetcode", "id": id})).await.unwrap();
    }
    let one = json!({"collection": "leetcode", "id": "a"});
    let e = call(&r, &env, "records.purge", one.clone()).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::ConfirmationRequired);
    assert_eq!(e.detail, Some(json!({"records": 1})));
    assert!(file(&env, "trash/leetcode/a.toml").is_some(), "nothing deleted yet");

    let mut confirmed = one.clone();
    confirmed["confirm"] = json!({"records": 1});
    assert_eq!(call(&r, &env, "records.purge", confirmed).await.unwrap(), json!({"purged": 1}));
    assert!(file(&env, "trash/leetcode/a.toml").is_none());
    let ev = env.backend.events().pop().unwrap();
    assert_eq!(
        (ev.topic.as_str(), ev.payload),
        ("records.trash.purged", json!({"collection": "leetcode", "records": 1, "id": "a"}))
    );
    assert_eq!(call(&r, &env, "records.purge", one).await.unwrap_err().code, ErrorCode::NotFound);

    // The whole trash: a stale count is asked again, never guessed.
    let all = json!({"collection": "leetcode", "confirm": {"records": 3}});
    assert_eq!(call(&r, &env, "records.purge", all).await.unwrap_err().code, ErrorCode::ConfirmationRequired);
    let all = json!({"collection": "leetcode", "confirm": {"records": 2}});
    assert_eq!(call(&r, &env, "records.purge", all).await.unwrap(), json!({"purged": 2}));
    let trash = call(&r, &env, "records.trash", json!({"collection": "leetcode"})).await.unwrap();
    assert_eq!(trash["items"], json!([]));
    assert_eq!(call(&r, &env, "records.purge", json!({"collection": "leetcode"})).await.unwrap(), json!({"purged": 0}));
}

#[tokio::test]
async fn a_file_that_cant_be_read_is_reported_and_hides_nothing() {
    // Dev A's review of #88: a hand-edit that leaves invalid UTF-8 must be one record's problem
    // in records.check (and one skipped file in records.list), not a failure of the whole op.
    let (r, env) = setup().await;
    add_two_sum(&r, &env).await;
    env.ctx.store.write("items/leetcode/garbled.toml", vec![0xff, 0xfe, b'x']).unwrap();
    let check = call(&r, &env, "records.check", json!({"collection": "leetcode"})).await.unwrap();
    assert_eq!(check["checked"], 2);
    assert_eq!(check["problems"][0]["id"], "garbled");
    assert!(check["problems"][0]["problems"][0].as_str().unwrap().contains("can't be read"), "{check}");
    let list = call(&r, &env, "records.list", json!({"collection": "leetcode"})).await.unwrap();
    assert_eq!(list["total"], 1);
    assert!(list["skipped"][0].as_str().unwrap().contains("garbled.toml"), "{list}");
}

/// A collection with a unique field and a reference between its own records, for #99.
const UNREADABLE_PLAN: &str = "[collection]\nid = \"plan\"\nlabel = \"Plan\"\n\n\
[[field]]\nname = \"title\"\ntype = \"string\"\nrequired = true\n\n\
[[field]]\nname = \"url\"\ntype = \"string\"\nunique = true\n\n\
[[field]]\nname = \"follows\"\ntype = \"ref\"\ncollection = \"plan\"\n";

#[tokio::test]
async fn one_unreadable_record_never_blocks_writes_renames_imports_or_the_trash() {
    // #99: an unreadable record file (here invalid UTF-8) used to abort every op that scans the
    // collection: unique checks on each write, reference checks, import, the trash and
    // rename_field. Now it's skipped, and reported where the person would otherwise not know.
    let (r, env) = setup().await;
    env.ctx.store.write("collections/plan.toml", UNREADABLE_PLAN).unwrap();
    let add = |id: &str, fields: Value| json!({"collection": "plan", "id": id, "fields": fields});
    call(&r, &env, "records.add", add("a", json!({"title": "A", "url": "https://x"}))).await.unwrap();
    call(&r, &env, "records.add", add("b", json!({"title": "B", "follows": "a"}))).await.unwrap();
    call(&r, &env, "records.add", add("gone", json!({"title": "Gone"}))).await.unwrap();
    call(&r, &env, "records.remove", json!({"collection": "plan", "id": "gone"})).await.unwrap();
    env.ctx.store.write("items/plan/garbled.toml", vec![0xff, 0xfe, b'x']).unwrap();
    env.ctx.store.write("trash/plan/garbled-too.toml", vec![0xff, 0xfe, b'y']).unwrap();

    // Unique checks run on every write: they skip it, and still catch a real duplicate.
    call(&r, &env, "records.add", add("c", json!({"title": "C", "url": "https://y"}))).await.unwrap();
    let dup = call(&r, &env, "records.add", add("d", json!({"title": "D", "url": "https://x"}))).await.unwrap_err();
    assert_eq!(dup.code, ErrorCode::Conflict, "{}", dup.message);
    let update = json!({"collection": "plan", "id": "c", "fields": {"title": "C2"}});
    call(&r, &env, "records.update", update).await.unwrap();

    // Reference checks: renaming a still repoints b.
    call(&r, &env, "records.rename", json!({"collection": "plan", "id": "a", "new_id": "a2"})).await.unwrap();
    let b = call(&r, &env, "records.get", json!({"collection": "plan", "id": "b"})).await.unwrap();
    assert_eq!(b["follows"], "a2");

    // Import.
    let rows = json!({"collection": "plan", "rows": [{"id": "e", "title": "E"}]});
    assert_eq!(call(&r, &env, "records.import", rows).await.unwrap()["added"], json!(["e"]));

    // The trash lists what it can read, and says what it couldn't.
    let trash = call(&r, &env, "records.trash", json!({"collection": "plan"})).await.unwrap();
    assert_eq!(trash["items"].as_array().unwrap().len(), 1, "{trash}");
    assert_eq!(trash["items"][0]["id"], "gone");
    assert!(trash["skipped"][0].as_str().unwrap().contains("garbled-too.toml"), "{trash}");

    // rename_field renames everywhere it can, and names the files it couldn't (they keep the old
    // name): the unreadable ones and one that's valid text but broken TOML.
    env.ctx.store.write("items/plan/broken.toml", "title = [unclosed").unwrap();
    let renamed = call(&r, &env, "records.rename_field", json!({"collection": "plan", "from": "title", "to": "name"}))
        .await
        .unwrap();
    assert_eq!(renamed["updated"], 5, "a2, b, c, e and gone in the trash: {renamed}");
    let skipped: Vec<&str> = renamed["skipped"].as_array().unwrap().iter().map(|s| s.as_str().unwrap()).collect();
    assert_eq!(skipped.len(), 3, "{skipped:?}");
    for name in ["garbled.toml", "garbled-too.toml", "broken.toml"] {
        assert!(skipped.iter().any(|s| s.contains(name)), "{name} not in {skipped:?}");
    }
    // Nothing unreadable: no `skipped` key at all, as in records.list.
    let clean = call(&r, &env, "records.trash", json!({"collection": "leetcode"})).await.unwrap();
    assert!(clean.get("skipped").is_none(), "{clean}");
}
