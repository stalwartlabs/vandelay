/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::Duration;

use rusqlite::Connection;
use vandelay::db;
use vandelay::imap::client::{ConnectMode, ImapClient};
use vandelay::imap::error::ImapError;
use vandelay::imap::transport::Connector;
use vandelay::logging::Logger;
use vandelay::sync::CommonConfig;
use vandelay::sync::import_imap::{ImapAuth, ImapImportConfig, run};

type Script = Box<dyn FnOnce(&mut MockConn) -> std::io::Result<()> + Send + 'static>;

struct MockImap {
    addr: String,
    port: u16,
    _thread: thread::JoinHandle<()>,
}

struct MockConn {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl MockConn {
    fn write_line(&mut self, s: &str) -> std::io::Result<()> {
        self.writer.write_all(s.as_bytes())?;
        self.writer.write_all(b"\r\n")?;
        self.writer.flush()
    }

    fn write_raw(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }

    fn read_command(&mut self) -> std::io::Result<(String, String)> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "client closed",
            ));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        let (tag, rest) = trimmed
            .split_once(' ')
            .map(|(t, r)| (t.to_owned(), r.to_owned()))
            .unwrap_or_else(|| (trimmed.to_owned(), String::new()));
        Ok((tag, rest))
    }
}

impl MockImap {
    fn start<H>(handler: H) -> MockImap
    where
        H: FnOnce(&mut MockConn) -> std::io::Result<()> + Send + 'static,
    {
        Self::start_scripts(vec![Box::new(handler)])
    }

    fn start_scripts(scripts: Vec<Script>) -> MockImap {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        let addr = format!("127.0.0.1:{port}");
        let queue: Mutex<Vec<Script>> = Mutex::new(scripts.into_iter().rev().collect());
        let queue = std::sync::Arc::new(queue);
        let thread = thread::spawn(move || {
            for stream in listener.incoming() {
                let stream = match stream {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                let writer = stream.try_clone().expect("clone");
                let reader = BufReader::new(stream);
                let mut conn = MockConn { reader, writer };
                let script = queue.lock().expect("queue").pop();
                if let Some(script) = script {
                    thread::spawn(move || {
                        let _ = script(&mut conn);
                    });
                } else {
                    let _ = conn.write_line("* BYE no script left");
                }
            }
        });
        MockImap {
            addr,
            port,
            _thread: thread,
        }
    }

    fn url(&self) -> String {
        format!("imap://{}", self.addr)
    }
}

fn cleartext_connector() -> Connector {
    Connector::new(true).expect("connector")
}

fn connect_mock(server: &MockImap) -> Result<ImapClient, ImapError> {
    ImapClient::connect(
        &cleartext_connector(),
        "127.0.0.1",
        server.port,
        ConnectMode::Plain,
        Logger::from_flags(false, 0),
    )
}

fn tempfile(label: &str) -> PathBuf {
    let counter = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!("vandelay_mock_imap_{label}_{counter}.sqlite"));
    if p.exists() {
        let _ = std::fs::remove_file(&p);
    }
    p
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn run_import(
    server: &MockImap,
    user: &str,
    archive: PathBuf,
    config_tweak: impl FnOnce(&mut ImapImportConfig),
) -> Result<vandelay::sync::Summary, vandelay::error::Error> {
    let common = CommonConfig {
        archive: archive.clone(),
        threads: 1,
        dry_run: false,
        max_retries: 1,
        allow_invalid_certs: false,
        logger: Logger::from_flags(true, 0),
    };
    let mut config = ImapImportConfig {
        url: server.url(),
        auth: ImapAuth::Basic {
            user: user.to_owned(),
            password: "p@ss".to_owned(),
        },
        allow_cleartext: true,
        compress: false,
        include: Vec::new(),
        exclude: Vec::new(),
        exclude_special: Vec::new(),
        folder: Vec::new(),
        subscribed_only: false,
        automap: true,
        include_deleted: false,
        fetch_batch: 256,
        imap_connections: 1,
        allow_source_change: false,
    };
    config_tweak(&mut config);
    run(common, config)
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap_or(0)
}

fn folder_role(conn: &Connection, name: &str) -> Option<String> {
    conn.query_row("SELECT role FROM mailboxes WHERE name = ?1", [name], |r| {
        r.get::<_, Option<String>>(0)
    })
    .ok()
    .flatten()
}

#[test]
fn greeting_capabilities_are_picked_up() {
    let server = MockImap::start(|conn| {
        conn.write_line("* OK [CAPABILITY IMAP4rev2 STARTTLS AUTH=PLAIN LITERAL+] Hello")?;
        let (tag, cmd) = conn.read_command()?;
        assert!(cmd.starts_with("CAPABILITY"));
        conn.write_line("* CAPABILITY IMAP4rev2 STARTTLS AUTH=PLAIN LITERAL+")?;
        conn.write_line(&format!("{tag} OK done"))?;
        let _ = conn.read_command();
        Ok(())
    });
    let client = connect_mock(&server).expect("connect");
    assert!(client.has_capability("IMAP4rev2"));
    assert!(client.has_capability("AUTH=PLAIN"));
    assert!(client.has_capability("LITERAL+"));
    assert!(!client.has_capability("XOAUTH2"));
}

#[test]
fn authenticate_oauthbearer_sasl_ir() {
    let server = MockImap::start(|conn| {
        conn.write_line("* OK Hello")?;
        let (tag, _) = conn.read_command()?;
        conn.write_line("* CAPABILITY IMAP4rev2 AUTH=OAUTHBEARER SASL-IR")?;
        conn.write_line(&format!("{tag} OK done"))?;
        let (tag, cmd) = conn.read_command()?;
        assert!(
            cmd.starts_with("AUTHENTICATE OAUTHBEARER ")
                && cmd.len() > "AUTHENTICATE OAUTHBEARER ".len(),
            "expected SASL-IR form, got {cmd}"
        );
        conn.write_line(&format!("{tag} OK welcome"))?;
        let _ = conn.read_command();
        Ok(())
    });
    let mut client = connect_mock(&server).expect("connect");
    client
        .authenticate_oauthbearer("alice@example.com", "ya29.token")
        .expect("oauthbearer auth");
}

#[test]
fn authenticate_oauthbearer_continuation_payload_uses_gs2_header() {
    use base64::Engine;
    let server = MockImap::start(|conn| {
        conn.write_line("* OK Hello")?;
        let (tag, _) = conn.read_command()?;
        conn.write_line("* CAPABILITY IMAP4rev2 AUTH=OAUTHBEARER")?;
        conn.write_line(&format!("{tag} OK done"))?;
        let (tag, cmd) = conn.read_command()?;
        assert_eq!(cmd, "AUTHENTICATE OAUTHBEARER");
        conn.write_line("+ ")?;
        let mut line = String::new();
        conn.reader.read_line(&mut line)?;
        let payload = line.trim_end_matches(['\r', '\n']).to_owned();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&payload)
            .expect("base64");
        let text = std::str::from_utf8(&decoded).unwrap_or("");
        assert!(
            text.starts_with("n,a=alice@example.com,"),
            "expected GS2 header, got {text:?}"
        );
        assert!(
            text.contains("auth=Bearer ya29.token"),
            "expected bearer kvpair, got {text:?}"
        );
        assert!(
            text.ends_with("\x01\x01"),
            "expected trailing kvsep, got {text:?}"
        );
        conn.write_line(&format!("{tag} OK welcome"))?;
        let _ = conn.read_command();
        Ok(())
    });
    let mut client = connect_mock(&server).expect("connect");
    client
        .authenticate_oauthbearer("alice@example.com", "ya29.token")
        .expect("oauthbearer auth");
}

#[test]
fn authenticate_plain_with_sasl_ir() {
    let server = MockImap::start(|conn| {
        conn.write_line("* OK Hello")?;
        let (tag, cmd) = conn.read_command()?;
        assert!(cmd.starts_with("CAPABILITY"));
        conn.write_line("* CAPABILITY IMAP4rev2 AUTH=PLAIN SASL-IR")?;
        conn.write_line(&format!("{tag} OK done"))?;
        let (tag, cmd) = conn.read_command()?;
        assert!(
            cmd.starts_with("AUTHENTICATE PLAIN ") && cmd.len() > "AUTHENTICATE PLAIN ".len(),
            "expected SASL-IR form, got {cmd}"
        );
        conn.write_line(&format!("{tag} OK welcome"))?;
        let _ = conn.read_command();
        Ok(())
    });
    let mut client = connect_mock(&server).expect("connect");
    client.authenticate_plain("alice", "p@ss").expect("auth");
}

#[test]
fn authenticate_plain_continuation_path() {
    let server = MockImap::start(|conn| {
        conn.write_line("* OK Hello")?;
        let (tag, _) = conn.read_command()?;
        conn.write_line("* CAPABILITY IMAP4rev2 AUTH=PLAIN")?;
        conn.write_line(&format!("{tag} OK done"))?;
        let (tag, cmd) = conn.read_command()?;
        assert_eq!(cmd, "AUTHENTICATE PLAIN");
        conn.write_line("+ ")?;
        let payload = {
            let mut line = String::new();
            conn.reader.read_line(&mut line)?;
            line.trim_end_matches(['\r', '\n']).to_owned()
        };
        assert!(!payload.is_empty(), "client should send the base64 payload");
        conn.write_line(&format!("{tag} OK welcome"))?;
        let _ = conn.read_command();
        Ok(())
    });
    let mut client = connect_mock(&server).expect("connect");
    client.authenticate_plain("alice", "p@ss").expect("auth");
}

#[test]
fn authenticate_plain_refused_falls_back_to_login() {
    let server = MockImap::start(|conn| {
        conn.write_line("* OK ready")?;
        let (tag, _) = conn.read_command()?;
        conn.write_line("* CAPABILITY IMAP4rev2 AUTH=PLAIN")?;
        conn.write_line(&format!("{tag} OK done"))?;
        let (tag, cmd) = conn.read_command()?;
        assert!(cmd.starts_with("AUTHENTICATE PLAIN"));
        conn.write_line(&format!("{tag} NO [AUTHENTICATIONFAILED] bad creds"))?;
        let _ = conn.read_command();
        Ok(())
    });
    let mut client = connect_mock(&server).expect("connect");
    let err = client.authenticate_plain("alice", "p@ss").unwrap_err();
    assert!(matches!(err, ImapError::AuthFailed(_)));
}

#[test]
fn bye_mid_response_closes_client() {
    let server = MockImap::start(|conn| {
        conn.write_line("* OK Hello")?;
        let (tag, _) = conn.read_command()?;
        conn.write_line("* CAPABILITY IMAP4rev2")?;
        conn.write_line(&format!("{tag} OK done"))?;
        conn.read_command()?;
        conn.write_line("* BYE server logging out")?;
        Ok(())
    });
    let mut client = connect_mock(&server).expect("connect");
    let err = client.noop().unwrap_err();
    assert!(matches!(err, ImapError::Bye(_)));
}

#[test]
fn run_collect_parses_fetch_with_literal_body() {
    let server = MockImap::start(|conn| {
        conn.write_line("* OK Hello")?;
        let (tag, _) = conn.read_command()?;
        conn.write_line("* CAPABILITY IMAP4rev2")?;
        conn.write_line(&format!("{tag} OK done"))?;
        let (tag, _) = conn.read_command()?;
        conn.write_raw(b"* 1 FETCH (UID 5 BODY[] {11}\r\n")?;
        conn.write_raw(b"Hello world)\r\n")?;
        conn.write_line(&format!("{tag} OK FETCH done"))?;
        let _ = conn.read_command();
        Ok(())
    });
    let mut client = connect_mock(&server).expect("connect");
    let r = client
        .run_collect("UID FETCH 5 (UID BODY.PEEK[])")
        .expect("fetch");
    let body = r.untagged.iter().find_map(|u| match u {
        vandelay::imap::response::Untagged::Fetch { items, .. } => Some(items.clone()),
        _ => None,
    });
    let items = body.expect("fetch items");
    let body_item = items.iter().find(|(n, _)| n == "BODY[]").unwrap();
    match &body_item.1 {
        vandelay::imap::response::Value::Str(s) => assert_eq!(s, "Hello world"),
        vandelay::imap::response::Value::Bytes(b) => assert_eq!(b, b"Hello world"),
        other => panic!("unexpected body shape: {other:?}"),
    }
}

const MSG_BODY: &[u8] = b"From: a@b\r\nTo: c@d\r\nSubject: hi\r\nMessage-ID: <m1@h>\r\nDate: Mon, 12 May 2025 10:00:00 +0000\r\n\r\nhello";

fn write_capability(conn: &mut MockConn, caps: &str) -> std::io::Result<()> {
    conn.write_line(&format!("* CAPABILITY {caps}"))?;
    Ok(())
}

fn write_post_auth_caps(conn: &mut MockConn, tag: &str, caps: &str) -> std::io::Result<()> {
    write_capability(conn, caps)?;
    conn.write_line(&format!("{tag} OK done"))
}

fn auth_preamble(conn: &mut MockConn, caps: &str) -> std::io::Result<()> {
    conn.write_line("* OK ready")?;
    let (tag, _) = conn.read_command()?;
    write_capability(conn, caps)?;
    conn.write_line(&format!("{tag} OK done"))?;
    let (tag, _) = conn.read_command()?;
    write_post_auth_caps(conn, &tag, caps)?;
    let (tag, _) = conn.read_command()?;
    write_capability(conn, caps)?;
    conn.write_line(&format!("{tag} OK done"))?;
    if caps.contains("UTF8=ACCEPT") && caps.contains("ENABLE") {
        let (tag, _) = conn.read_command()?;
        conn.write_line("* ENABLED UTF8=ACCEPT")?;
        conn.write_line(&format!("{tag} OK"))?;
    }
    Ok(())
}

fn drain_until_close(conn: &mut MockConn) {
    while let Ok((tag, cmd)) = conn.read_command() {
        if cmd.starts_with("LOGOUT") {
            let _ = conn.write_line("* BYE bye");
            let _ = conn.write_line(&format!("{tag} OK"));
            break;
        }
        let _ = conn.write_line(&format!("{tag} OK"));
    }
}

fn worker_idle_script(caps: &'static str) -> Script {
    Box::new(move |conn: &mut MockConn| -> std::io::Result<()> {
        if auth_preamble(conn, caps).is_err() {
            return Ok(());
        }
        drain_until_close(conn);
        Ok(())
    })
}

fn worker_fetch_script(
    caps: &'static str,
    folder: &'static str,
    uidvalidity: u32,
    uidnext: u32,
    exists: u32,
    bodies: Vec<(u32, u32, &'static [u8])>,
) -> Script {
    Box::new(move |conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(conn, caps)?;
        let (tag, cmd) = conn.read_command()?;
        assert!(
            cmd.starts_with("SELECT") && cmd.contains(folder),
            "worker expected SELECT {folder}, got {cmd}"
        );
        write_select(conn, &tag, uidvalidity, uidnext, exists)?;
        let (tag, cmd) = conn.read_command()?;
        assert!(
            cmd.starts_with("UID FETCH"),
            "worker expected UID FETCH, got {cmd}"
        );
        for (seq, uid, body) in &bodies {
            write_fetch_message(conn, *seq, *uid, body)?;
        }
        conn.write_line(&format!("{tag} OK"))?;
        drain_until_close(conn);
        Ok(())
    })
}

fn write_select(
    conn: &mut MockConn,
    tag: &str,
    uidvalidity: u32,
    uidnext: u32,
    exists: u32,
) -> std::io::Result<()> {
    conn.write_line(&format!("* {exists} EXISTS"))?;
    conn.write_line(&format!("* OK [UIDVALIDITY {uidvalidity}] uids valid"))?;
    conn.write_line(&format!("* OK [UIDNEXT {uidnext}] next uid"))?;
    conn.write_line("* OK [PERMANENTFLAGS (\\Seen \\Flagged \\*)] limited")?;
    conn.write_line(&format!("{tag} OK [READ-WRITE] SELECT completed"))
}

fn write_fetch_message(
    conn: &mut MockConn,
    seq: u32,
    uid: u32,
    body: &[u8],
) -> std::io::Result<()> {
    let header = format!(
        "* {seq} FETCH (UID {uid} FLAGS (\\Seen) INTERNALDATE \"12-May-2025 10:00:00 +0000\" RFC822.SIZE {} BODY[] {{{}}}\r\n",
        body.len(),
        body.len()
    );
    conn.write_raw(header.as_bytes())?;
    conn.write_raw(body)?;
    conn.write_raw(b")\r\n")
}

fn control_script_one_folder(uidvalidity: u32, uidnext: u32, uids: &'static [u32]) -> Script {
    Box::new(move |conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN")?;
        let (tag, cmd) = conn.read_command()?;
        assert_eq!(cmd, "LIST \"\" \"*\"");
        conn.write_line("* LIST () \"/\" \"INBOX\"")?;
        conn.write_line(&format!("{tag} OK LIST done"))?;
        let (tag, cmd) = conn.read_command()?;
        assert_eq!(cmd, "LSUB \"\" \"*\"");
        conn.write_line(&format!("{tag} OK LSUB done"))?;
        let (tag, cmd) = conn.read_command()?;
        assert_eq!(cmd, "SELECT \"INBOX\"");
        write_select(conn, &tag, uidvalidity, uidnext, uids.len() as u32)?;
        let (tag, cmd) = conn.read_command()?;
        assert_eq!(cmd, "UID SEARCH ALL");
        let uid_strs: Vec<String> = uids.iter().map(|u| u.to_string()).collect();
        conn.write_line(&format!("* SEARCH {}", uid_strs.join(" ")))?;
        conn.write_line(&format!("{tag} OK SEARCH done"))?;
        drain_until_close(conn);
        Ok(())
    })
}

fn control_script_present_flags(
    uidvalidity: u32,
    uidnext: u32,
    uids: &'static [u32],
    flags_reply: &'static [(u32, &'static str)],
) -> Script {
    Box::new(move |conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN")?;
        let (tag, cmd) = conn.read_command()?;
        assert_eq!(cmd, "LIST \"\" \"*\"");
        conn.write_line("* LIST () \"/\" \"INBOX\"")?;
        conn.write_line(&format!("{tag} OK LIST done"))?;
        let (tag, cmd) = conn.read_command()?;
        assert_eq!(cmd, "LSUB \"\" \"*\"");
        conn.write_line(&format!("{tag} OK LSUB done"))?;
        let (tag, cmd) = conn.read_command()?;
        assert_eq!(cmd, "SELECT \"INBOX\"");
        write_select(conn, &tag, uidvalidity, uidnext, uids.len() as u32)?;
        let (tag, cmd) = conn.read_command()?;
        assert_eq!(cmd, "UID SEARCH ALL");
        let uid_strs: Vec<String> = uids.iter().map(|u| u.to_string()).collect();
        conn.write_line(&format!("* SEARCH {}", uid_strs.join(" ")))?;
        conn.write_line(&format!("{tag} OK SEARCH done"))?;
        let (tag, cmd) = conn.read_command()?;
        assert!(
            cmd.starts_with("UID FETCH") && cmd.contains("(UID FLAGS)") && !cmd.contains("BODY"),
            "expected body-less flags fetch on the present set, got {cmd}"
        );
        for (uid, flags) in flags_reply {
            conn.write_line(&format!("* {uid} FETCH (UID {uid} FLAGS ({flags}))"))?;
        }
        conn.write_line(&format!("{tag} OK FETCH done"))?;
        drain_until_close(conn);
        Ok(())
    })
}

#[test]
fn coordinator_imports_one_folder_one_message() {
    let server = MockImap::start_scripts(vec![
        control_script_one_folder(12345, 2, &[1]),
        worker_fetch_script(
            "IMAP4rev2 LITERAL+ AUTH=PLAIN",
            "INBOX",
            12345,
            2,
            1,
            vec![(1, 1, MSG_BODY)],
        ),
    ]);
    let archive = tempfile("happy");
    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("import");
    let mailbox = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == "mailbox")
        .unwrap();
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == "email")
        .unwrap();
    assert_eq!(mailbox.1.created, 1);
    assert_eq!(email.1.created, 1, "summary={summary:?}");
    assert_eq!(email.1.fetched, 1);
    let conn = Connection::open(&archive).unwrap();
    db::init::apply_schema(&conn).unwrap();
    assert_eq!(count(&conn, "mailboxes"), 1);
    assert_eq!(count(&conn, "emails"), 1);
    assert_eq!(count(&conn, "blobs"), 1);
    assert_eq!(folder_role(&conn, "INBOX"), Some("inbox".to_owned()));
}

#[test]
fn coordinator_uses_esearch_when_advertised() {
    let control: Script = Box::new(|conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(conn, "IMAP4rev2 ESEARCH LITERAL+ AUTH=PLAIN")?;
        let (tag, _) = conn.read_command()?;
        conn.write_line("* LIST () \"/\" \"INBOX\"")?;
        conn.write_line(&format!("{tag} OK"))?;
        let (tag, _) = conn.read_command()?;
        conn.write_line(&format!("{tag} OK"))?;
        let (tag, _) = conn.read_command()?;
        write_select(conn, &tag, 100, 4, 2)?;
        let (tag, cmd) = conn.read_command()?;
        assert_eq!(cmd, "UID SEARCH RETURN (ALL) ALL");
        conn.write_line(&format!("* ESEARCH (TAG \"{tag}\") UID ALL 1:3"))?;
        conn.write_line(&format!("{tag} OK"))?;
        drain_until_close(conn);
        Ok(())
    });
    let server = MockImap::start_scripts(vec![
        control,
        worker_fetch_script(
            "IMAP4rev2 ESEARCH LITERAL+ AUTH=PLAIN",
            "INBOX",
            100,
            4,
            2,
            vec![(1, 1, MSG_BODY), (2, 2, MSG_BODY), (3, 3, MSG_BODY)],
        ),
    ]);
    let archive = tempfile("esearch");
    let _ = run_import(&server, "alice", archive.clone(), |_| {}).expect("import");
    let conn = Connection::open(&archive).unwrap();
    assert!(count(&conn, "emails") >= 1);
}

#[test]
fn coordinator_falls_back_when_esearch_returns_bad() {
    let control: Script = Box::new(|conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(conn, "IMAP4rev2 ESEARCH LITERAL+ AUTH=PLAIN")?;
        let (tag, _) = conn.read_command()?;
        conn.write_line("* LIST () \"/\" \"INBOX\"")?;
        conn.write_line(&format!("{tag} OK"))?;
        let (tag, _) = conn.read_command()?;
        conn.write_line(&format!("{tag} OK"))?;
        let (tag, _) = conn.read_command()?;
        write_select(conn, &tag, 100, 2, 1)?;
        let (tag, cmd) = conn.read_command()?;
        assert_eq!(cmd, "UID SEARCH RETURN (ALL) ALL");
        conn.write_line(&format!("{tag} BAD esearch is not really supported"))?;
        let (tag, cmd) = conn.read_command()?;
        assert_eq!(cmd, "UID SEARCH ALL");
        conn.write_line("* SEARCH 1")?;
        conn.write_line(&format!("{tag} OK"))?;
        drain_until_close(conn);
        Ok(())
    });
    let server = MockImap::start_scripts(vec![
        control,
        worker_fetch_script(
            "IMAP4rev2 ESEARCH LITERAL+ AUTH=PLAIN",
            "INBOX",
            100,
            2,
            1,
            vec![(1, 1, MSG_BODY)],
        ),
    ]);
    let archive = tempfile("esearch_fallback");
    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("import");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == "email")
        .unwrap();
    assert_eq!(
        email.1.created, 1,
        "fallback should have fetched the message"
    );
}

#[test]
fn coordinator_falls_back_to_uid_fetch_when_search_all_bad() {
    let control: Script = Box::new(|conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN")?;
        let (tag, _) = conn.read_command()?;
        conn.write_line("* LIST () \"/\" \"INBOX\"")?;
        conn.write_line(&format!("{tag} OK"))?;
        let (tag, _) = conn.read_command()?;
        conn.write_line(&format!("{tag} OK"))?;
        let (tag, _) = conn.read_command()?;
        write_select(conn, &tag, 100, 2, 1)?;
        let (tag, cmd) = conn.read_command()?;
        assert_eq!(cmd, "UID SEARCH ALL");
        conn.write_line(&format!("{tag} BAD legacy server"))?;
        let (tag, cmd) = conn.read_command()?;
        assert_eq!(cmd, "UID FETCH 1:* (UID)");
        conn.write_line("* 1 FETCH (UID 1)")?;
        conn.write_line(&format!("{tag} OK"))?;
        drain_until_close(conn);
        Ok(())
    });
    let server = MockImap::start_scripts(vec![
        control,
        worker_fetch_script(
            "IMAP4rev2 LITERAL+ AUTH=PLAIN",
            "INBOX",
            100,
            2,
            1,
            vec![(1, 1, MSG_BODY)],
        ),
    ]);
    let archive = tempfile("search_fallback");
    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("import");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == "email")
        .unwrap();
    assert_eq!(email.1.created, 1);
}

#[test]
fn coordinator_imports_message_despite_size_mismatch() {
    let control = control_script_one_folder(100, 2, &[1]);
    let worker: Script = Box::new(|conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN")?;
        let (tag, cmd) = conn.read_command()?;
        assert!(cmd.starts_with("SELECT"));
        write_select(conn, &tag, 100, 2, 1)?;
        let (tag, _) = conn.read_command()?;
        let header = format!(
            "* 1 FETCH (UID 1 FLAGS () INTERNALDATE \"12-May-2025 10:00:00 +0000\" RFC822.SIZE 9999 BODY[] {{{}}}\r\n",
            MSG_BODY.len()
        );
        conn.write_raw(header.as_bytes())?;
        conn.write_raw(MSG_BODY)?;
        conn.write_raw(b")\r\n")?;
        conn.write_line(&format!("{tag} OK"))?;
        drain_until_close(conn);
        Ok(())
    });
    let server = MockImap::start_scripts(vec![control, worker]);
    let archive = tempfile("size_mismatch");
    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("import");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == "email")
        .unwrap();
    assert_eq!(email.1.skipped, 0);
    assert_eq!(email.1.created, 1);
    let dbc = Connection::open(&archive).unwrap();
    assert_eq!(count(&dbc, "emails"), 1);
}

fn single_inbox_scripts(uidvalidity: u32, uidnext: u32, body: &'static [u8]) -> Vec<Script> {
    vec![
        control_script_one_folder(uidvalidity, uidnext, &[1]),
        worker_fetch_script(
            "IMAP4rev2 LITERAL+ AUTH=PLAIN",
            "INBOX",
            uidvalidity,
            uidnext,
            1,
            vec![(1, 1, body)],
        ),
    ]
}

#[test]
fn coordinator_uidvalidity_change_wipes_folder_emails() {
    let mut scripts: Vec<Script> = Vec::new();
    scripts.extend(single_inbox_scripts(100, 2, MSG_BODY));
    scripts.extend(single_inbox_scripts(999, 2, MSG_BODY));
    let server = MockImap::start_scripts(scripts);
    let archive = tempfile("uidvalidity");
    run_import(&server, "alice", archive.clone(), |_| {}).expect("first import");
    let dbc = Connection::open(&archive).unwrap();
    assert_eq!(count(&dbc, "emails"), 1);
    drop(dbc);

    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("second import");
    let dbc = Connection::open(&archive).unwrap();
    assert_eq!(count(&dbc, "emails"), 1);
    assert_eq!(count(&dbc, "blobs"), 1);
    let new_uv: u32 = dbc
        .query_row(
            "SELECT uidvalidity FROM imap_folder_state WHERE folder = 'INBOX'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(new_uv, 999);
    let email_after = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == "email")
        .unwrap()
        .1
        .clone();
    assert_eq!(email_after.deleted, 1, "stale UV emails wiped");
    assert_eq!(email_after.created, 1, "re-imported under new UV");
}

#[test]
fn coordinator_present_run_is_convergent() {
    let mut scripts: Vec<Script> = Vec::new();
    scripts.extend(single_inbox_scripts(100, 2, MSG_BODY));

    scripts.push(control_script_present_flags(100, 2, &[1], &[(1, "\\Seen")]));
    scripts.push(worker_idle_script("IMAP4rev2 LITERAL+ AUTH=PLAIN"));
    let server = MockImap::start_scripts(scripts);
    let archive = tempfile("converge");
    run_import(&server, "alice", archive.clone(), |_| {}).expect("first import");
    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("second import");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == "email")
        .unwrap();
    assert_eq!(email.1.created, 0, "convergent run creates nothing");
    assert_eq!(email.1.deleted, 0, "convergent run deletes nothing");
    assert_eq!(email.1.updated, 0, "unchanged flags update nothing");
    let dbc = Connection::open(&archive).unwrap();
    assert_eq!(
        count(&dbc, "blobs"),
        1,
        "no body re-fetched on a present-only run"
    );
}

#[test]
fn coordinator_present_flag_change_updates_keywords() {
    let mut scripts: Vec<Script> = Vec::new();
    scripts.extend(single_inbox_scripts(100, 2, MSG_BODY));
    scripts.push(control_script_present_flags(
        100,
        2,
        &[1],
        &[(1, "\\Seen \\Flagged")],
    ));
    scripts.push(worker_idle_script("IMAP4rev2 LITERAL+ AUTH=PLAIN"));
    let server = MockImap::start_scripts(scripts);
    let archive = tempfile("flagupdate");
    run_import(&server, "alice", archive.clone(), |_| {}).expect("first import");
    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("second import");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == "email")
        .unwrap();
    assert_eq!(
        email.1.updated, 1,
        "a changed flag set is counted as updated"
    );
    assert_eq!(email.1.created, 0, "present message is not re-created");
    assert_eq!(email.1.fetched, 0, "no body fetched on a present-only run");
    let dbc = Connection::open(&archive).unwrap();
    assert_eq!(count(&dbc, "blobs"), 1, "body not re-fetched");
    let kw: String = dbc
        .query_row("SELECT keywords FROM emails LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert!(
        kw.contains("$seen") && kw.contains("$flagged"),
        "keywords should reflect the new \\Flagged: {kw}"
    );
}

#[test]
fn coordinator_present_newly_deleted_is_left_intact() {
    let mut scripts: Vec<Script> = Vec::new();
    scripts.extend(single_inbox_scripts(100, 2, MSG_BODY));
    scripts.push(control_script_present_flags(
        100,
        2,
        &[1],
        &[(1, "\\Seen \\Deleted")],
    ));
    scripts.push(worker_idle_script("IMAP4rev2 LITERAL+ AUTH=PLAIN"));
    let server = MockImap::start_scripts(scripts);
    let archive = tempfile("presentdeleted");
    run_import(&server, "alice", archive.clone(), |_| {}).expect("first import");
    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("second import");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == "email")
        .unwrap();
    assert_eq!(
        email.1.updated, 0,
        "a present message that newly gained \\Deleted is skipped, not updated"
    );
    let dbc = Connection::open(&archive).unwrap();
    assert_eq!(
        count(&dbc, "emails"),
        1,
        "the archived message is preserved"
    );
    let kw: String = dbc
        .query_row("SELECT keywords FROM emails LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert!(
        kw.contains("$seen") && !kw.contains("$deleted"),
        "keywords left intact: {kw}"
    );
}

#[test]
fn coordinator_special_use_drives_role() {
    let control: Script = Box::new(|conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(
            conn,
            "IMAP4rev2 LIST-EXTENDED SPECIAL-USE LITERAL+ AUTH=PLAIN",
        )?;
        let (tag, cmd) = conn.read_command()?;
        assert!(cmd.contains("RETURN (SPECIAL-USE SUBSCRIBED)"));
        conn.write_line("* LIST () \"/\" \"INBOX\"")?;
        conn.write_line("* LIST (\\Sent) \"/\" \"S1\"")?;
        conn.write_line("* LIST (\\Trash) \"/\" \"Bin\"")?;
        conn.write_line(&format!("{tag} OK"))?;
        for _ in 0..3 {
            let (tag, _) = conn.read_command()?;
            write_select(conn, &tag, 1, 1, 0)?;
            let (tag, _) = conn.read_command()?;
            conn.write_line("* SEARCH")?;
            conn.write_line(&format!("{tag} OK"))?;
        }
        drain_until_close(conn);
        Ok(())
    });
    let server = MockImap::start_scripts(vec![
        control,
        worker_idle_script("IMAP4rev2 LIST-EXTENDED SPECIAL-USE LITERAL+"),
    ]);
    let archive = tempfile("specialuse");
    run_import(&server, "alice", archive.clone(), |_| {}).expect("import");
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(folder_role(&conn, "INBOX"), Some("inbox".to_owned()));
    assert_eq!(folder_role(&conn, "S1"), Some("sent".to_owned()));
    assert_eq!(folder_role(&conn, "Bin"), Some("trash".to_owned()));
}

#[test]
fn coordinator_omits_special_use_when_unadvertised() {
    let control: Script = Box::new(|conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(
            conn,
            "IMAP4rev2 LIST-EXTENDED LIST-STATUS LITERAL+ AUTH=PLAIN",
        )?;
        let (tag, cmd) = conn.read_command()?;
        if cmd.contains("SPECIAL-USE") {
            conn.write_line(&format!(
                "{tag} BAD parse error: unknown LIST return option \"SPECIAL-USE\""
            ))?;
            return Ok(());
        }
        assert!(
            cmd.contains("RETURN (SUBSCRIBED CHILDREN STATUS (UIDVALIDITY UIDNEXT MESSAGES))"),
            "expected LIST-STATUS form without SPECIAL-USE, got {cmd}"
        );
        conn.write_line("* LIST () \"/\" \"INBOX\"")?;
        conn.write_line("* STATUS \"INBOX\" (UIDVALIDITY 100 UIDNEXT 1 MESSAGES 0)")?;
        conn.write_line(&format!("{tag} OK"))?;
        drain_until_close(conn);
        Ok(())
    });
    let server = MockImap::start_scripts(vec![
        control,
        worker_idle_script("IMAP4rev2 LIST-EXTENDED LIST-STATUS LITERAL+"),
    ]);
    let archive = tempfile("nospecialuse");
    run_import(&server, "alice", archive.clone(), |_| {})
        .expect("import must not send unadvertised SPECIAL-USE");
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(folder_role(&conn, "INBOX"), Some("inbox".to_owned()));
}

#[test]
fn coordinator_skips_deleted_messages_by_default() {
    let control = control_script_one_folder(100, 3, &[1, 2]);
    let worker: Script = Box::new(|conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN")?;
        let (tag, cmd) = conn.read_command()?;
        assert!(cmd.starts_with("SELECT"));
        write_select(conn, &tag, 100, 3, 2)?;
        let (tag, _) = conn.read_command()?;
        let h1 = format!(
            "* 1 FETCH (UID 1 FLAGS (\\Seen) INTERNALDATE \"12-May-2025 10:00:00 +0000\" RFC822.SIZE {} BODY[] {{{}}}\r\n",
            MSG_BODY.len(),
            MSG_BODY.len()
        );
        conn.write_raw(h1.as_bytes())?;
        conn.write_raw(MSG_BODY)?;
        conn.write_raw(b")\r\n")?;
        let h2 = format!(
            "* 2 FETCH (UID 2 FLAGS (\\Deleted) INTERNALDATE \"13-May-2025 10:00:00 +0000\" RFC822.SIZE {} BODY[] {{{}}}\r\n",
            MSG_BODY.len(),
            MSG_BODY.len()
        );
        conn.write_raw(h2.as_bytes())?;
        conn.write_raw(MSG_BODY)?;
        conn.write_raw(b")\r\n")?;
        conn.write_line(&format!("{tag} OK"))?;
        drain_until_close(conn);
        Ok(())
    });
    let server = MockImap::start_scripts(vec![control, worker]);
    let archive = tempfile("deleted");
    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("import");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == "email")
        .unwrap();
    assert_eq!(email.1.created, 1, "only live message imported");
    assert_eq!(email.1.skipped, 1, "deleted skipped by default");
}

#[test]
fn coordinator_source_change_detected_on_different_url() {
    let control1: Script = Box::new(|conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN")?;
        let (tag, _) = conn.read_command()?;
        conn.write_line(&format!("{tag} OK"))?;
        let (tag, _) = conn.read_command()?;
        conn.write_line(&format!("{tag} OK"))?;
        drain_until_close(conn);
        Ok(())
    });
    let server1 = MockImap::start_scripts(vec![
        control1,
        worker_idle_script("IMAP4rev2 LITERAL+ AUTH=PLAIN"),
    ]);
    let archive = tempfile("srcchange");
    run_import(&server1, "alice", archive.clone(), |_| {}).expect("first");

    let control2: Script = Box::new(|conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN")?;
        drain_until_close(conn);
        Ok(())
    });
    let server2 = MockImap::start_scripts(vec![control2]);

    let err = run_import(&server2, "alice", archive.clone(), |_| {}).unwrap_err();
    assert!(matches!(err, vandelay::error::Error::SourceChange(_)));
}

#[test]
fn client_compress_deflate_upgrade_round_trip() {
    use std::io::Write as _;
    let server = MockImap::start(|conn| {
        conn.write_line("* OK ready")?;
        let (tag, _) = conn.read_command()?;
        write_capability(conn, "IMAP4rev2 AUTH=PLAIN COMPRESS=DEFLATE")?;
        conn.write_line(&format!("{tag} OK done"))?;
        let (tag, cmd) = conn.read_command()?;
        assert_eq!(cmd, "COMPRESS DEFLATE");
        conn.write_line(&format!("{tag} OK compress on"))?;

        let mut compressor = flate2::Compress::new(flate2::Compression::default(), false);
        let mut decompressor = flate2::Decompress::new(false);

        let mut input = vec![0u8; 4096];
        let mut plaintext: Vec<u8> = Vec::new();
        loop {
            let n = std::io::Read::read(&mut conn.reader, &mut input)?;
            if n == 0 {
                break;
            }
            let mut out = vec![0u8; 8192];
            let before_in = decompressor.total_in();
            let before_out = decompressor.total_out();
            decompressor
                .decompress(&input[..n], &mut out, flate2::FlushDecompress::None)
                .unwrap();
            let produced = (decompressor.total_out() - before_out) as usize;
            plaintext.extend_from_slice(&out[..produced]);
            let _ = before_in;
            if plaintext.contains(&b'\n') {
                break;
            }
        }
        let line = std::str::from_utf8(&plaintext).unwrap_or("");
        assert!(
            line.contains("NOOP"),
            "client sent {line:?} (expected NOOP)"
        );
        let client_tag = line.split_whitespace().next().unwrap_or("?");

        let reply = format!("{client_tag} OK pong\r\n");
        let mut comp_out = vec![0u8; reply.len() + 256];
        compressor
            .compress(reply.as_bytes(), &mut comp_out, flate2::FlushCompress::Sync)
            .unwrap();
        let produced = compressor.total_out() as usize;
        conn.writer.write_all(&comp_out[..produced])?;
        conn.writer.flush()?;
        Ok(())
    });
    let mut client = connect_mock(&server).expect("connect");
    client.compress_deflate().expect("compress upgrade");
    client.noop().expect("noop after compress");
}

#[test]
fn client_compress_refused_when_capability_missing() {
    let server = MockImap::start(|conn| {
        conn.write_line("* OK ready")?;
        let (tag, _) = conn.read_command()?;
        write_capability(conn, "IMAP4rev2 AUTH=PLAIN")?;
        conn.write_line(&format!("{tag} OK done"))?;
        Ok(())
    });
    let mut client = connect_mock(&server).expect("connect");
    let err = client.compress_deflate().unwrap_err();
    assert!(matches!(err, ImapError::Unsupported(_)));
}

#[test]
fn coordinator_dispatches_to_multiple_worker_connections() {
    use std::sync::atomic::AtomicUsize;
    static WORKER_INVOCATIONS: AtomicUsize = AtomicUsize::new(0);
    WORKER_INVOCATIONS.store(0, Ordering::SeqCst);

    let many_uids: Vec<u32> = (1..=8).collect();
    let many_uids_static: &'static [u32] = Box::leak(many_uids.into_boxed_slice());
    let control = control_script_one_folder(100, 9, many_uids_static);

    let make_worker = || -> Script {
        Box::new(|conn: &mut MockConn| -> std::io::Result<()> {
            WORKER_INVOCATIONS.fetch_add(1, Ordering::SeqCst);
            auth_preamble(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN")?;

            loop {
                let (tag, cmd) = match conn.read_command() {
                    Ok(x) => x,
                    Err(_) => return Ok(()),
                };
                if cmd.starts_with("SELECT") {
                    write_select(conn, &tag, 100, 9, 8)?;
                    continue;
                }
                if cmd.starts_with("UID FETCH") {
                    let after = cmd.strip_prefix("UID FETCH ").unwrap_or("");
                    let set = after.split_whitespace().next().unwrap_or("");
                    let uids = parse_uid_set(set);
                    for (i, uid) in uids.iter().enumerate() {
                        write_fetch_message(conn, (i as u32) + 1, *uid, MSG_BODY)?;
                    }
                    conn.write_line(&format!("{tag} OK"))?;
                    continue;
                }
                if cmd.starts_with("LOGOUT") {
                    conn.write_line("* BYE")?;
                    conn.write_line(&format!("{tag} OK"))?;
                    return Ok(());
                }
                conn.write_line(&format!("{tag} OK"))?;
            }
        })
    };
    let server =
        MockImap::start_scripts(vec![control, make_worker(), make_worker(), make_worker()]);
    let archive = tempfile("parallel");
    let summary = run_import(&server, "alice", archive.clone(), |c| {
        c.imap_connections = 3;
        c.fetch_batch = 3;
    })
    .expect("import");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == "email")
        .unwrap();
    assert_eq!(email.1.created, 8, "all 8 messages imported");

    assert!(
        WORKER_INVOCATIONS.load(Ordering::SeqCst) >= 1,
        "at least one worker accepted the connection"
    );
}

fn parse_uid_set(s: &str) -> Vec<u32> {
    let mut out = Vec::new();
    for part in s.split(',') {
        if let Some((lo, hi)) = part.split_once(':') {
            let lo: u32 = lo.parse().unwrap_or(0);
            let hi: u32 = hi.parse().unwrap_or(0);
            for n in lo..=hi {
                out.push(n);
            }
        } else if let Ok(n) = part.parse::<u32>() {
            out.push(n);
        }
    }
    out
}

#[test]
fn coordinator_uses_list_status_to_skip_empty_folder_select() {
    use std::sync::atomic::AtomicU32;
    static SELECTS: AtomicU32 = AtomicU32::new(0);
    SELECTS.store(0, Ordering::SeqCst);

    let control: Script = Box::new(|conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(
            conn,
            "IMAP4rev2 LITERAL+ LIST-EXTENDED LIST-STATUS SPECIAL-USE AUTH=PLAIN",
        )?;
        let (tag, cmd) = conn.read_command()?;
        assert!(
            cmd.contains(
                "RETURN (SPECIAL-USE SUBSCRIBED CHILDREN STATUS (UIDVALIDITY UIDNEXT MESSAGES))"
            ),
            "expected LIST-STATUS form, got {cmd}"
        );

        conn.write_line("* LIST () \"/\" \"INBOX\"")?;
        conn.write_line("* STATUS \"INBOX\" (UIDVALIDITY 100 UIDNEXT 1 MESSAGES 0)")?;
        conn.write_line("* LIST () \"/\" \"Sub\"")?;
        conn.write_line("* STATUS \"Sub\" (UIDVALIDITY 200 UIDNEXT 2 MESSAGES 1)")?;
        conn.write_line(&format!("{tag} OK"))?;

        loop {
            let (tag, cmd) = match conn.read_command() {
                Ok(x) => x,
                Err(_) => return Ok(()),
            };
            if cmd.starts_with("SELECT") {
                SELECTS.fetch_add(1, Ordering::SeqCst);
                let uv = if cmd.contains("INBOX") { 100 } else { 200 };
                write_select(conn, &tag, uv, 2, 1)?;
            } else if cmd.starts_with("UID SEARCH") {
                conn.write_line("* SEARCH 1")?;
                conn.write_line(&format!("{tag} OK"))?;
            } else if cmd.starts_with("NOOP") {
                conn.write_line(&format!("{tag} OK"))?;
            } else if cmd.starts_with("LOGOUT") {
                conn.write_line("* BYE")?;
                conn.write_line(&format!("{tag} OK"))?;
                return Ok(());
            } else {
                conn.write_line(&format!("{tag} OK"))?;
            }
        }
    });
    let server = MockImap::start_scripts(vec![
        control,
        worker_fetch_script(
            "IMAP4rev2 LITERAL+ LIST-EXTENDED LIST-STATUS",
            "Sub",
            200,
            2,
            1,
            vec![(1, 1, MSG_BODY)],
        ),
    ]);
    let archive = tempfile("liststatus");
    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("import");

    let n = SELECTS.load(Ordering::SeqCst);
    assert_eq!(n, 1, "expected exactly 1 control-side SELECT, got {n}");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == "email")
        .unwrap();
    assert_eq!(email.1.created, 1);
}

#[test]
fn coordinator_noops_between_folders() {
    use std::sync::atomic::AtomicU32;
    static NOOPS: AtomicU32 = AtomicU32::new(0);
    NOOPS.store(0, Ordering::SeqCst);

    let control: Script = Box::new(|conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN")?;
        let (tag, _) = conn.read_command()?;
        conn.write_line("* LIST () \"/\" \"A\"")?;
        conn.write_line("* LIST () \"/\" \"B\"")?;
        conn.write_line(&format!("{tag} OK"))?;
        let (tag, _) = conn.read_command()?;
        conn.write_line(&format!("{tag} OK"))?;

        for i in 0..2 {
            let (tag, cmd) = conn.read_command()?;
            if cmd == "NOOP" {
                NOOPS.fetch_add(1, Ordering::SeqCst);
                conn.write_line(&format!("{tag} OK NOOP"))?;
                let (tag2, _) = conn.read_command()?;
                write_select(conn, &tag2, (i + 1) as u32, 1, 0)?;
            } else {
                assert!(
                    cmd.starts_with("SELECT"),
                    "expected SELECT or NOOP, got {cmd}"
                );
                write_select(conn, &tag, (i + 1) as u32, 1, 0)?;
            }
            let (tag, cmd) = conn.read_command()?;
            assert!(
                cmd.starts_with("UID SEARCH"),
                "expected UID SEARCH, got {cmd}"
            );
            conn.write_line("* SEARCH")?;
            conn.write_line(&format!("{tag} OK"))?;
        }
        drain_until_close(conn);
        Ok(())
    });
    let server = MockImap::start_scripts(vec![
        control,
        worker_idle_script("IMAP4rev2 LITERAL+ AUTH=PLAIN"),
    ]);
    let archive = tempfile("noop");
    let _ = run_import(&server, "alice", archive.clone(), |_| {}).expect("import");
    assert_eq!(
        NOOPS.load(Ordering::SeqCst),
        1,
        "exactly one NOOP between the two folders"
    );
}

#[test]
fn coordinator_retries_transient_no_on_uid_search() {
    use std::sync::atomic::AtomicU32;
    static SEARCH_ATTEMPTS: AtomicU32 = AtomicU32::new(0);
    SEARCH_ATTEMPTS.store(0, Ordering::SeqCst);

    let control: Script = Box::new(|conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN")?;
        let (tag, _) = conn.read_command()?;
        conn.write_line("* LIST () \"/\" \"INBOX\"")?;
        conn.write_line(&format!("{tag} OK"))?;
        let (tag, _) = conn.read_command()?;
        conn.write_line(&format!("{tag} OK"))?;
        let (tag, _) = conn.read_command()?;
        write_select(conn, &tag, 100, 2, 1)?;
        let (tag, cmd) = conn.read_command()?;
        assert_eq!(cmd, "UID SEARCH ALL");
        SEARCH_ATTEMPTS.fetch_add(1, Ordering::SeqCst);
        conn.write_line(&format!("{tag} NO try again later"))?;
        let (tag, cmd) = conn.read_command()?;
        assert_eq!(cmd, "UID SEARCH ALL");
        SEARCH_ATTEMPTS.fetch_add(1, Ordering::SeqCst);
        conn.write_line("* SEARCH 1")?;
        conn.write_line(&format!("{tag} OK"))?;
        drain_until_close(conn);
        Ok(())
    });
    let server = MockImap::start_scripts(vec![
        control,
        worker_fetch_script(
            "IMAP4rev2 LITERAL+ AUTH=PLAIN",
            "INBOX",
            100,
            2,
            1,
            vec![(1, 1, MSG_BODY)],
        ),
    ]);
    let archive = tempfile("retry");
    let summary =
        run_import(&server, "alice", archive.clone(), |_| {}).expect("import succeeds via retry");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == "email")
        .unwrap();
    assert_eq!(email.1.created, 1);
    assert_eq!(
        SEARCH_ATTEMPTS.load(Ordering::SeqCst),
        2,
        "first UID SEARCH should have been retried once"
    );
}

#[test]
fn coordinator_auth_plain_refused_then_login_succeeds() {
    let control: Script = Box::new(|conn: &mut MockConn| -> std::io::Result<()> {
        conn.write_line("* OK ready")?;
        let (tag, _) = conn.read_command()?;
        write_capability(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN")?;
        conn.write_line(&format!("{tag} OK done"))?;
        let (tag, cmd) = conn.read_command()?;
        assert!(cmd.starts_with("AUTHENTICATE PLAIN"));
        conn.write_line(&format!("{tag} NO PLAIN refused"))?;
        let (tag, cmd) = conn.read_command()?;
        assert!(cmd.starts_with("LOGIN "));
        conn.write_line(&format!("{tag} OK welcome"))?;
        let (tag, _) = conn.read_command()?;
        write_capability(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN")?;
        conn.write_line(&format!("{tag} OK done"))?;
        let (tag, _) = conn.read_command()?;
        conn.write_line("* LIST () \"/\" \"INBOX\"")?;
        conn.write_line(&format!("{tag} OK"))?;
        let (tag, _) = conn.read_command()?;
        conn.write_line(&format!("{tag} OK"))?;
        let (tag, _) = conn.read_command()?;
        write_select(conn, &tag, 100, 1, 0)?;
        let (tag, _) = conn.read_command()?;
        conn.write_line("* SEARCH")?;
        conn.write_line(&format!("{tag} OK"))?;
        drain_until_close(conn);
        Ok(())
    });
    let server = MockImap::start_scripts(vec![
        control,
        worker_idle_script("IMAP4rev2 LITERAL+ AUTH=PLAIN"),
    ]);
    let archive = tempfile("plain_to_login");
    let summary = run_import(&server, "alice", archive, |_| {}).expect("import");
    assert!(!summary.any_failed());
}

#[test]
fn coordinator_auth_plain_and_login_both_refused_aborts_run() {
    let server = MockImap::start(|conn| {
        conn.write_line("* OK ready")?;
        let (tag, _) = conn.read_command()?;
        write_capability(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN")?;
        conn.write_line(&format!("{tag} OK done"))?;
        let (tag, cmd) = conn.read_command()?;
        assert!(cmd.starts_with("AUTHENTICATE PLAIN"));
        conn.write_line(&format!("{tag} NO PLAIN not allowed"))?;
        let (tag, cmd) = conn.read_command()?;
        assert!(cmd.starts_with("LOGIN"));
        conn.write_line(&format!("{tag} NO [AUTHENTICATIONFAILED] bad password"))?;
        let _ = conn.read_command();
        Ok(())
    });
    let archive = tempfile("dual_refuse");
    let err = run_import(&server, "alice", archive, |_| {}).unwrap_err();
    assert!(matches!(err, vandelay::error::Error::Connection(_)));
    assert_eq!(err.exit_code(), 2);
}

#[test]
fn coordinator_skips_folder_on_select_no() {
    let control: Script = Box::new(|conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN")?;
        let (tag, _) = conn.read_command()?;
        conn.write_line("* LIST () \"/\" \"INBOX\"")?;
        conn.write_line("* LIST () \"/\" \"Forbidden\"")?;
        conn.write_line(&format!("{tag} OK"))?;
        let (tag, _) = conn.read_command()?;
        conn.write_line(&format!("{tag} OK"))?;
        let (tag, _) = conn.read_command()?;
        write_select(conn, &tag, 100, 1, 0)?;
        let (tag, _) = conn.read_command()?;
        conn.write_line("* SEARCH")?;
        conn.write_line(&format!("{tag} OK"))?;
        let (tag, cmd) = conn.read_command()?;
        if cmd == "NOOP" {
            conn.write_line(&format!("{tag} OK"))?;
            let (tag, _) = conn.read_command()?;
            conn.write_line(&format!("{tag} NO permission denied"))?;
        } else {
            conn.write_line(&format!("{tag} NO permission denied"))?;
        }
        drain_until_close(conn);
        Ok(())
    });
    let server = MockImap::start_scripts(vec![
        control,
        worker_idle_script("IMAP4rev2 LITERAL+ AUTH=PLAIN"),
    ]);
    let archive = tempfile("select_no");
    let summary = run_import(&server, "alice", archive, |_| {}).expect("import");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == "email")
        .unwrap();
    assert!(email.1.failed >= 1);
}

#[test]
fn coordinator_include_deleted_imports_with_dollar_deleted_keyword() {
    let control = control_script_one_folder(100, 2, &[1]);
    let worker: Script = Box::new(|conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN")?;
        let (tag, cmd) = conn.read_command()?;
        assert!(cmd.starts_with("SELECT"));
        write_select(conn, &tag, 100, 2, 1)?;
        let (tag, _) = conn.read_command()?;
        let header = format!(
            "* 1 FETCH (UID 1 FLAGS (\\Deleted) INTERNALDATE \"12-May-2025 10:00:00 +0000\" RFC822.SIZE {} BODY[] {{{}}}\r\n",
            MSG_BODY.len(),
            MSG_BODY.len()
        );
        conn.write_raw(header.as_bytes())?;
        conn.write_raw(MSG_BODY)?;
        conn.write_raw(b")\r\n")?;
        conn.write_line(&format!("{tag} OK"))?;
        drain_until_close(conn);
        Ok(())
    });
    let server = MockImap::start_scripts(vec![control, worker]);
    let archive = tempfile("include_deleted");
    let summary = run_import(&server, "alice", archive.clone(), |c| {
        c.include_deleted = true;
    })
    .expect("import");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == "email")
        .unwrap();
    assert_eq!(email.1.created, 1);
    let conn = Connection::open(&archive).unwrap();
    let kw: String = conn
        .query_row("SELECT keywords FROM emails LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert!(kw.contains("$deleted"));
}

#[test]
fn coordinator_noautomap_leaves_role_null_on_heuristic_match() {
    let control: Script = Box::new(|conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN")?;
        let (tag, _) = conn.read_command()?;
        conn.write_line("* LIST () \"/\" \"Sent Items\"")?;
        conn.write_line(&format!("{tag} OK"))?;
        let (tag, _) = conn.read_command()?;
        conn.write_line(&format!("{tag} OK"))?;
        let (tag, _) = conn.read_command()?;
        write_select(conn, &tag, 100, 1, 0)?;
        let (tag, _) = conn.read_command()?;
        conn.write_line("* SEARCH")?;
        conn.write_line(&format!("{tag} OK"))?;
        drain_until_close(conn);
        Ok(())
    });
    let server = MockImap::start_scripts(vec![
        control,
        worker_idle_script("IMAP4rev2 LITERAL+ AUTH=PLAIN"),
    ]);
    let archive = tempfile("noautomap");
    run_import(&server, "alice", archive.clone(), |c| {
        c.automap = false;
    })
    .expect("import");
    let conn = Connection::open(&archive).unwrap();
    let role: Option<String> = conn
        .query_row(
            "SELECT role FROM mailboxes WHERE name = 'Sent Items'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        role.is_none(),
        "noautomap should leave heuristic-only role NULL"
    );
}

#[test]
fn coordinator_subscribed_only_excludes_unsubscribed() {
    let control: Script = Box::new(|conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(
            conn,
            "IMAP4rev2 LIST-EXTENDED SPECIAL-USE LITERAL+ AUTH=PLAIN",
        )?;
        let (tag, _) = conn.read_command()?;
        conn.write_line("* LIST (\\Subscribed) \"/\" \"INBOX\"")?;
        conn.write_line("* LIST () \"/\" \"Other\"")?;
        conn.write_line(&format!("{tag} OK"))?;
        let (tag, _) = conn.read_command()?;
        write_select(conn, &tag, 100, 1, 0)?;
        let (tag, _) = conn.read_command()?;
        conn.write_line("* SEARCH")?;
        conn.write_line(&format!("{tag} OK"))?;
        drain_until_close(conn);
        Ok(())
    });
    let server = MockImap::start_scripts(vec![
        control,
        worker_idle_script("IMAP4rev2 LIST-EXTENDED SPECIAL-USE LITERAL+ AUTH=PLAIN"),
    ]);
    let archive = tempfile("subscribed_only");
    run_import(&server, "alice", archive.clone(), |c| {
        c.subscribed_only = true;
    })
    .expect("import");
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(count(&conn, "mailboxes"), 1);
    let n: String = conn
        .query_row("SELECT name FROM mailboxes LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, "INBOX");
}

#[test]
fn coordinator_imap4rev1_only_works() {
    let control: Script = Box::new(|conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(conn, "IMAP4rev1 LITERAL+ AUTH=PLAIN")?;
        let (tag, _) = conn.read_command()?;
        conn.write_line("* LIST () \"/\" \"INBOX\"")?;
        conn.write_line(&format!("{tag} OK"))?;
        let (tag, _) = conn.read_command()?;
        conn.write_line(&format!("{tag} OK"))?;
        let (tag, _) = conn.read_command()?;
        write_select(conn, &tag, 100, 1, 0)?;
        let (tag, _) = conn.read_command()?;
        conn.write_line("* SEARCH")?;
        conn.write_line(&format!("{tag} OK"))?;
        drain_until_close(conn);
        Ok(())
    });
    let server = MockImap::start_scripts(vec![
        control,
        worker_idle_script("IMAP4rev1 LITERAL+ AUTH=PLAIN"),
    ]);
    let archive = tempfile("imap4rev1");
    let summary = run_import(&server, "alice", archive, |_| {}).expect("import");
    assert!(!summary.any_failed());
}

#[test]
fn coordinator_enables_utf8_accept_when_advertised() {
    use std::sync::atomic::AtomicBool;
    static SAW_ENABLE: AtomicBool = AtomicBool::new(false);
    SAW_ENABLE.store(false, Ordering::SeqCst);

    let control: Script = Box::new(|conn: &mut MockConn| -> std::io::Result<()> {
        conn.write_line("* OK ready")?;
        let (tag, _) = conn.read_command()?;
        write_capability(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN ENABLE UTF8=ACCEPT")?;
        conn.write_line(&format!("{tag} OK done"))?;
        let (tag, _) = conn.read_command()?;
        write_capability(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN ENABLE UTF8=ACCEPT")?;
        conn.write_line(&format!("{tag} OK done"))?;
        let (tag, _) = conn.read_command()?;
        write_capability(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN ENABLE UTF8=ACCEPT")?;
        conn.write_line(&format!("{tag} OK done"))?;
        let (tag, cmd) = conn.read_command()?;
        assert!(cmd.starts_with("ENABLE"));
        SAW_ENABLE.store(true, Ordering::SeqCst);
        conn.write_line("* ENABLED UTF8=ACCEPT")?;
        conn.write_line(&format!("{tag} OK"))?;
        let (tag, _) = conn.read_command()?;
        conn.write_line("* LIST () \"/\" \"INBOX\"")?;
        conn.write_line(&format!("{tag} OK"))?;
        let (tag, _) = conn.read_command()?;
        conn.write_line(&format!("{tag} OK"))?;
        let (tag, _) = conn.read_command()?;
        write_select(conn, &tag, 100, 1, 0)?;
        let (tag, _) = conn.read_command()?;
        conn.write_line("* SEARCH")?;
        conn.write_line(&format!("{tag} OK"))?;
        drain_until_close(conn);
        Ok(())
    });
    let server = MockImap::start_scripts(vec![
        control,
        worker_idle_script("IMAP4rev2 LITERAL+ AUTH=PLAIN ENABLE UTF8=ACCEPT"),
    ]);
    let archive = tempfile("utf8accept");
    run_import(&server, "alice", archive, |_| {}).expect("import");
    assert!(SAW_ENABLE.load(Ordering::SeqCst), "ENABLE was issued");
}

#[test]
fn coordinator_skips_enable_when_utf8_accept_absent() {
    use std::sync::atomic::AtomicBool;
    static SAW_ENABLE: AtomicBool = AtomicBool::new(false);
    SAW_ENABLE.store(false, Ordering::SeqCst);

    let control: Script = Box::new(|conn: &mut MockConn| -> std::io::Result<()> {
        conn.write_line("* OK ready")?;
        let (tag, _) = conn.read_command()?;
        write_capability(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN")?;
        conn.write_line(&format!("{tag} OK done"))?;
        let (tag, _) = conn.read_command()?;
        write_capability(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN")?;
        conn.write_line(&format!("{tag} OK done"))?;
        let (tag, _) = conn.read_command()?;
        write_capability(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN")?;
        conn.write_line(&format!("{tag} OK done"))?;
        let (tag, cmd) = conn.read_command()?;
        if cmd.starts_with("ENABLE") {
            SAW_ENABLE.store(true, Ordering::SeqCst);
            conn.write_line(&format!("{tag} OK"))?;
            let (tag, _) = conn.read_command()?;
            conn.write_line("* LIST () \"/\" \"INBOX\"")?;
            conn.write_line(&format!("{tag} OK"))?;
        } else {
            conn.write_line("* LIST () \"/\" \"INBOX\"")?;
            conn.write_line(&format!("{tag} OK"))?;
        }
        let (tag, _) = conn.read_command()?;
        conn.write_line(&format!("{tag} OK"))?;
        let (tag, _) = conn.read_command()?;
        write_select(conn, &tag, 100, 1, 0)?;
        let (tag, _) = conn.read_command()?;
        conn.write_line("* SEARCH")?;
        conn.write_line(&format!("{tag} OK"))?;
        drain_until_close(conn);
        Ok(())
    });
    let server = MockImap::start_scripts(vec![
        control,
        worker_idle_script("IMAP4rev2 LITERAL+ AUTH=PLAIN"),
    ]);
    let archive = tempfile("noutf8");
    run_import(&server, "alice", archive, |_| {}).expect("import");
    assert!(
        !SAW_ENABLE.load(Ordering::SeqCst),
        "ENABLE must NOT be issued"
    );
}

#[test]
fn coordinator_inbox_casefold_lowercase_input() {
    let control: Script = Box::new(|conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN")?;
        let (tag, _) = conn.read_command()?;
        conn.write_line("* LIST () \"/\" \"Inbox\"")?;
        conn.write_line(&format!("{tag} OK"))?;
        let (tag, _) = conn.read_command()?;
        conn.write_line(&format!("{tag} OK"))?;
        let (tag, _) = conn.read_command()?;
        write_select(conn, &tag, 100, 1, 0)?;
        let (tag, _) = conn.read_command()?;
        conn.write_line("* SEARCH")?;
        conn.write_line(&format!("{tag} OK"))?;
        drain_until_close(conn);
        Ok(())
    });
    let server = MockImap::start_scripts(vec![
        control,
        worker_idle_script("IMAP4rev2 LITERAL+ AUTH=PLAIN"),
    ]);
    let archive = tempfile("inbox_casefold");
    run_import(&server, "alice", archive.clone(), |_| {}).expect("import");
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(folder_role(&conn, "INBOX"), Some("inbox".to_owned()));
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM mailboxes WHERE name = 'INBOX'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn coordinator_authenticationfailed_yields_exit2_connection_error() {
    let server = MockImap::start(|conn| {
        conn.write_line("* OK ready")?;
        let (tag, _) = conn.read_command()?;
        write_capability(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN")?;
        conn.write_line(&format!("{tag} OK done"))?;
        let (tag, _) = conn.read_command()?;
        conn.write_line(&format!("{tag} NO [AUTHENTICATIONFAILED] bad creds"))?;
        let _ = conn.read_command();
        Ok(())
    });
    let archive = tempfile("authfail");
    let err = run_import(&server, "alice", archive, |_| {}).unwrap_err();
    assert!(matches!(err, vandelay::error::Error::Connection(_)));
    assert_eq!(err.exit_code(), 2);
}

fn read_select_mailbox(conn: &mut MockConn) -> std::io::Result<(String, String)> {
    let (tag, cmd) = conn.read_command()?;
    let arg = cmd
        .strip_prefix("SELECT ")
        .unwrap_or_else(|| panic!("expected SELECT, got {cmd}"));
    if arg.starts_with('{') {
        let mut line = String::new();
        conn.reader.read_line(&mut line)?;
        Ok((tag, line.trim_end_matches(['\r', '\n']).to_owned()))
    } else {
        Ok((tag, arg.trim_matches('"').to_owned()))
    }
}

const TURKISH_SENT: &str = "Gönderilmiş Postalar";
const FRENCH_SENT_UTF8: &str = "Envoyés";
const FRENCH_SENT_MUTF7: &str = "Envoy&AOk-s";

#[test]
fn utf8_accept_server_gets_the_folder_name_back_as_utf8() {
    let control: Script = Box::new(|conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(conn, "IMAP4rev2 ENABLE UTF8=ACCEPT LITERAL+ AUTH=PLAIN")?;
        let (tag, cmd) = conn.read_command()?;
        assert_eq!(cmd, "LIST \"\" \"*\"");
        conn.write_line(&format!("* LIST () \"/\" \"{TURKISH_SENT}\""))?;
        conn.write_line(&format!("{tag} OK LIST done"))?;
        let (tag, _) = conn.read_command()?;
        conn.write_line(&format!("{tag} OK LSUB done"))?;
        let (tag, name) = read_select_mailbox(conn)?;
        assert_eq!(
            name, TURKISH_SENT,
            "an ENABLEd UTF8=ACCEPT server must be sent the name as UTF-8"
        );
        write_select(conn, &tag, 900, 2, 1)?;
        let (tag, cmd) = conn.read_command()?;
        assert_eq!(cmd, "UID SEARCH ALL");
        conn.write_line("* SEARCH 1")?;
        conn.write_line(&format!("{tag} OK SEARCH done"))?;
        drain_until_close(conn);
        Ok(())
    });
    let worker: Script = Box::new(|conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(conn, "IMAP4rev2 ENABLE UTF8=ACCEPT LITERAL+ AUTH=PLAIN")?;
        let (tag, name) = read_select_mailbox(conn)?;
        assert_eq!(
            name, TURKISH_SENT,
            "the fetch worker must agree with the coordinator"
        );
        write_select(conn, &tag, 900, 2, 1)?;
        let (tag, _) = conn.read_command()?;
        write_fetch_message(conn, 1, 1, MSG_BODY)?;
        conn.write_line(&format!("{tag} OK FETCH done"))?;
        drain_until_close(conn);
        Ok(())
    });

    let server = MockImap::start_scripts(vec![control, worker]);
    let archive = tempfile("utf8_accept_name");
    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("import");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == "email")
        .unwrap();
    assert_eq!(email.1.failed, 0, "summary={summary:?}");
    assert_eq!(email.1.created, 1);
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(
        conn.query_row::<String, _, _>("SELECT name FROM mailboxes", [], |r| r.get(0))
            .unwrap(),
        TURKISH_SENT,
        "the folder name is stored as UTF-8, not as Latin-1 mojibake"
    );
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn mutf7_server_gets_the_folder_name_back_as_mutf7() {
    let control: Script = Box::new(|conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN")?;
        let (tag, cmd) = conn.read_command()?;
        assert_eq!(cmd, "LIST \"\" \"*\"");
        conn.write_line(&format!("* LIST () \"/\" \"{FRENCH_SENT_MUTF7}\""))?;
        conn.write_line(&format!("{tag} OK LIST done"))?;
        let (tag, _) = conn.read_command()?;
        conn.write_line(&format!("{tag} OK LSUB done"))?;
        let (tag, name) = read_select_mailbox(conn)?;
        assert_eq!(
            name, FRENCH_SENT_MUTF7,
            "a server without UTF8=ACCEPT must be sent modified UTF-7"
        );
        write_select(conn, &tag, 901, 1, 0)?;
        let (tag, cmd) = conn.read_command()?;
        assert_eq!(cmd, "UID SEARCH ALL");
        conn.write_line("* SEARCH")?;
        conn.write_line(&format!("{tag} OK SEARCH done"))?;
        drain_until_close(conn);
        Ok(())
    });

    let server = MockImap::start_scripts(vec![control, worker_idle_script("IMAP4rev2 LITERAL+")]);
    let archive = tempfile("mutf7_name");
    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("import");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == "email")
        .unwrap();
    assert_eq!(email.1.failed, 0, "summary={summary:?}");
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(
        conn.query_row::<String, _, _>("SELECT name FROM mailboxes", [], |r| r.get(0))
            .unwrap(),
        FRENCH_SENT_UTF8
    );
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn utf8_name_from_a_server_that_never_enabled_utf8_falls_back_on_select() {
    let control: Script = Box::new(|conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN")?;
        let (tag, cmd) = conn.read_command()?;
        assert_eq!(cmd, "LIST \"\" \"*\"");
        conn.write_line(&format!("* LIST () \"/\" \"{FRENCH_SENT_UTF8}\""))?;
        conn.write_line(&format!("{tag} OK LIST done"))?;
        let (tag, _) = conn.read_command()?;
        conn.write_line(&format!("{tag} OK LSUB done"))?;
        let (tag, name) = read_select_mailbox(conn)?;
        assert_eq!(name, FRENCH_SENT_MUTF7);
        conn.write_line(&format!("{tag} NO [NONEXISTENT] Mailbox does not exist."))?;
        let (tag, name) = read_select_mailbox(conn)?;
        assert_eq!(
            name, FRENCH_SENT_UTF8,
            "a refused modified UTF-7 name must be retried as UTF-8"
        );
        write_select(conn, &tag, 902, 2, 1)?;
        let (tag, cmd) = conn.read_command()?;
        assert_eq!(cmd, "UID SEARCH ALL");
        conn.write_line("* SEARCH 1")?;
        conn.write_line(&format!("{tag} OK SEARCH done"))?;
        drain_until_close(conn);
        Ok(())
    });
    let worker: Script = Box::new(|conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN")?;
        let (tag, name) = read_select_mailbox(conn)?;
        assert_eq!(name, FRENCH_SENT_MUTF7);
        conn.write_line(&format!("{tag} NO [NONEXISTENT] Mailbox does not exist."))?;
        let (tag, name) = read_select_mailbox(conn)?;
        assert_eq!(name, FRENCH_SENT_UTF8);
        write_select(conn, &tag, 902, 2, 1)?;
        let (tag, _) = conn.read_command()?;
        write_fetch_message(conn, 1, 1, MSG_BODY)?;
        conn.write_line(&format!("{tag} OK FETCH done"))?;
        drain_until_close(conn);
        Ok(())
    });

    let server = MockImap::start_scripts(vec![control, worker]);
    let archive = tempfile("utf8_no_enable");
    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("import");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == "email")
        .unwrap();
    assert_eq!(email.1.failed, 0, "summary={summary:?}");
    assert_eq!(email.1.created, 1);
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(
        conn.query_row::<String, _, _>("SELECT name FROM mailboxes", [], |r| r.get(0))
            .unwrap(),
        FRENCH_SENT_UTF8
    );
    let _ = std::fs::remove_file(&archive);
}
