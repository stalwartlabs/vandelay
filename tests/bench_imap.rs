/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! IMAP APPEND throughput against the same remote server as `bench_transport`.
//!
//! IMAP does not go through the JMAP blob upload store, so it is not subject to
//! the `jmap.protocol.upload.quota` ceiling that caps `Email/import`.

use std::time::Instant;

use vandelay::imap::transport::Connector;
use vandelay::imap::{ConnectMode, ImapClient};
use vandelay::logging::Logger;

fn dotenv() -> std::collections::HashMap<String, String> {
    let raw = std::fs::read_to_string(".env").expect("read .env");
    raw.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect()
}

fn mails() -> usize {
    std::env::var("BENCH_MAILS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100)
}

fn host() -> String {
    let raw = dotenv().get("JMAP_SERVER").expect("JMAP_SERVER").clone();
    raw.trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_owned()
}

fn message(tag: &str, i: usize) -> Vec<u8> {
    format!(
        "From: bench@vandelay.org\r\nTo: sink@vandelay.org\r\n\
         Subject: imap {tag} {i}\r\nMessage-ID: <imap-{tag}-{i}@vandelay.org>\r\n\
         Date: Wed, 01 Jan 2020 00:00:00 +0000\r\n\r\nbody {i}\r\n"
    )
    .into_bytes()
}

fn connect() -> ImapClient {
    let env = dotenv();
    let connector = Connector::new(false).expect("connector");
    let mut client = ImapClient::connect(
        &connector,
        &host(),
        993,
        ConnectMode::ImplicitTls,
        Logger::from_flags(false, 0),
    )
    .expect("imap connect");
    let user = env.get("JMAP_ACCOUNT").expect("JMAP_ACCOUNT");
    let password = env.get("JMAP_PASSWORD").expect("JMAP_PASSWORD");
    println!("imap capabilities: {}", client.capabilities.iter().cloned().collect::<Vec<_>>().join(" "));
    if let Err(e) = client.authenticate_plain(user, password) {
        panic!("AUTHENTICATE PLAIN failed: {e}");
    }
    client
}

fn ensure_folder(client: &mut ImapClient, name: &str) {
    let _ = client.run(&format!("CREATE \"{name}\""));
}

/// One APPEND command per message, strict request/response.
fn append_serial(client: &mut ImapClient, folder: &str, tag: &str, n: usize) -> usize {
    let mut ok = 0;
    for i in 1..=n {
        let body = message(tag, i);
        let cmd = format!(
            "APPEND \"{folder}\" {{{}+}}\r\n{}",
            body.len(),
            String::from_utf8_lossy(&body)
        );
        if client.run(&cmd).is_ok() {
            ok += 1;
        }
    }
    ok
}

/// MULTIAPPEND (RFC 3502): every message of a batch in a single command.
fn append_multi(
    client: &mut ImapClient,
    folder: &str,
    tag: &str,
    n: usize,
    batch: usize,
) -> usize {
    let mut ok = 0;
    let mut i = 1;
    while i <= n {
        let upper = (i + batch - 1).min(n);
        let mut cmd = format!("APPEND \"{folder}\"");
        for j in i..=upper {
            let body = message(tag, j);
            cmd.push_str(&format!(
                " {{{}+}}\r\n{}",
                body.len(),
                String::from_utf8_lossy(&body)
            ));
        }
        match client.run(&cmd) {
            Ok(_) => ok += upper - i + 1,
            Err(e) => panic!("MULTIAPPEND failed: {e}"),
        }
        i = upper + 1;
    }
    ok
}

#[test]
#[ignore = "writes to a remote server configured in .env"]
fn bench_imap_append() {
    let n = mails();
    let mut client = connect();
    let caps: Vec<&str> = ["LITERAL+", "MULTIAPPEND", "COMPRESS=DEFLATE", "CONDSTORE"]
        .into_iter()
        .filter(|c| client.has_capability(c))
        .collect();
    println!("\n=== IMAP APPEND, {n} mails ===");
    println!("capabilities present: {}", caps.join(" "));

    let folder = "vandelay-bench-imap";
    ensure_folder(&mut client, folder);
    let pid = std::process::id();

    let started = Instant::now();
    let ok = append_serial(&mut client, folder, &format!("s{pid}"), n);
    let elapsed = started.elapsed();
    println!(
        "serial APPEND            {:>7.1}s  {:>7.1} mails/s   appended={ok}/{n}",
        elapsed.as_secs_f64(),
        ok as f64 / elapsed.as_secs_f64()
    );

    if client.has_capability("MULTIAPPEND") {
        for batch in [50usize, 200] {
            let started = Instant::now();
            let ok = append_multi(&mut client, folder, &format!("m{batch}{pid}"), n, batch);
            let elapsed = started.elapsed();
            println!(
                "MULTIAPPEND batch={batch:<4}    {:>7.1}s  {:>7.1} mails/s   appended={ok}/{n}",
                elapsed.as_secs_f64(),
                ok as f64 / elapsed.as_secs_f64()
            );
        }
    }
    let _ = client.logout();
}
