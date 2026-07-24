/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! Transport comparison for email import against a remote JMAP server.
//!
//! Deliberately does NOT use `vandelay::jmap` — it drives ureq directly so the
//! connection-pool size, request batching and call layout can be varied
//! independently of production code. Each mode imports the same corpus of
//! freshly generated messages and reports mails/second.
//!
//! Configure the target through `.env` (JMAP_SERVER/JMAP_ACCOUNT/JMAP_PASSWORD).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{Map, Value, json};
use ureq::Agent;
use ureq::config::RedirectAuthHeaders;
use ureq::tls::{TlsConfig, TlsProvider};

const CORE: &str = "urn:ietf:params:jmap:core";
const MAIL: &str = "urn:ietf:params:jmap:mail";
const BLOB: &str = "urn:ietf:params:jmap:blob";

fn mails() -> usize {
    std::env::var("BENCH_MAILS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100)
}

fn dotenv() -> std::collections::HashMap<String, String> {
    let raw = std::fs::read_to_string(".env").expect("read .env");
    raw.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect()
}

fn agent(pool_per_host: usize) -> Agent {
    let tls = TlsConfig::builder()
        .provider(TlsProvider::Rustls)
        .unversioned_rustls_crypto_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build();
    Agent::config_builder()
        .tls_config(tls)
        .http_status_as_error(false)
        .redirect_auth_headers(RedirectAuthHeaders::SameHost)
        .max_idle_connections(pool_per_host * 4)
        .max_idle_connections_per_host(pool_per_host)
        .build()
        .new_agent()
}

fn basic_header(user: &str, password: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))
    )
}

struct Target {
    api_url: String,
    upload_url: String,
    account_id: String,
    auth: String,
    ws_url: Option<String>,
}

fn discover(agent: &Agent, base: &str, auth: &str) -> Target {
    let mut resp = agent
        .get(&format!("{base}/.well-known/jmap"))
        .header("Authorization", auth)
        .call()
        .expect("session request");
    let session: Value = serde_json::from_str(&resp.body_mut().read_to_string().expect("body"))
        .expect("session json");
    let account_id = session["primaryAccounts"][MAIL]
        .as_str()
        .expect("primary mail account")
        .to_owned();
    let ws_url = session["capabilities"]["urn:ietf:params:jmap:websocket"]["url"]
        .as_str()
        .map(str::to_owned);
    Target {
        api_url: session["apiUrl"].as_str().expect("apiUrl").to_owned(),
        upload_url: session["uploadUrl"]
            .as_str()
            .expect("uploadUrl")
            .replace("{accountId}", &account_id),
        account_id,
        auth: auth.to_owned(),
        ws_url,
    }
}

/// Count of 429/`overQuota` backoffs across a whole mode, so a mode that only
/// looks slow because the server throttled it can be told apart from one that
/// is genuinely latency-bound.
static THROTTLED: AtomicUsize = AtomicUsize::new(0);

fn backoff(attempt: u32) {
    THROTTLED.fetch_add(1, Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(500 * (1 << attempt.min(5)) as u64));
}

impl Target {
    /// POSTs and retries while the server answers 429, so throttling shows up
    /// as elapsed time rather than as a panic.
    fn post(&self, agent: &Agent, url: &str, ct: &str, payload: &[u8]) -> (u16, String) {
        for attempt in 0..8 {
            let mut resp = agent
                .post(url)
                .header("Authorization", &self.auth)
                .header("Content-Type", ct)
                .send(payload)
                .expect("request");
            let status = resp.status().as_u16();
            let text = resp.body_mut().read_to_string().expect("body");
            // 429 = blob quota; 400 + urn:...:error:limit = concurrency cap.
            let limited = status == 429
                || (status == 400 && text.contains("jmap:error:limit"));
            if !limited {
                return (status, text);
            }
            backoff(attempt);
        }
        panic!("{url}: still 429 after 8 attempts");
    }

    fn api(&self, agent: &Agent, using: &[&str], calls: Value) -> Value {
        let body = json!({ "using": using, "methodCalls": calls });
        let payload = serde_json::to_vec(&body).expect("encode");
        for attempt in 0..8 {
            let (status, text) =
                self.post(agent, &self.api_url, "application/json", &payload);
            assert_eq!(status, 200, "api status {status}: {text}");
            let v: Value = serde_json::from_str(&text).expect("api json");
            // Method-level quota rejection: same condition, different envelope.
            let over = v["methodResponses"][0][1]["notCreated"]
                .as_object()
                .is_some_and(|nc| nc.values().any(|e| e["type"] == "overQuota"));
            if !over {
                return v;
            }
            backoff(attempt);
        }
        panic!("api: still overQuota after 8 attempts");
    }

    fn upload(&self, agent: &Agent, bytes: &[u8]) -> String {
        let (status, text) = self.post(agent, &self.upload_url, "message/rfc822", bytes);
        let v: Value = serde_json::from_str(&text)
            .unwrap_or_else(|_| panic!("upload status {status}: {text}"));
        match v["blobId"].as_str() {
            Some(id) => id.to_owned(),
            None => panic!("upload status {status}: {text}"),
        }
    }
}

/// Distinct corpus per mode so nothing is skipped as already-present.
fn corpus(tag: &str, n: usize) -> Vec<Vec<u8>> {
    (1..=n)
        .map(|i| {
            format!(
                "From: bench@vandelay.org\r\nTo: sink@vandelay.org\r\n\
                 Subject: transport {tag} {i}\r\nMessage-ID: <tx-{tag}-{i}@vandelay.org>\r\n\
                 Date: Wed, 01 Jan 2020 00:00:00 +0000\r\n\r\nbody {i}\r\n"
            )
            .into_bytes()
        })
        .collect()
}

fn email_obj(blob: &str, mailbox: &str) -> Value {
    json!({
        "blobId": blob,
        "mailboxIds": { mailbox: true },
        "keywords": {},
        "receivedAt": "2020-01-01T00:00:00Z",
    })
}

fn ensure_mailbox(target: &Target, agent: &Agent, name: &str) -> String {
    let resp = target.api(
        agent,
        &[CORE, MAIL],
        json!([["Mailbox/query", {
            "accountId": target.account_id,
            "filter": { "name": name }
        }, "q"]]),
    );
    if let Some(id) = resp["methodResponses"][0][1]["ids"][0].as_str() {
        return id.to_owned();
    }
    let resp = target.api(
        agent,
        &[CORE, MAIL],
        json!([["Mailbox/set", {
            "accountId": target.account_id,
            "create": { "m": { "name": name } }
        }, "c"]]),
    );
    resp["methodResponses"][0][1]["created"]["m"]["id"]
        .as_str()
        .expect("created mailbox")
        .to_owned()
}

fn count_created(resp: &Value) -> usize {
    resp["methodResponses"]
        .as_array()
        .map(|calls| {
            calls
                .iter()
                .filter_map(|c| c[1]["created"].as_object())
                .map(Map::len)
                .sum()
        })
        .unwrap_or(0)
}

/// Current production shape: one HTTP upload plus one single-email
/// `Email/import` per message.
fn mode_per_mail(
    target: &Target,
    agent: &Agent,
    mailbox: &str,
    corpus: &[Vec<u8>],
    workers: usize,
) -> usize {
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= corpus.len() {
                        break;
                    }
                    let blob = target.upload(agent, &corpus[i]);
                    let resp = target.api(
                        agent,
                        &[CORE, MAIL],
                        json!([["Email/import", {
                            "accountId": target.account_id,
                            "emails": { "e": email_obj(&blob, mailbox) }
                        }, "i"]]),
                    );
                    done.fetch_add(count_created(&resp), Ordering::Relaxed);
                }
            });
        }
    });
    done.load(Ordering::Relaxed)
}

/// `Blob/upload` (RFC 9404) batches N blobs into one JMAP request, then one
/// `Email/import` imports all N. Two HTTP requests per batch instead of per mail.
fn mode_batched(
    target: &Target,
    agent: &Agent,
    mailbox: &str,
    corpus: &[Vec<u8>],
    batch: usize,
    workers: usize,
) -> usize {
    let chunks: Vec<&[Vec<u8>]> = corpus.chunks(batch).collect();
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let c = next.fetch_add(1, Ordering::Relaxed);
                    if c >= chunks.len() {
                        break;
                    }
                    let chunk = chunks[c];
                    let mut create = Map::new();
                    for (i, bytes) in chunk.iter().enumerate() {
                        create.insert(
                            format!("b{i}"),
                            json!({
                                "data": [ { "data:asBase64":
                                    base64::engine::general_purpose::STANDARD.encode(bytes) } ],
                                "type": "message/rfc822"
                            }),
                        );
                    }
                    let resp = target.api(
                        agent,
                        &[CORE, BLOB],
                        json!([["Blob/upload", {
                            "accountId": target.account_id,
                            "create": Value::Object(create)
                        }, "u"]]),
                    );
                    let created = resp["methodResponses"][0][1]["created"]
                        .as_object()
                        .cloned()
                        .unwrap_or_else(|| {
                            panic!("Blob/upload failed: {}", resp["methodResponses"][0][1])
                        });
                    let mut emails = Map::new();
                    for (key, v) in &created {
                        let blob = v["id"].as_str().expect("blob id");
                        emails.insert(key.clone(), email_obj(blob, mailbox));
                    }
                    let resp = target.api(
                        agent,
                        &[CORE, MAIL],
                        json!([["Email/import", {
                            "accountId": target.account_id,
                            "emails": Value::Object(emails)
                        }, "i"]]),
                    );
                    done.fetch_add(count_created(&resp), Ordering::Relaxed);
                }
            });
        }
    });
    done.load(Ordering::Relaxed)
}

type WsSocket = tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>;

/// JMAP over WebSocket, RFC 8887. One socket, request/response framed as JSON
/// text messages correlated by `id`.
fn ws_connect(url: &str, auth: &str) -> WsSocket {
    use tungstenite::client::IntoClientRequest;
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
    let mut req = url.into_client_request().expect("ws request");
    req.headers_mut()
        .insert("Authorization", auth.parse().expect("auth header"));
    req.headers_mut()
        .insert("Sec-WebSocket-Protocol", "jmap".parse().expect("subproto"));
    let (socket, _resp) = tungstenite::connect(req).expect("ws connect");
    socket
}

fn ws_api(socket: &mut WsSocket, using: &[&str], calls: Value, id: &str) -> Value {
    let body = json!({
        "@type": "Request", "using": using, "methodCalls": calls, "id": id
    });
    socket
        .send(tungstenite::Message::Text(body.to_string().into()))
        .expect("ws send");
    loop {
        match socket.read().expect("ws read") {
            tungstenite::Message::Text(t) => {
                let v: Value = serde_json::from_str(&t).expect("ws json");
                if v["@type"] == "Response" {
                    return v;
                }
            }
            tungstenite::Message::Ping(p) => {
                let _ = socket.send(tungstenite::Message::Pong(p));
            }
            tungstenite::Message::Close(c) => panic!("ws closed: {c:?}"),
            _ => {}
        }
    }
}

fn percentiles(mut v: Vec<f64>) -> (f64, f64, f64) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (v[0], v[v.len() / 2], v[v.len() - 1])
}

/// Round-trip latency of a trivial API call over each transport. Consumes no
/// blob upload quota, so it isolates transport cost from server-side work.
#[test]
#[ignore = "talks to a remote server configured in .env"]
fn bench_transport_latency() {
    let env = dotenv();
    let raw = env.get("JMAP_SERVER").expect("JMAP_SERVER").clone();
    let base = if raw.starts_with("http") {
        raw
    } else {
        format!("https://{raw}")
    };
    let auth = basic_header(
        env.get("JMAP_ACCOUNT").expect("JMAP_ACCOUNT"),
        env.get("JMAP_PASSWORD").expect("JMAP_PASSWORD"),
    );
    let ag = agent(4);
    let target = discover(&ag, &base, &auth);
    let rounds = 30;
    let calls = json!([["Mailbox/get", {
        "accountId": target.account_id, "ids": []
    }, "g"]]);

    let mut http = Vec::new();
    for _ in 0..rounds {
        let t = Instant::now();
        target.api(&ag, &[CORE, MAIL], calls.clone());
        http.push(t.elapsed().as_secs_f64() * 1000.0);
    }

    let ws_url = target.ws_url.clone().expect("server advertises websocket");
    let mut socket = ws_connect(&ws_url, &auth);
    let mut ws = Vec::new();
    for i in 0..rounds {
        let t = Instant::now();
        ws_api(&mut socket, &[CORE, MAIL], calls.clone(), &format!("r{i}"));
        ws.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let _ = socket.close(None);

    // RFC 8887 allows many requests in flight on one socket. Send all, then read
    // all, instead of strict request/response ping-pong.
    let mut socket = ws_connect(&ws_url, &auth);
    let pipeline = 30;
    let started = Instant::now();
    for i in 0..pipeline {
        let body = json!({
            "@type": "Request", "using": [CORE, MAIL],
            "methodCalls": calls, "id": format!("p{i}")
        });
        socket
            .send(tungstenite::Message::Text(body.to_string().into()))
            .expect("ws send");
    }
    let mut seen = 0;
    while seen < pipeline {
        if let tungstenite::Message::Text(t) = socket.read().expect("ws read") {
            let v: Value = serde_json::from_str(&t).expect("ws json");
            if v["@type"] == "Response" {
                seen += 1;
            }
        }
    }
    let pipelined = started.elapsed();
    let _ = socket.close(None);

    let (hmin, hmed, hmax) = percentiles(http);
    let (wmin, wmed, wmax) = percentiles(ws);
    println!("\n=== API round-trip, {rounds} calls of Mailbox/get ids=[] ===");
    println!("HTTP/1.1 keepalive  min={hmin:.0}ms median={hmed:.0}ms max={hmax:.0}ms");
    println!("WebSocket (RFC8887) min={wmin:.0}ms median={wmed:.0}ms max={wmax:.0}ms");
    println!("websocket vs http:  {:.2}x per call", hmed / wmed);
    println!(
        "WebSocket pipelined {pipeline} in flight: {:.0}ms total = {:.1}ms/call, {:.1}x vs serial http",
        pipelined.as_secs_f64() * 1000.0,
        pipelined.as_secs_f64() * 1000.0 / pipeline as f64,
        hmed / (pipelined.as_secs_f64() * 1000.0 / pipeline as f64),
    );
}

/// A synthesised `Email/set` create. Unlike `Email/import` this needs no blob,
/// so it exercises the write path without touching the upload quota. Note it is
/// NOT byte-faithful — the server reassembles the MIME — so it is a transport
/// measurement, not a migration proposal.
fn set_create(mailbox: &str, tag: &str, i: usize) -> Value {
    json!({
        "mailboxIds": { mailbox: true },
        "from": [ { "email": "bench@vandelay.org" } ],
        "to": [ { "email": "sink@vandelay.org" } ],
        "subject": format!("write {tag} {i}"),
        "receivedAt": "2020-01-01T00:00:00Z",
        "bodyValues": { "1": { "value": format!("body {i}") } },
        "textBody": [ { "partId": "1", "type": "text/plain" } ],
    })
}

fn set_call(account: &str, creates: Map<String, Value>) -> Value {
    json!([["Email/set", { "accountId": account, "create": Value::Object(creates) }, "s"]])
}

/// Write throughput across transports, measured on `Email/set`.
#[test]
#[ignore = "writes to a remote server configured in .env"]
fn bench_write_paths() {
    let env = dotenv();
    let raw = env.get("JMAP_SERVER").expect("JMAP_SERVER").clone();
    let base = if raw.starts_with("http") {
        raw
    } else {
        format!("https://{raw}")
    };
    let auth = basic_header(
        env.get("JMAP_ACCOUNT").expect("JMAP_ACCOUNT"),
        env.get("JMAP_PASSWORD").expect("JMAP_PASSWORD"),
    );
    let ag = agent(8);
    let target = discover(&ag, &base, &auth);
    let mailbox = ensure_mailbox(&target, &ag, "vandelay-bench");
    let n = mails();
    let pid = std::process::id();
    println!("\n=== write paths, Email/set, {n} mails each ===");

    // HTTP, one create per request, W workers.
    for workers in [1usize, 4] {
        let tag = format!("h{workers}-{pid}");
        let next = AtomicUsize::new(0);
        let done = AtomicUsize::new(0);
        let started = Instant::now();
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= n {
                            break;
                        }
                        let mut c = Map::new();
                        c.insert("e".to_owned(), set_create(&mailbox, &tag, i));
                        let resp =
                            target.api(&ag, &[CORE, MAIL], set_call(&target.account_id, c));
                        done.fetch_add(count_created(&resp), Ordering::Relaxed);
                    }
                });
            }
        });
        report(
            &format!("HTTP 1/request, {workers} workers"),
            done.load(Ordering::Relaxed),
            n,
            started.elapsed(),
        );
    }

    // HTTP, batched creates per request, W workers.
    for (batch, workers) in [(50usize, 1usize), (50, 4)] {
        let tag = format!("hb{batch}x{workers}-{pid}");
        let chunks: Vec<(usize, usize)> = (0..n)
            .step_by(batch)
            .map(|s| (s, (s + batch).min(n)))
            .collect();
        let next = AtomicUsize::new(0);
        let done = AtomicUsize::new(0);
        let started = Instant::now();
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    loop {
                        let c = next.fetch_add(1, Ordering::Relaxed);
                        if c >= chunks.len() {
                            break;
                        }
                        let (lo, hi) = chunks[c];
                        let mut creates = Map::new();
                        for i in lo..hi {
                            creates.insert(format!("e{i}"), set_create(&mailbox, &tag, i));
                        }
                        let resp =
                            target.api(&ag, &[CORE, MAIL], set_call(&target.account_id, creates));
                        done.fetch_add(count_created(&resp), Ordering::Relaxed);
                    }
                });
            }
        });
        report(
            &format!("HTTP batch={batch}, {workers} workers"),
            done.load(Ordering::Relaxed),
            n,
            started.elapsed(),
        );
    }

    let ws_url = target.ws_url.clone().expect("websocket url");

    // WebSocket, one create per request, K requests in flight.
    for inflight in [1usize, 8, 32] {
        let tag = format!("w{inflight}-{pid}");
        let mut socket = ws_connect(&ws_url, &auth);
        let started = Instant::now();
        let mut sent = 0;
        let mut acked = 0;
        let mut created = 0;
        while acked < n {
            while sent < n && sent - acked < inflight {
                let mut c = Map::new();
                c.insert("e".to_owned(), set_create(&mailbox, &tag, sent));
                let body = json!({
                    "@type": "Request", "using": [CORE, MAIL],
                    "methodCalls": set_call(&target.account_id, c), "id": format!("w{sent}")
                });
                socket
                    .send(tungstenite::Message::Text(body.to_string().into()))
                    .expect("ws send");
                sent += 1;
            }
            if let tungstenite::Message::Text(t) = socket.read().expect("ws read") {
                let v: Value = serde_json::from_str(&t).expect("ws json");
                if v["@type"] == "Response" {
                    created += count_created(&v);
                    acked += 1;
                }
            }
        }
        let _ = socket.close(None);
        report(
            &format!("WebSocket 1/request, {inflight} in flight"),
            created,
            n,
            started.elapsed(),
        );
    }

    // WebSocket, batched creates, K requests in flight.
    for (batch, inflight) in [(50usize, 4usize), (200, 4)] {
        let tag = format!("wb{batch}x{inflight}-{pid}");
        let chunks: Vec<(usize, usize)> = (0..n)
            .step_by(batch)
            .map(|s| (s, (s + batch).min(n)))
            .collect();
        let mut socket = ws_connect(&ws_url, &auth);
        let started = Instant::now();
        let (mut sent, mut acked, mut created) = (0usize, 0usize, 0usize);
        while acked < chunks.len() {
            while sent < chunks.len() && sent - acked < inflight {
                let (lo, hi) = chunks[sent];
                let mut creates = Map::new();
                for i in lo..hi {
                    creates.insert(format!("e{i}"), set_create(&mailbox, &tag, i));
                }
                let body = json!({
                    "@type": "Request", "using": [CORE, MAIL],
                    "methodCalls": set_call(&target.account_id, creates),
                    "id": format!("wb{sent}")
                });
                socket
                    .send(tungstenite::Message::Text(body.to_string().into()))
                    .expect("ws send");
                sent += 1;
            }
            if let tungstenite::Message::Text(t) = socket.read().expect("ws read") {
                let v: Value = serde_json::from_str(&t).expect("ws json");
                if v["@type"] == "Response" {
                    created += count_created(&v);
                    acked += 1;
                }
            }
        }
        let _ = socket.close(None);
        report(
            &format!("WebSocket batch={batch}, {inflight} in flight"),
            created,
            n,
            started.elapsed(),
        );
    }
}

/// One WebSocket, `inflight` chunks in flight. Each chunk is a two-step
/// pipeline: `Blob/upload` for the whole batch, then `Email/import` for the
/// blob ids that came back. Steps of different chunks overlap freely.
fn mode_ws_pipelined(
    target: &Target,
    ws_url: &str,
    mailbox: &str,
    corpus: &[Vec<u8>],
    batch: usize,
    inflight: usize,
) -> usize {
    let chunks: Vec<&[Vec<u8>]> = corpus.chunks(batch).collect();
    let mut socket = ws_connect(ws_url, &target.auth);
    let mut next_chunk = 0usize;
    let mut open = 0usize;
    let mut imported = 0usize;
    let mut finished = 0usize;

    let send_upload = |socket: &mut WsSocket, idx: usize, chunk: &[Vec<u8>]| {
        let mut create = Map::new();
        for (i, bytes) in chunk.iter().enumerate() {
            create.insert(
                format!("b{i}"),
                json!({
                    "data": [ { "data:asBase64":
                        base64::engine::general_purpose::STANDARD.encode(bytes) } ],
                    "type": "message/rfc822"
                }),
            );
        }
        let body = json!({
            "@type": "Request", "using": [CORE, BLOB],
            "methodCalls": [["Blob/upload", {
                "accountId": target.account_id, "create": Value::Object(create)
            }, "u"]],
            "id": format!("u{idx}")
        });
        socket
            .send(tungstenite::Message::Text(body.to_string().into()))
            .expect("ws send upload");
    };

    while next_chunk < chunks.len() && open < inflight {
        send_upload(&mut socket, next_chunk, chunks[next_chunk]);
        next_chunk += 1;
        open += 1;
    }

    while finished < chunks.len() {
        let msg = socket.read().expect("ws read");
        let tungstenite::Message::Text(t) = msg else {
            continue;
        };
        let v: Value = serde_json::from_str(&t).expect("ws json");
        if v["@type"] != "Response" {
            continue;
        }
        let id = v["requestId"].as_str().unwrap_or_default().to_owned();
        if let Some(idx) = id.strip_prefix('u') {
            let created = v["methodResponses"][0][1]["created"]
                .as_object()
                .cloned()
                .unwrap_or_else(|| panic!("Blob/upload failed: {}", v["methodResponses"][0][1]));
            let mut emails = Map::new();
            for (key, blob) in &created {
                emails.insert(
                    key.clone(),
                    email_obj(blob["id"].as_str().expect("blob id"), mailbox),
                );
            }
            let body = json!({
                "@type": "Request", "using": [CORE, MAIL],
                "methodCalls": [["Email/import", {
                    "accountId": target.account_id, "emails": Value::Object(emails)
                }, "i"]],
                "id": format!("i{idx}")
            });
            socket
                .send(tungstenite::Message::Text(body.to_string().into()))
                .expect("ws send import");
        } else if id.starts_with('i') {
            imported += count_created(&v);
            finished += 1;
            open -= 1;
            if next_chunk < chunks.len() {
                send_upload(&mut socket, next_chunk, chunks[next_chunk]);
                next_chunk += 1;
                open += 1;
            }
        }
    }
    let _ = socket.close(None);
    imported
}

/// WebSocket import sweep: batch size against requests in flight.
#[test]
#[ignore = "writes to a remote server configured in .env"]
fn bench_ws_import() {
    let env = dotenv();
    let raw = env.get("JMAP_SERVER").expect("JMAP_SERVER").clone();
    let base = if raw.starts_with("http") {
        raw
    } else {
        format!("https://{raw}")
    };
    let auth = basic_header(
        env.get("JMAP_ACCOUNT").expect("JMAP_ACCOUNT"),
        env.get("JMAP_PASSWORD").expect("JMAP_PASSWORD"),
    );
    let ag = agent(8);
    let target = discover(&ag, &base, &auth);
    let mailbox = ensure_mailbox(&target, &ag, "vandelay-bench");
    let ws_url = target.ws_url.clone().expect("websocket url");
    let n = mails();
    let pid = std::process::id();
    println!("\n=== websocket Blob/upload + Email/import, {n} mails each ===");

    for (batch, inflight) in [
        (10usize, 4usize),
        (50, 2),
        (50, 4),
        (50, 8),
        (100, 4),
        (100, 8),
        (200, 4),
        (200, 8),
    ] {
        let body = corpus(&format!("ws{batch}x{inflight}-{pid}"), n);
        let started = Instant::now();
        let ok = mode_ws_pipelined(&target, &ws_url, &mailbox, &body, batch, inflight);
        report(
            &format!("WS batch={batch:<4} inflight={inflight}"),
            ok,
            n,
            started.elapsed(),
        );
    }
}

fn report(label: &str, imported: usize, expected: usize, elapsed: Duration) {
    println!(
        "{label:<44} {:>7.1}s  {:>7.1} mails/s   imported={imported}/{expected}",
        elapsed.as_secs_f64(),
        imported as f64 / elapsed.as_secs_f64(),
    );
}

#[test]
#[ignore = "writes to a remote server configured in .env"]
fn bench_transport_modes() {
    let env = dotenv();
    let raw = env.get("JMAP_SERVER").expect("JMAP_SERVER").clone();
    let base = if raw.starts_with("http") {
        raw
    } else {
        format!("https://{raw}")
    };
    let user = env.get("JMAP_ACCOUNT").expect("JMAP_ACCOUNT");
    let password = env.get("JMAP_PASSWORD").expect("JMAP_PASSWORD");
    let auth = basic_header(user, password);

    let setup = agent(4);
    let target = discover(&setup, &base, &auth);
    let mailbox = ensure_mailbox(&target, &setup, "vandelay-bench");
    let n = mails();
    println!(
        "\n=== transport modes, {n} mails, websocket={} ===",
        target.ws_url.as_deref().unwrap_or("unsupported")
    );

    let cases: Vec<(String, Box<dyn Fn(&Agent, &[Vec<u8>]) -> usize>, usize)> = vec![
        (
            "A upload+import per mail, 4 workers".to_owned(),
            Box::new(|a, c| mode_per_mail(&target, a, &mailbox, c, 4)),
            8,
        ),
        (
            "B Blob/upload batch=20, 4 workers".to_owned(),
            Box::new(|a, c| mode_batched(&target, a, &mailbox, c, 20, 4)),
            8,
        ),
        (
            "C Blob/upload batch=50, 4 workers".to_owned(),
            Box::new(|a, c| mode_batched(&target, a, &mailbox, c, 50, 4)),
            8,
        ),
        (
            "D Blob/upload batch=100, 4 workers".to_owned(),
            Box::new(|a, c| mode_batched(&target, a, &mailbox, c, 100, 4)),
            8,
        ),
        (
            "E Blob/upload batch=200, 4 workers".to_owned(),
            Box::new(|a, c| mode_batched(&target, a, &mailbox, c, 200, 4)),
            8,
        ),
        (
            "F Blob/upload batch=100, 2 workers".to_owned(),
            Box::new(|a, c| mode_batched(&target, a, &mailbox, c, 100, 2)),
            8,
        ),
    ];

    let selected = std::env::var("BENCH_MODES").unwrap_or_else(|_| "ABCDEFGH".to_owned());
    for (i, (label, run, pool)) in cases.iter().enumerate() {
        let letter = label.chars().next().unwrap();
        if !selected.contains(letter) {
            continue;
        }
        let ag = agent(*pool);
        let body = corpus(&format!("{}-{i}", std::process::id()), n);
        THROTTLED.store(0, Ordering::Relaxed);
        let started = Instant::now();
        let imported = run(&ag, &body);
        let throttled = THROTTLED.load(Ordering::Relaxed);
        report(label, imported, n, started.elapsed());
        if throttled > 0 {
            println!("    (throttled {throttled}x — server quota, not latency)");
        }
    }
}

/// Is `maxConcurrentRequests` scoped to the account or to the connection?
/// Eight workers, each with its OWN agent and therefore its own TCP connection.
/// If the cap were per connection this would pass; if per account it must fail.
#[test]
#[ignore = "talks to a remote server configured in .env"]
fn concurrency_limit_scope() {
    let env = dotenv();
    let raw = env.get("JMAP_SERVER").expect("JMAP_SERVER").clone();
    let base = if raw.starts_with("http") {
        raw
    } else {
        format!("https://{raw}")
    };
    let auth = basic_header(
        env.get("JMAP_ACCOUNT").expect("JMAP_ACCOUNT"),
        env.get("JMAP_PASSWORD").expect("JMAP_PASSWORD"),
    );
    let setup = agent(4);
    let target = discover(&setup, &base, &auth);

    for workers in [4usize, 8, 12] {
        let rejected = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    // One agent per worker => one dedicated connection per worker.
                    let own = agent(1);
                    let body = json!({
                        "using": [CORE, MAIL],
                        "methodCalls": [["Email/query", {
                            "accountId": target.account_id, "limit": 50
                        }, "q"]]
                    });
                    let payload = serde_json::to_vec(&body).unwrap();
                    for _ in 0..10 {
                        let mut resp = own
                            .post(&target.api_url)
                            .header("Authorization", &target.auth)
                            .header("Content-Type", "application/json")
                            .send(payload.as_slice())
                            .expect("request");
                        let status = resp.status().as_u16();
                        let text = resp.body_mut().read_to_string().unwrap_or_default();
                        if status == 400 && text.contains("jmap:error:limit") {
                            rejected.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        });
        println!(
            "{workers} workers, {workers} separate connections: {} of {} requests rejected with maxConcurrentRequests",
            rejected.load(Ordering::Relaxed),
            workers * 10
        );
    }
}

/// Reports what the server actually enforces, as opposed to what it advertises:
/// the concurrency ceiling, then how many blobs fit in one quota window.
#[test]
#[ignore = "talks to a remote server configured in .env"]
fn verify_effective_limits() {
    let env = dotenv();
    let raw = env.get("JMAP_SERVER").expect("JMAP_SERVER").clone();
    let base = if raw.starts_with("http") {
        raw
    } else {
        format!("https://{raw}")
    };
    let auth = basic_header(
        env.get("JMAP_ACCOUNT").expect("JMAP_ACCOUNT"),
        env.get("JMAP_PASSWORD").expect("JMAP_PASSWORD"),
    );
    let setup = agent(8);
    let target = discover(&setup, &base, &auth);

    println!("\n=== effective concurrency (Email/query, no uploads) ===");
    for workers in [4usize, 8, 16, 24, 32] {
        let rejected = AtomicUsize::new(0);
        let ok = AtomicUsize::new(0);
        let started = Instant::now();
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    let own = agent(1);
                    let body = json!({
                        "using": [CORE, MAIL],
                        "methodCalls": [["Email/query", {
                            "accountId": target.account_id, "limit": 20
                        }, "q"]]
                    });
                    let payload = serde_json::to_vec(&body).unwrap();
                    for _ in 0..10 {
                        let mut resp = own
                            .post(&target.api_url)
                            .header("Authorization", &target.auth)
                            .header("Content-Type", "application/json")
                            .send(payload.as_slice())
                            .expect("request");
                        let status = resp.status().as_u16();
                        let text = resp.body_mut().read_to_string().unwrap_or_default();
                        if status == 400 && text.contains("jmap:error:limit") {
                            rejected.fetch_add(1, Ordering::Relaxed);
                        } else if status == 200 {
                            ok.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        });
        let elapsed = started.elapsed().as_secs_f64();
        println!(
            "  {workers:>2} concurrent: {:>3} ok, {:>3} rejected, {:>5.1} req/s",
            ok.load(Ordering::Relaxed),
            rejected.load(Ordering::Relaxed),
            ok.load(Ordering::Relaxed) as f64 / elapsed,
        );
    }

    println!("\n=== blob upload quota depth (uploads until refusal) ===");
    let payload = b"From: q@x\r\nSubject: quota depth\r\nMessage-ID: <qd@x>\r\n\r\nx\r\n";
    let mut accepted = 0usize;
    let mut bytes = 0usize;
    let started = Instant::now();
    for _ in 0..5000 {
        let mut resp = setup
            .post(&target.upload_url)
            .header("Authorization", &target.auth)
            .header("Content-Type", "message/rfc822")
            .send(payload.as_slice())
            .expect("upload");
        let status = resp.status().as_u16();
        let _ = resp.body_mut().read_to_string();
        if status != 200 {
            break;
        }
        accepted += 1;
        bytes += payload.len();
    }
    println!(
        "  {accepted} blobs / {bytes} bytes accepted in {:.0}s before refusal",
        started.elapsed().as_secs_f64()
    );
}

/// Splits the import into its two phases so the cost can be attributed:
/// blob creation first, then `Email/import` over blobs that already exist.
#[test]
#[ignore = "writes to a remote server configured in .env"]
fn bench_import_phases() {
    let env = dotenv();
    let raw = env.get("JMAP_SERVER").expect("JMAP_SERVER").clone();
    let base = if raw.starts_with("http") {
        raw
    } else {
        format!("https://{raw}")
    };
    let auth = basic_header(
        env.get("JMAP_ACCOUNT").expect("JMAP_ACCOUNT"),
        env.get("JMAP_PASSWORD").expect("JMAP_PASSWORD"),
    );
    let ag = agent(16);
    let target = discover(&ag, &base, &auth);
    let mailbox = ensure_mailbox(&target, &ag, "vandelay-bench");
    let n = mails();
    let pid = std::process::id();

    for workers in [4usize, 8, 16] {
        let body = corpus(&format!("ph{workers}-{pid}"), n);
        let blobs: Vec<String> = Vec::with_capacity(n);
        let blobs = std::sync::Mutex::new(blobs);
        let next = AtomicUsize::new(0);

        // Phase 1: create every blob, batched through Blob/upload.
        let started = Instant::now();
        let batch = std::env::var("BENCH_BATCH")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(50usize);
        let chunks: Vec<&[Vec<u8>]> = body.chunks(batch).collect();
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    loop {
                        let c = next.fetch_add(1, Ordering::Relaxed);
                        if c >= chunks.len() {
                            break;
                        }
                        let mut create = Map::new();
                        for (i, bytes) in chunks[c].iter().enumerate() {
                            create.insert(
                                format!("b{c}_{i}"),
                                json!({
                                    "data": [ { "data:asBase64":
                                        base64::engine::general_purpose::STANDARD.encode(bytes) } ],
                                    "type": "message/rfc822"
                                }),
                            );
                        }
                        let resp = target.api(
                            &ag,
                            &[CORE, BLOB],
                            json!([["Blob/upload", {
                                "accountId": target.account_id, "create": Value::Object(create)
                            }, "u"]]),
                        );
                        let created = resp["methodResponses"][0][1]["created"]
                            .as_object()
                            .cloned()
                            .unwrap_or_default();
                        let mut guard = blobs.lock().unwrap();
                        for v in created.values() {
                            if let Some(id) = v["id"].as_str() {
                                guard.push(id.to_owned());
                            }
                        }
                    }
                });
            }
        });
        let upload_time = started.elapsed();
        let ids = blobs.into_inner().unwrap();

        // Phase 2: import those blobs, nothing else.
        let idx = AtomicUsize::new(0);
        let imported = AtomicUsize::new(0);
        let groups: Vec<&[String]> = ids.chunks(batch).collect();
        let started = Instant::now();
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    loop {
                        let g = idx.fetch_add(1, Ordering::Relaxed);
                        if g >= groups.len() {
                            break;
                        }
                        let mut emails = Map::new();
                        for (i, blob) in groups[g].iter().enumerate() {
                            emails.insert(format!("e{g}_{i}"), email_obj(blob, &mailbox));
                        }
                        let resp = target.api(
                            &ag,
                            &[CORE, MAIL],
                            json!([["Email/import", {
                                "accountId": target.account_id, "emails": Value::Object(emails)
                            }, "i"]]),
                        );
                        imported.fetch_add(count_created(&resp), Ordering::Relaxed);
                    }
                });
            }
        });
        let import_time = started.elapsed();

        println!(
            "workers={workers:<3} Blob/upload {:>6.1}/s ({:>5.1}s)   Email/import {:>6.1}/s ({:>5.1}s)   end-to-end {:>5.1} mails/s   imported={}/{n}",
            ids.len() as f64 / upload_time.as_secs_f64(),
            upload_time.as_secs_f64(),
            imported.load(Ordering::Relaxed) as f64 / import_time.as_secs_f64(),
            import_time.as_secs_f64(),
            imported.load(Ordering::Relaxed) as f64
                / (upload_time + import_time).as_secs_f64(),
            imported.load(Ordering::Relaxed),
        );
    }
}
