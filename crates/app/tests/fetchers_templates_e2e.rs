//! Every built-in fetchers source template, end to end (ADR 0025 §6a; ADR 0028 §10), through
//! the real binary and daemon: make it from the template with `--set` answers, check the
//! rendered source file, and -- where a stub is actually reachable -- that `fetchers test`
//! fetches from it and parses what it got.
//!
//! `rss`'s own answer is the url, so it's stubbed directly. `greenhouse`/`lever` bake a fixed
//! host into the template text with only a path segment (BOARD/COMPANY) filled in -- checked
//! against that real text, then hand-patched to the stub's address exactly as a person editing
//! the file by hand would (`redirect_url`), the same "edit the file, then `fetchers reload`"
//! round trip `fetchers.edit`'s own doc comment describes. `hn-whoishiring`/`wwr` hit a fixed,
//! compiled-in host with no url of their own to redirect at all (one is HN's real API, the
//! other a scraped page behind its own robots.txt check) -- those two are checked through
//! `fetchers show` only: the template renders the right kind, category and filter.

mod common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};

use common::{ok, Home};
use serde_json::Value;

fn json(home: &Home, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend_from_slice(args);
    serde_json::from_str(&ok(home, &all)).unwrap()
}

/// The file text `fetchers show SOURCE` says is on disk.
fn source_text(home: &Home, id: &str) -> String {
    json(home, &["fetchers", "show", id])["text"].as_str().unwrap().to_owned()
}

/// Serves `body` (as `content_type`) to every request it gets, on its own thread, for as long
/// as the test runs.
fn stub_server(content_type: &'static str, body: &'static str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut stream = stream;
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                );
                let _ = stream.write_all(resp.as_bytes());
            });
        }
    });
    addr
}

/// Patches SOURCE's `url` line directly on disk, to `addr`, exactly as a hand edit would, then
/// has the daemon re-read it -- `fetchers.reload`'s whole reason to exist. Only the `[kind]`
/// table's own `url = "..."` line changes; `url_field`/`items_path` etc. (greenhouse's,
/// lever's) are untouched.
fn redirect_url(home: &Home, id: &str, addr: SocketAddr) {
    let path = home.dir.path().join(format!("home/data/fetchers/sources/{id}.toml"));
    let text = std::fs::read_to_string(&path).unwrap();
    let patched: String = text
        .lines()
        .map(|l| if l.trim_start().starts_with("url = ") { format!("url = \"http://{addr}/\"") } else { l.to_owned() })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(&path, patched).unwrap();
    // No `fetchers reload` CLI subcommand exists yet; `fetchers.reload`'s own op does.
    ok(home, &["call", "fetchers.reload", "{}"]);
}

#[test]
fn templates_list_and_peek_show_what_they_make() {
    let home = Home::new();
    let out = ok(&home, &["fetchers", "templates"]);
    for id in ["greenhouse", "lever", "rss", "wwr", "hn-whoishiring"] {
        assert!(out.contains(id), "{id}: {out}");
    }
    let peek = ok(&home, &["fetchers", "peek", "rss"]);
    assert!(peek.contains("Works for RSS 0.x/1.0/2.0 and Atom alike"), "{peek}");
}

#[test]
fn rss_template_makes_a_source_that_actually_fetches() {
    let home = Home::new();
    let feed = r#"<?xml version="1.0"?><rss version="2.0"><channel><title>T</title>
<item><title>Junior Rust Engineer</title><link>https://example.test/jobs/1</link><guid>1</guid></item>
</channel></rss>"#;
    let addr = stub_server("application/rss+xml", feed);
    let url = format!("http://{addr}/feed.xml");

    let out = ok(&home, &["fetchers", "new", "blog", "--from", "rss", "--set", &format!("URL={url}"), "--yes"]);
    assert!(out.contains("✓ made blog from rss, switched off"), "{out}");
    let text = source_text(&home, "blog");
    assert!(text.contains(&format!("url = \"{url}\"")) && text.contains("kind = \"rss\""), "{text}");

    let items = json(&home, &["fetchers", "test", "blog"])["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["title"], "Junior Rust Engineer");
    assert_eq!(items[0]["url"], "https://example.test/jobs/1");
}

#[test]
fn greenhouse_template_renders_the_real_url_and_fetches_once_redirected() {
    let home = Home::new();
    let out = ok(&home, &["fetchers", "new", "roles", "--from", "greenhouse", "--set", "BOARD=acme", "--yes"]);
    assert!(out.contains("✓ made roles from greenhouse, switched off"), "{out}");
    let text = source_text(&home, "roles");
    assert!(
        text.contains("url = \"https://boards-api.greenhouse.io/v1/boards/acme/jobs\"")
            && text.contains("items_path = \"jobs\"")
            && text.contains("kind = \"json-api\""),
        "{text}"
    );

    let body = r#"{"jobs": [{"id": 42, "title": "Backend Engineer", "absolute_url": "https://boards.greenhouse.io/acme/jobs/42"}]}"#;
    let addr = stub_server("application/json", body);
    redirect_url(&home, "roles", addr);

    let items = json(&home, &["fetchers", "test", "roles"])["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["title"], "Backend Engineer");
    assert_eq!(items[0]["url"], "https://boards.greenhouse.io/acme/jobs/42");
}

#[test]
fn lever_template_renders_the_real_url_and_fetches_once_redirected() {
    let home = Home::new();
    let out = ok(&home, &["fetchers", "new", "roles", "--from", "lever", "--set", "COMPANY=acme", "--yes"]);
    assert!(out.contains("✓ made roles from lever, switched off"), "{out}");
    let text = source_text(&home, "roles");
    assert!(
        text.contains("url = \"https://api.lever.co/v0/postings/acme?mode=json\"")
            && text.contains("kind = \"json-api\""),
        "{text}"
    );

    let body = r#"[{"id": "abc", "text": "Platform Engineer", "hostedUrl": "https://jobs.lever.co/acme/abc"}]"#;
    let addr = stub_server("application/json", body);
    redirect_url(&home, "roles", addr);

    let items = json(&home, &["fetchers", "test", "roles"])["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["title"], "Platform Engineer");
    assert_eq!(items[0]["url"], "https://jobs.lever.co/acme/abc");
}

#[test]
fn hn_whoishiring_template_renders_its_kind_and_optional_filter() {
    let home = Home::new();
    let out = ok(
        &home,
        &["fetchers", "new", "hn", "--from", "hn-whoishiring", "--set", "TITLE_FILTER=junior, entry level", "--yes"],
    );
    assert!(out.contains("✓ made hn from hn-whoishiring, switched off"), "{out}");
    let shown = json(&home, &["fetchers", "show", "hn"]);
    assert_eq!(shown["kind"], "hn-whoishiring");
    assert_eq!(shown["enabled"], false);
    let text = shown["text"].as_str().unwrap();
    assert!(text.contains("kind = \"hn-whoishiring\""), "{text}");
    assert!(text.contains("contains = \"junior\"") && text.contains("contains = \"entry level\""), "{text}");

    // No TITLE_FILTER at all: the optional "# if TITLE_FILTER" line is dropped, not written
    // with an empty filter.
    ok(&home, &["fetchers", "new", "hn2", "--from", "hn-whoishiring", "--yes"]);
    assert!(!source_text(&home, "hn2").contains("filter"), "{}", source_text(&home, "hn2"));
}

#[test]
fn wwr_template_renders_its_category_and_optional_filter() {
    let home = Home::new();
    let out = ok(
        &home,
        &[
            "fetchers",
            "new",
            "remote",
            "--from",
            "wwr",
            "--set",
            "CATEGORY=remote-full-stack-programming-jobs",
            "--set",
            "TITLE_FILTER=senior",
            "--yes",
        ],
    );
    assert!(out.contains("✓ made remote from wwr, switched off"), "{out}");
    let shown = json(&home, &["fetchers", "show", "remote"]);
    assert_eq!(shown["kind"], "wwr");
    let text = shown["text"].as_str().unwrap();
    assert!(
        text.contains("kind = \"wwr\"") && text.contains("category = \"remote-full-stack-programming-jobs\""),
        "{text}"
    );
    assert!(text.contains("contains = \"senior\""), "{text}");
}
