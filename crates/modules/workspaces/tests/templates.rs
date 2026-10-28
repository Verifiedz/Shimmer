//! Workspace templates (ADR 0025) through `handle`, against the in-memory `Ctx` and
//! `FakeLauncher` (CLAUDE.md §12 rule 13).

use std::process::Command;

use serde_json::{json, Value};
use shimmer_core::testing::TestEnv;
use shimmer_core::{ErrorCode, Module, Result, StepOutcome, StoreBackend};
use shimmer_workspaces::Workspaces;

async fn call(w: &Workspaces, env: &TestEnv, op: &str, params: Value) -> Result<Value> {
    w.handle(op, params, &env.ctx).await
}

fn file(env: &TestEnv, path: &str) -> Option<String> {
    env.backend.read("workspaces", path).unwrap().map(|b| String::from_utf8(b).unwrap())
}

/// Answers for every required question of every built-in.
fn answers(template: &str) -> Value {
    match template {
        "web-project" => json!({"PROJECT_DIR": "/home/me/code/site"}),
        "vm" => json!({"VM_SOFTWARE": "virtualbox", "VM_NAME": "Debian", "ON_STOP": "suspend"}),
        "monorepo" => json!({"PROJECT_DIR": "/home/me/code/acme", "APPS": "web api"}),
        "offline-prep" => json!({"PROJECTS": "~/code/site ~/code/api", "UPDATE_REPOS": "fetch-only"}),
        "free-disk" => json!({"MODE": "preview"}),
        "update-everything" => json!({"MODE": "preview", "SYSTEM_UPDATES": "skip"}),
        _ => json!({}),
    }
}

#[tokio::test]
async fn templates_lists_the_built_ins_with_their_questions() {
    let env = TestEnv::new("workspaces");
    let data = call(&Workspaces::default(), &env, "workspaces.templates", json!({})).await.unwrap();
    let ids: Vec<&str> = data["templates"].as_array().unwrap().iter().map(|t| t["id"].as_str().unwrap()).collect();
    // By category (code, daily, upkeep, travel, testing), then id.
    assert_eq!(
        ids,
        [
            "monorepo",
            "scratch",
            "vm",
            "web-project",
            "daily-dev",
            "gaming",
            "github-inbox",
            "free-disk",
            "health",
            "update-everything",
            "offline-prep",
            "smoke-test",
        ]
    );
    assert_eq!(data["categories"][0], json!({"id": "code", "label": "Coding"}));
    assert_eq!(data["templates"][0]["category"], "code");
    let web = &data["templates"][3];
    assert_eq!(
        web["questions"][0],
        json!({"name": "PROJECT_DIR", "prompt": "Project folder", "kind": "folder", "required": true})
    );
    assert_eq!(web["questions"][1]["choices"][1], "cursor");
    assert_eq!(web["questions"][1]["default"], "vscode");
}

#[tokio::test]
async fn every_built_in_creates_a_workspace_that_launches() {
    let data =
        call(&Workspaces::default(), &TestEnv::new("workspaces"), "workspaces.templates", json!({})).await.unwrap();
    for t in data["templates"].as_array().unwrap() {
        let id = t["id"].as_str().unwrap();
        let env = TestEnv::new("workspaces");
        let w = Workspaces::default();
        let created = call(&w, &env, "workspaces.new", json!({"id": "mine", "template": id, "values": answers(id)}))
            .await
            .unwrap_or_else(|e| panic!("{id}: {}", e.message));
        assert_eq!(created["state"], "ready", "{id}");
        assert_eq!(created["has_cleanup_script"], true, "{id}");
        assert!(file(&env, "mine/lib/shimmer-open.sh").is_some(), "{id}: the helper is copied");
        assert!(file(&env, "mine/template.toml").is_none(), "{id}: the questions are not");
        assert!(file(&env, "mine/state.toml").is_none(), "{id}: it starts ready, with no state file");
        assert_eq!(env.backend.events().pop().unwrap().payload, json!({"workspace": "mine", "template": id}));

        // Every step launches through the launcher, in order.
        let steps = created["steps"].as_array().unwrap();
        let outcomes = steps.iter().map(|s| {
            let supervised = s["mode"] == "supervised";
            Ok(StepOutcome {
                session_id: "01S".into(),
                exit_code: supervised.then_some(0),
                timed_out: false,
                log_path: String::new(),
            })
        });
        env.launcher.outcomes.lock().unwrap().extend(outcomes);
        let done = call(&w, &env, "workspaces.activate", json!({"id": "mine"})).await.unwrap();
        assert_eq!(done["state"], "active", "{id}");
        let received = env.launcher.received.lock().unwrap();
        assert_eq!(received.len(), steps.len(), "{id}");
        assert!(
            received[0].user_env.iter().any(|(k, _)| k == t["questions"][0]["name"].as_str().unwrap()),
            "{id}: answers are [env]"
        );
    }
}

#[tokio::test]
async fn answers_become_env_and_the_label_can_be_set() {
    let env = TestEnv::new("workspaces");
    let w = Workspaces::default();
    let values = json!({"PROJECT_DIR": "/home/me/code/site", "CODE_EDITOR": "cursor",
                        "LINKS": "https://me.dev https://vercel.com/me/site/deployments", "TERMINAL_COMMAND": "claude"});
    let created = call(
        &w,
        &env,
        "workspaces.new",
        json!({"id": "site", "template": "web-project", "values": values, "label": "My site"}),
    )
    .await
    .unwrap();
    assert_eq!(created["label"], "My site");
    let text = file(&env, "site/workspace.toml").unwrap();
    let doc: toml::Table = toml::from_str(&text).unwrap();
    let env_table = doc["env"].as_table().unwrap();
    assert_eq!(env_table["CODE_EDITOR"].as_str(), Some("cursor"));
    assert_eq!(env_table["DEV_COMMAND"].as_str(), Some("auto"), "defaults are written");
    assert_eq!(env_table["SERVICES_COMMAND"].as_str(), Some(""), "empty answers are written too");
    assert!(text.starts_with("# Web project:"), "the template's comments are kept");
}

#[tokio::test]
async fn create_never_overwrites_and_checks_everything_first() {
    let env = TestEnv::new("workspaces");
    let w = Workspaces::default();
    let create = |id: &str, template: &str, values: Value| json!({"id": id, "template": template, "values": values});

    let e = call(&w, &env, "workspaces.new", create("site", "web-project", json!({}))).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams);
    assert!(e.message.contains("PROJECT_DIR (Project folder) is required"), "{}", e.message);
    let e = call(&w, &env, "workspaces.new", create("site", "web-project", json!({"PROJECT_DIR": "~/site"})))
        .await
        .unwrap_err();
    assert!(e.message.contains("absolute path"), "{}", e.message);
    let e = call(&w, &env, "workspaces.new", create("Site!", "smoke-test", json!({}))).await.unwrap_err();
    assert!(e.message.contains("not a valid workspace id"), "{}", e.message);
    let e = call(&w, &env, "workspaces.new", create("site", "nope", json!({}))).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
    assert!(
        e.message.contains(
            "there are: daily-dev, free-disk, gaming, github-inbox, health, monorepo, offline-prep, scratch, smoke-test, update-everything, vm, web-project"
        ),
        "{}",
        e.message
    );
    assert!(env.backend.events().is_empty(), "nothing written");

    env.ctx.store.write("site/notes.txt", "mine").unwrap();
    let e = call(&w, &env, "workspaces.new", create("site", "smoke-test", json!({}))).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict);
    assert_eq!(file(&env, "site/notes.txt").as_deref(), Some("mine"));
    assert!(file(&env, "site/workspace.toml").is_none());
}

/// Every script is valid POSIX `sh`: Debian's `sh` is dash (ADR 0012 §5).
#[test]
fn every_script_is_valid_sh() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/templates");
    let mut checked = 0;
    let mut dirs = vec![std::path::PathBuf::from(root)];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "sh") {
                let out = Command::new("sh").arg("-n").arg(&path).output().unwrap();
                assert!(out.status.success(), "{}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
                checked += 1;
            }
        }
    }
    assert!(checked >= 15, "found {checked} scripts");
}

/// Load the shared helper as a step does, with `TMPDIR` and `SHIMMER_HOME` set: the folder it
/// keeps run-time files in, or the error it stopped with.
#[cfg(unix)]
fn state_dir(tmp: &std::path::Path, home: &str) -> std::result::Result<String, String> {
    let helper = concat!(env!("CARGO_MANIFEST_DIR"), "/templates/lib/shimmer-open.sh");
    let out = Command::new("sh")
        .args(["-c", ". \"$1\" && printf %s \"$SHIMMER_STATE_DIR\"", "sh", helper])
        .env("TMPDIR", tmp)
        .env("SHIMMER_HOME", home)
        .env("SHIMMER_WORKSPACE_ID", "site")
        .output()
        .unwrap();
    match out.status.success() {
        true => Ok(String::from_utf8(out.stdout).unwrap()),
        false => Err(String::from_utf8_lossy(&out.stderr).into_owned()),
    }
}

/// #165: templates keep pid files in this folder; one shared by every Shimmer folder and user
/// let a workspace stop another's processes, or let someone plant a pid for it to signal.
#[cfg(unix)]
#[test]
fn run_time_files_live_in_a_private_folder_per_user_shimmer_folder_and_workspace() {
    use std::os::unix::fs::PermissionsExt;
    /// Removes the test's folder even when an assert fails.
    struct Gone(std::path::PathBuf);
    impl Drop for Gone {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let tmp = std::env::temp_dir().join(format!("shimmer-state-test-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let _gone = Gone(tmp.clone());
    let uid = String::from_utf8(Command::new("id").arg("-u").output().unwrap().stdout).unwrap();

    let a = state_dir(&tmp, "/home/a/.local/share/shimmer").unwrap();
    let b = state_dir(&tmp, "/home/b/.local/share/shimmer").unwrap();
    assert_ne!(a, b, "two Shimmer folders with a workspace named 'site' must not share one");
    assert!(a.ends_with("-site"), "{a}");
    let name = std::path::Path::new(&a).file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.starts_with(&format!("shimmer-{}-", uid.trim())), "named after the user: {name}");
    assert_eq!(state_dir(&tmp, "/home/a/.local/share/shimmer").unwrap(), a, "the same each time");
    let mode = std::fs::metadata(&a).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "private: {mode:o}");
    // Our own folder left with loose permissions is made private again.
    std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o777)).unwrap();
    state_dir(&tmp, "/home/a/.local/share/shimmer").unwrap();
    assert_eq!(std::fs::metadata(&a).unwrap().permissions().mode() & 0o777, 0o700, "tightened again");

    // Something already at that path that isn't our own folder is refused.
    std::fs::remove_dir(&b).unwrap();
    std::os::unix::fs::symlink(&tmp, &b).unwrap();
    let e = state_dir(&tmp, "/home/b/.local/share/shimmer").unwrap_err();
    assert!(e.contains("not a folder of your own") && e.contains("a reboot clears it"), "{e}");
    std::fs::remove_file(&b).unwrap();
    std::fs::write(&b, "1234").unwrap();
    assert!(state_dir(&tmp, "/home/b/.local/share/shimmer").is_err(), "a planted file too");
}

#[tokio::test]
async fn the_vm_template_never_takes_a_password_only_a_keychain_entry_name() {
    // ADR 0025 §6: an answer is written to workspace.toml in plain text, so the vm template
    // asks for the NAME of a keychain entry, and its scripts read the secret at run time.
    let env = TestEnv::new("workspaces");
    let w = Workspaces::default();
    let values = json!({"VM_SOFTWARE": "utm", "VM_NAME": "Debian", "ON_STOP": "suspend", "VM_HOST": "auto", "SSH_USER": "me",
                        "SSH_PASSWORD_ITEM": "shimmer-vm-debian", "REMOTE_EDITOR": "cursor"});
    w.handle("workspaces.new", json!({"id": "dev-vm", "template": "vm", "values": values}), &env.ctx).await.unwrap();
    let text = file(&env, "dev-vm/workspace.toml").unwrap();
    assert!(text.contains("SSH_PASSWORD_ITEM = \"shimmer-vm-debian\""), "{text}");
    assert!(file(&env, "dev-vm/lib/shimmer-vm.sh").is_some(), "the vm helper is copied");
    let questions = w.handle("workspaces.templates", json!({}), &env.ctx).await.unwrap();
    let vm = questions["templates"].as_array().unwrap().iter().find(|t| t["id"] == "vm").unwrap();
    let names: Vec<&str> = vm["questions"].as_array().unwrap().iter().map(|q| q["name"].as_str().unwrap()).collect();
    assert!(!names.iter().any(|n| n.ends_with("PASSWORD")), "no question asks for a password itself: {names:?}");

    let e = w
        .handle(
            "workspaces.new",
            json!({"id": "x", "template": "vm", "values": {"VM_SOFTWARE": "hyperv", "VM_NAME": "a"}}),
            &env.ctx,
        )
        .await
        .unwrap_err();
    assert!(e.message.contains("must be one of utm, parallels"), "{}", e.message);
}

#[tokio::test]
async fn one_template_can_be_asked_for_with_its_files() {
    // `peek --scripts` (ADR 0025 §8): every file a workspace made from it gets, as it gets them.
    let env = TestEnv::new("workspaces");
    let w = Workspaces::default();
    let data = call(&w, &env, "workspaces.templates", json!({"id": "free-disk", "files": true})).await.unwrap();
    let templates = data["templates"].as_array().unwrap();
    assert_eq!(templates.len(), 1);
    let files = templates[0]["files"].as_array().unwrap();
    let paths: Vec<&str> = files.iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert!(paths.contains(&"steps/02-projects.sh") && paths.contains(&"cleanup.sh"), "{paths:?}");
    assert!(paths.contains(&"lib/shimmer-free.sh") && !paths.contains(&"template.toml"), "{paths:?}");
    let script = files.iter().find(|f| f["path"] == "steps/02-projects.sh").unwrap();
    assert!(script["text"].as_str().unwrap().contains("rm -rf"), "the real script");

    // Without `files`, no files: the list stays small.
    let all = call(&w, &env, "workspaces.templates", json!({})).await.unwrap();
    assert!(all["templates"][0].get("files").is_none());
    let e = call(&w, &env, "workspaces.templates", json!({"id": "nope"})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
    let e = call(&w, &env, "workspaces.templates", json!({"scripts": true})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams, "unknown params are named, not ignored");
}
