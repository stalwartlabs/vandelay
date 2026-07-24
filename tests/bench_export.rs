/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! Throughput probe for email export against a live Stalwart container.
//!
//! Not an assertion test: it prints a table of mails/second per `--threads`
//! setting so the export pipeline can be compared against the server's own
//! advertised concurrency limits.

mod integration;
mod seeder;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use integration::stalwart::shared as shared_stalwart;
use serde_json::Value;
use vandelay::db;
use vandelay::jmap::account::AccountSelector;
use vandelay::jmap::http::Auth;
use vandelay::logging::Logger;
use vandelay::sync::{self, CommonConfig, ConnectConfig, ExportConfig};

fn mails() -> usize {
    std::env::var("BENCH_MAILS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000)
}

fn thread_settings() -> Vec<usize> {
    match std::env::var("BENCH_THREADS") {
        Ok(v) => v.split(',').filter_map(|s| s.trim().parse().ok()).collect(),
        Err(_) => vec![1, 4, 8, 16],
    }
}

fn bench_verbose() -> u8 {
    std::env::var("BENCH_VERBOSE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn base_url() -> &'static str {
    shared_stalwart().base_url()
}

fn tmp_archive(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "vandelay-bench-{tag}-{}-{}.sqlite",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

/// Unique per process run so repeated runs never collide by Message-ID
/// (export is convergent — identical mails would be skipped, not created).
fn run_tag() -> String {
    std::env::var("BENCH_TAG").unwrap_or_else(|_| {
        format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        )
    })
}

/// One dedicated `vandelay-bench` mailbox (no role, so it never merges into a
/// real Inbox) plus `mails()` short RFC822 messages, each unique by Message-ID.
fn build_archive(path: &PathBuf) {
    let conn = db::init::open(path).expect("open archive");
    conn.execute(
        "INSERT INTO mailboxes (id,name,parent_id,role) VALUES (1,'vandelay-bench',NULL,NULL)",
        [],
    )
    .expect("insert mailbox");
    let tag = run_tag();
    let tx = conn.unchecked_transaction().expect("tx");
    for n in 1..=mails() {
        let raw = format!(
            "From: bench@vandelay.org\r\nTo: sink@vandelay.org\r\n\
             Subject: vandelay bench {tag} {n}\r\nMessage-ID: <bench-{tag}-{n}@vandelay.org>\r\n\
             Date: Wed, 01 Jan 2020 00:00:00 +0000\r\n\r\nbody {n}\r\n"
        );
        let blob = db::blobs::intern_blob(&tx, raw.as_bytes()).expect("intern blob");
        tx.execute(
            "INSERT INTO emails (blob_id,received_at,mailbox_ids,keywords)
             VALUES (?1,'2020-01-01T00:00:00Z','[1]','[]')",
            rusqlite::params![blob],
        )
        .expect("insert email");
    }
    tx.commit().expect("commit");
}

fn fresh_target(admin: &seeder::admin::Admin, domain_id: &str, localpart: &str) -> String {
    admin
        .create_account(localpart, domain_id, seeder::USER_PASSWORD, false)
        .expect("create bench account");
    admin.invalidate_caches().expect("invalidate caches");
    let email = format!("{localpart}@{}", seeder::DOMAIN);
    seeder::jmap::Jmap::connect(base_url(), &email, seeder::USER_PASSWORD)
        .expect("connect bench account")
        .account_id
}

fn core_limits(session: &Value) -> String {
    let core = session
        .get("capabilities")
        .and_then(|c| c.get("urn:ietf:params:jmap:core"));
    match core {
        Some(c) => format!(
            "maxConcurrentRequests={} maxConcurrentUpload={} maxCallsInRequest={} maxObjectsInSet={} maxSizeUpload={}",
            c.get("maxConcurrentRequests").unwrap_or(&Value::Null),
            c.get("maxConcurrentUpload").unwrap_or(&Value::Null),
            c.get("maxCallsInRequest").unwrap_or(&Value::Null),
            c.get("maxObjectsInSet").unwrap_or(&Value::Null),
            c.get("maxSizeUpload").unwrap_or(&Value::Null),
        ),
        None => "no urn:ietf:params:jmap:core capability".to_owned(),
    }
}

struct Run {
    threads: usize,
    elapsed: Duration,
    created: u64,
    failed: u64,
    retries: u64,
    retry_after_sleeps: u64,
}

fn run_export(threads: usize, target_id: &str, localpart: &str) -> Run {
    let archive = tmp_archive(&format!("t{threads}"));
    build_archive(&archive);

    let common = CommonConfig {
        archive: archive.clone(),
        threads,
        dry_run: false,
        max_retries: 5,
        allow_invalid_certs: true,
        logger: Logger::from_flags(false, 0),
    };
    let config = ExportConfig {
        connect: ConnectConfig {
            url: base_url().to_owned(),
            auth: Auth::Basic {
                user: format!("{localpart}@{}", seeder::DOMAIN),
                password: seeder::USER_PASSWORD.to_owned(),
            },
            account: AccountSelector::Id(target_id.to_owned()),
        },
        objects: None,
        prune: false,
        yes: true,
    };

    let started = Instant::now();
    let summary = sync::export::run(common, config).expect("export run");
    let elapsed = started.elapsed();

    let email = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "Email")
        .map(|(_, c)| c.clone())
        .expect("email counts");
    let _ = std::fs::remove_file(&archive);

    Run {
        threads,
        elapsed,
        created: email.created,
        failed: email.failed,
        retries: summary.retries_observed,
        retry_after_sleeps: summary.retry_after_sleeps,
    }
}

fn dotenv() -> std::collections::HashMap<String, String> {
    let raw = std::fs::read_to_string(".env").expect("read .env");
    raw.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect()
}

/// Same probe against a remote JMAP server configured in `.env`.
/// Writes `BENCH_MAILS` messages into the account's inbox — keep it small.
#[test]
#[ignore = "writes to a remote server configured in .env"]
fn bench_export_remote() {
    let env = dotenv();
    let raw_url = env.get("JMAP_SERVER").expect("JMAP_SERVER").clone();
    let url = if raw_url.starts_with("http") {
        raw_url
    } else {
        format!("https://{raw_url}")
    };
    let user = env.get("JMAP_ACCOUNT").expect("JMAP_ACCOUNT").clone();
    let password = env.get("JMAP_PASSWORD").expect("JMAP_PASSWORD").clone();

    if let Ok(path) = std::env::var("BENCH_BUILD_ARCHIVE") {
        // Build a corpus archive and stop, so the CLI can be driven against it.
        let p = PathBuf::from(&path);
        let _ = std::fs::remove_file(&p);
        build_archive(&p);
        println!("archive written: {path}");
        return;
    }
    let jmap = seeder::jmap::Jmap::connect(&url, &user, &password).expect("connect remote");
    let account_id = jmap.account_id.clone();
    println!("\n=== remote {url} ===");

    // Advertised limits, plus a raw round-trip baseline that involves no
    // export logic at all: 10 sequential blob uploads of one small message.
    let probe = vandelay::jmap::http::HttpClient::new(
        Auth::Basic {
            user: user.clone(),
            password: password.clone(),
        },
        vandelay::jmap::http::RetryPolicy::new(5),
        true,
    );
    let session = vandelay::jmap::session::Session::discover(&probe, &url).expect("discover");
    let limits = session.core_limits().expect("core limits");
    println!(
        "limits: maxConcurrentRequests={} maxConcurrentUpload={} maxCallsInRequest={} maxObjectsInSet={}",
        limits.max_concurrent_requests,
        limits.max_concurrent_upload,
        limits.max_calls_in_request,
        limits.max_objects_in_set,
    );
    probe.set_limits(&limits);
    let payload = b"From: a@b\r\nSubject: probe\r\nMessage-ID: <probe@x>\r\n\r\nx\r\n";
    let mut samples = Vec::new();
    for _ in 0..10 {
        let t = Instant::now();
        vandelay::jmap::blobxfer::upload_bytes(
            &probe,
            &session,
            &account_id,
            "message/rfc822",
            payload,
        )
        .expect("probe upload");
        samples.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "raw sequential upload round-trip: min={:.0}ms median={:.0}ms max={:.0}ms",
        samples[0],
        samples[samples.len() / 2],
        samples[samples.len() - 1],
    );

    for threads in thread_settings() {
        let archive = tmp_archive(&format!("remote-t{threads}"));
        build_archive(&archive);
        let common = CommonConfig {
            archive: archive.clone(),
            threads,
            dry_run: false,
            max_retries: 5,
            allow_invalid_certs: true,
            logger: Logger::from_flags(false, bench_verbose()),
        };
        let config = ExportConfig {
            connect: ConnectConfig {
                url: url.clone(),
                auth: Auth::Basic {
                    user: user.clone(),
                    password: password.clone(),
                },
                account: AccountSelector::Id(account_id.clone()),
            },
            objects: Some(vec![
                vandelay::types::ObjectType::Mailbox,
                vandelay::types::ObjectType::Email,
            ]),
            prune: false,
            yes: true,
        };
        let started = Instant::now();
        let summary = sync::export::run(common, config).expect("remote export");
        let elapsed = started.elapsed();
        let email = summary
            .per_type
            .iter()
            .find(|(t, _)| *t == "Email")
            .map(|(_, c)| c.clone())
            .expect("email counts");
        println!(
            "threads={:<3} {:>7.1}s  {:>6.1} mails/s  created={} skipped={} failed={} retries={} retry_after={}",
            threads,
            elapsed.as_secs_f64(),
            email.created as f64 / elapsed.as_secs_f64(),
            email.created,
            email.skipped,
            email.failed,
            summary.retries_observed,
            summary.retry_after_sleeps,
        );
        let _ = std::fs::remove_file(&archive);
    }
}

#[test]
#[ignore = "requires Docker"]
fn bench_export_thousand_emails() {
    let stalwart = shared_stalwart();
    let session = stalwart.fetch_jmap_session().expect("session");
    println!("\n=== server limits ===\n{}", core_limits(&session));

    let admin = seeder::admin::Admin::connect(
        base_url(),
        seeder::ADMIN_USER,
        seeder::ADMIN_PASSWORD,
    )
    .expect("admin connect");
    admin
        .teardown_domain(seeder::DOMAIN)
        .expect("teardown domain");
    admin.invalidate_caches().expect("invalidate");
    let domain_id = admin
        .ensure_domain(seeder::DOMAIN)
        .expect("ensure domain");
    admin.invalidate_caches().expect("invalidate");

    let mut runs = Vec::new();
    for threads in thread_settings() {
        let localpart = format!("bench{threads}");
        let target_id = fresh_target(&admin, &domain_id, &localpart);
        let run = run_export(threads, &target_id, &localpart);
        println!(
            "threads={:<3} {:>7.1}s  {:>6.1} mails/s  created={} failed={} retries={} retry_after={}",
            run.threads,
            run.elapsed.as_secs_f64(),
            run.created as f64 / run.elapsed.as_secs_f64(),
            run.created,
            run.failed,
            run.retries,
            run.retry_after_sleeps,
        );
        runs.push(run);
    }

    println!("\n=== summary ===");
    let base = &runs[0];
    for run in &runs {
        let speedup = base.elapsed.as_secs_f64() / run.elapsed.as_secs_f64();
        println!(
            "threads={:<3} speedup vs threads=1: {speedup:>4.2}x",
            run.threads
        );
    }
    let _ = seeder::teardown(base_url());

    for run in &runs {
        assert_eq!(run.created, mails() as u64, "threads={}", run.threads);
        assert_eq!(run.failed, 0, "threads={}", run.threads);
    }
}
