//! The built-in workspace templates (ADR 0025 §6), embedded in the binary so they work with no
//! files. Each is a folder in `crates/modules/workspaces/templates/`; the shared helper in
//! `templates/lib/` is copied into every one.

use shimmer_core::{Error, Result};

use crate::template::Template;

macro_rules! file {
    ($path:literal, $from:literal) => {
        ($path, include_str!(concat!("../templates/", $from)))
    };
}

const HELPER: (&str, &str) = file!("lib/shimmer-open.sh", "lib/shimmer-open.sh");
const VM_HELPER: (&str, &str) = file!("lib/shimmer-vm.sh", "lib/shimmer-vm.sh");
const FREE_HELPER: (&str, &str) = file!("lib/shimmer-free.sh", "lib/shimmer-free.sh");
const GITHUB_HELPER: (&str, &str) = file!("lib/shimmer-github.sh", "lib/shimmer-github.sh");
const HEALTH_HELPER: (&str, &str) = file!("lib/shimmer-health.sh", "lib/shimmer-health.sh");
const MONOREPO_HELPER: (&str, &str) = file!("lib/shimmer-monorepo.sh", "lib/shimmer-monorepo.sh");
const OFFLINE_HELPER: (&str, &str) = file!("lib/shimmer-offline.sh", "lib/shimmer-offline.sh");
const SCRATCH_HELPER: (&str, &str) = file!("lib/shimmer-scratch.sh", "lib/shimmer-scratch.sh");
const UPDATE_HELPER: (&str, &str) = file!("lib/shimmer-update.sh", "lib/shimmer-update.sh");
const DAILY_DEV_HELPER: (&str, &str) = file!("lib/shimmer-daily-dev.sh", "lib/shimmer-daily-dev.sh");

/// `(id, files)`, sorted by id.
const BUILTINS: &[(&str, &[(&str, &str)])] = &[
    (
        "daily-dev",
        &[
            file!("template.toml", "daily-dev/template.toml"),
            file!("workspace.toml", "daily-dev/workspace.toml"),
            file!("steps/01-check.sh", "daily-dev/steps/01-check.sh"),
            file!("steps/02-editor.sh", "daily-dev/steps/02-editor.sh"),
            file!("steps/03-system-monitor.sh", "daily-dev/steps/03-system-monitor.sh"),
            file!("steps/04-gpu-monitor.sh", "daily-dev/steps/04-gpu-monitor.sh"),
            file!("steps/05-notes.sh", "daily-dev/steps/05-notes.sh"),
            file!("steps/06-chat.sh", "daily-dev/steps/06-chat.sh"),
            file!("steps/07-browser.sh", "daily-dev/steps/07-browser.sh"),
            file!("steps/08-focus.sh", "daily-dev/steps/08-focus.sh"),
            file!("cleanup.sh", "daily-dev/cleanup.sh"),
            HELPER,
            DAILY_DEV_HELPER,
        ],
    ),
    (
        "free-disk",
        &[
            file!("template.toml", "free-disk/template.toml"),
            file!("workspace.toml", "free-disk/workspace.toml"),
            file!("steps/01-check.sh", "free-disk/steps/01-check.sh"),
            file!("steps/02-projects.sh", "free-disk/steps/02-projects.sh"),
            file!("steps/03-caches.sh", "free-disk/steps/03-caches.sh"),
            file!("steps/04-system.sh", "free-disk/steps/04-system.sh"),
            file!("steps/05-summary.sh", "free-disk/steps/05-summary.sh"),
            file!("cleanup.sh", "free-disk/cleanup.sh"),
            HELPER,
            FREE_HELPER,
        ],
    ),
    (
        "github-inbox",
        &[
            file!("template.toml", "github-inbox/template.toml"),
            file!("workspace.toml", "github-inbox/workspace.toml"),
            file!("steps/01-check.sh", "github-inbox/steps/01-check.sh"),
            file!("steps/02-fetch.sh", "github-inbox/steps/02-fetch.sh"),
            file!("steps/03-open.sh", "github-inbox/steps/03-open.sh"),
            file!("steps/04-summary.sh", "github-inbox/steps/04-summary.sh"),
            file!("cleanup.sh", "github-inbox/cleanup.sh"),
            HELPER,
            GITHUB_HELPER,
        ],
    ),
    (
        "health",
        &[
            file!("template.toml", "health/template.toml"),
            file!("workspace.toml", "health/workspace.toml"),
            file!("steps/01-check.sh", "health/steps/01-check.sh"),
            file!("steps/02-report.sh", "health/steps/02-report.sh"),
            file!("steps/03-summary.sh", "health/steps/03-summary.sh"),
            file!("cleanup.sh", "health/cleanup.sh"),
            HELPER,
            HEALTH_HELPER,
        ],
    ),
    (
        "monorepo",
        &[
            file!("template.toml", "monorepo/template.toml"),
            file!("workspace.toml", "monorepo/workspace.toml"),
            file!("steps/01-check.sh", "monorepo/steps/01-check.sh"),
            file!("steps/02-git.sh", "monorepo/steps/02-git.sh"),
            file!("steps/03-install.sh", "monorepo/steps/03-install.sh"),
            file!("steps/04-services.sh", "monorepo/steps/04-services.sh"),
            file!("steps/05-editor.sh", "monorepo/steps/05-editor.sh"),
            file!("steps/06-dev.sh", "monorepo/steps/06-dev.sh"),
            file!("steps/07-wait.sh", "monorepo/steps/07-wait.sh"),
            file!("steps/08-browser.sh", "monorepo/steps/08-browser.sh"),
            file!("steps/09-logs.sh", "monorepo/steps/09-logs.sh"),
            file!("steps/10-terminal.sh", "monorepo/steps/10-terminal.sh"),
            file!("cleanup.sh", "monorepo/cleanup.sh"),
            HELPER,
            MONOREPO_HELPER,
        ],
    ),
    (
        "offline-prep",
        &[
            file!("template.toml", "offline-prep/template.toml"),
            file!("workspace.toml", "offline-prep/workspace.toml"),
            file!("steps/01-check.sh", "offline-prep/steps/01-check.sh"),
            file!("steps/02-machine.sh", "offline-prep/steps/02-machine.sh"),
            file!("steps/03-repos.sh", "offline-prep/steps/03-repos.sh"),
            file!("steps/04-deps.sh", "offline-prep/steps/04-deps.sh"),
            file!("steps/05-docker.sh", "offline-prep/steps/05-docker.sh"),
            file!("steps/06-build.sh", "offline-prep/steps/06-build.sh"),
            file!("steps/07-verify.sh", "offline-prep/steps/07-verify.sh"),
            file!("steps/08-docs.sh", "offline-prep/steps/08-docs.sh"),
            file!("steps/09-pages.sh", "offline-prep/steps/09-pages.sh"),
            file!("steps/10-github.sh", "offline-prep/steps/10-github.sh"),
            file!("steps/11-ai.sh", "offline-prep/steps/11-ai.sh"),
            file!("steps/12-summary.sh", "offline-prep/steps/12-summary.sh"),
            file!("cleanup.sh", "offline-prep/cleanup.sh"),
            HELPER,
            OFFLINE_HELPER,
        ],
    ),
    (
        "scratch",
        &[
            file!("template.toml", "scratch/template.toml"),
            file!("workspace.toml", "scratch/workspace.toml"),
            file!("steps/01-check.sh", "scratch/steps/01-check.sh"),
            file!("steps/02-folder.sh", "scratch/steps/02-folder.sh"),
            file!("steps/03-editor.sh", "scratch/steps/03-editor.sh"),
            file!("steps/04-terminal.sh", "scratch/steps/04-terminal.sh"),
            file!("steps/05-summary.sh", "scratch/steps/05-summary.sh"),
            file!("cleanup.sh", "scratch/cleanup.sh"),
            HELPER,
            SCRATCH_HELPER,
        ],
    ),
    (
        "smoke-test",
        &[
            file!("template.toml", "smoke-test/template.toml"),
            file!("workspace.toml", "smoke-test/workspace.toml"),
            file!("steps/01-check.sh", "smoke-test/steps/01-check.sh"),
            file!("steps/02-wait.sh", "smoke-test/steps/02-wait.sh"),
            file!("steps/03-background.sh", "smoke-test/steps/03-background.sh"),
            file!("cleanup.sh", "smoke-test/cleanup.sh"),
            HELPER,
        ],
    ),
    (
        "update-everything",
        &[
            file!("template.toml", "update-everything/template.toml"),
            file!("workspace.toml", "update-everything/workspace.toml"),
            file!("steps/01-check.sh", "update-everything/steps/01-check.sh"),
            file!("steps/02-system.sh", "update-everything/steps/02-system.sh"),
            file!("steps/03-tools.sh", "update-everything/steps/03-tools.sh"),
            file!("steps/04-summary.sh", "update-everything/steps/04-summary.sh"),
            file!("cleanup.sh", "update-everything/cleanup.sh"),
            HELPER,
            UPDATE_HELPER,
        ],
    ),
    (
        "vm",
        &[
            file!("template.toml", "vm/template.toml"),
            file!("workspace.toml", "vm/workspace.toml"),
            file!("steps/01-check.sh", "vm/steps/01-check.sh"),
            file!("steps/02-start.sh", "vm/steps/02-start.sh"),
            file!("steps/03-wait.sh", "vm/steps/03-wait.sh"),
            file!("steps/04-editor.sh", "vm/steps/04-editor.sh"),
            file!("steps/05-terminal.sh", "vm/steps/05-terminal.sh"),
            file!("cleanup.sh", "vm/cleanup.sh"),
            HELPER,
            VM_HELPER,
        ],
    ),
    (
        "web-project",
        &[
            file!("template.toml", "web-project/template.toml"),
            file!("workspace.toml", "web-project/workspace.toml"),
            file!("steps/01-check.sh", "web-project/steps/01-check.sh"),
            file!("steps/02-git.sh", "web-project/steps/02-git.sh"),
            file!("steps/03-install.sh", "web-project/steps/03-install.sh"),
            file!("steps/04-services.sh", "web-project/steps/04-services.sh"),
            file!("steps/05-editor.sh", "web-project/steps/05-editor.sh"),
            file!("steps/06-dev-server.sh", "web-project/steps/06-dev-server.sh"),
            file!("steps/07-wait.sh", "web-project/steps/07-wait.sh"),
            file!("steps/08-browser.sh", "web-project/steps/08-browser.sh"),
            file!("steps/09-terminal.sh", "web-project/steps/09-terminal.sh"),
            file!("cleanup.sh", "web-project/cleanup.sh"),
            HELPER,
        ],
    ),
];

/// Every built-in template, checked. A broken one is a bug, and a test parses them all.
pub fn all() -> Result<Vec<Template>> {
    BUILTINS.iter().map(|(id, files)| Template::parse(id, files)).collect()
}

/// One template, or `not_found` naming the ones there are.
pub fn find(id: &str) -> Result<Template> {
    match BUILTINS.iter().find(|(t, _)| *t == id) {
        Some((id, files)) => Template::parse(id, files),
        None => {
            let names: Vec<&str> = BUILTINS.iter().map(|(id, _)| *id).collect();
            Err(Error::not_found(format!("no workspace template '{id}' (there are: {})", names.join(", "))))
        }
    }
}
