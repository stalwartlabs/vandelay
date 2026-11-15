/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod common;

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use common::line_server::{LineServer, MockConn, Script};
use encodify::base64::STANDARD;
use rusqlite::Connection;
use vandelay::archive::Archive;
use vandelay::archive::sources::SourceChange;
use vandelay::error::ExitStatus;
use vandelay::imap::automap::Automap;
use vandelay::imap::client::ImapClient;
use vandelay::imap::command::Command;
use vandelay::imap::error::ImapError;
use vandelay::imap::session::Compress;
use vandelay::import::imap::DeletedMessages;
use vandelay::import::imap::FolderScope;
use vandelay::import::imap::MimeKeywords;
use vandelay::import::imap::{ImapImport, SaslAuth};
use vandelay::logging::{Level, Logger};
use vandelay::net::http::TlsPolicy;
use vandelay::net::sasl::SaslPayload;
use vandelay::net::stream::Cleartext;
use vandelay::net::stream::ConnectMode;
use vandelay::net::stream::Connector;
use vandelay::sync::{RunMode, RunOptions};
use vandelay::types::ObjectType;

struct MockImap(LineServer);

impl MockImap {
    fn start<H>(handler: H) -> MockImap
    where
        H: FnOnce(&mut MockConn) -> std::io::Result<()> + Send + 'static,
    {
        Self::start_scripts(vec![Box::new(handler)])
    }

    fn start_scripts(scripts: Vec<Script>) -> MockImap {
        MockImap(LineServer::start(scripts, "* BYE no script left"))
    }

    fn url(&self) -> String {
        self.0.url("imap")
    }
}

impl MockConn {
    fn read_command(&mut self) -> std::io::Result<(String, String)> {
        let line = self.read_line()?;
        Ok(match line.split_once(' ') {
            Some((tag, rest)) => (tag.to_owned(), rest.to_owned()),
            None => (line, String::new()),
        })
    }
}

fn cleartext_connector() -> Connector {
    Connector::new(TlsPolicy::AcceptInvalid).expect("connector")
}

fn connect_mock(server: &MockImap) -> Result<ImapClient, ImapError> {
    ImapClient::connect(
        &cleartext_connector(),
        "127.0.0.1",
        server.0.port,
        ConnectMode::Plain,
        Logger::new(Level::Default),
    )
}

fn run_import(
    server: &MockImap,
    user: &str,
    archive: PathBuf,
    config_tweak: impl FnOnce(&mut ImapImport),
) -> Result<vandelay::sync::Summary, vandelay::error::Error> {
    run_import_in(server, user, archive, RunMode::Apply, config_tweak)
}

fn run_import_in(
    server: &MockImap,
    user: &str,
    archive: PathBuf,
    mode: RunMode,
    config_tweak: impl FnOnce(&mut ImapImport),
) -> Result<vandelay::sync::Summary, vandelay::error::Error> {
    let common = RunOptions {
        archive: archive.clone(),
        threads: 1,
        mode,
        max_retries: 1,
        tls: TlsPolicy::Verify,
        logger: Logger::new(Level::Quiet),
    };
    let mut config = ImapImport {
        url: server.url(),
        auth: SaslAuth::Basic {
            user: user.to_owned(),
            password: "p@ss".to_owned(),
        },
        cleartext: Cleartext::Allow,
        compress: Compress::Off,
        include: Vec::new(),
        exclude: Vec::new(),
        exclude_special: Vec::new(),
        folder: Vec::new(),
        scope: FolderScope::All,
        automap: Automap::Enabled,
        deleted: DeletedMessages::Skip,
        mime_keywords: MimeKeywords::Skip,
        fetch_batch: 256,
        imap_connections: 1,
        source_change: SourceChange::Reject,
    };
    config_tweak(&mut config);
    config.run(common).into_result()
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
    assert!(client.capabilities().contains("IMAP4rev2"));
    assert!(client.capabilities().contains("AUTH=PLAIN"));
    assert!(client.capabilities().contains("LITERAL+"));
    assert!(!client.capabilities().contains("XOAUTH2"));
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
        .authenticate(SaslPayload::OAuthBearer {
            user: "alice@example.com",
            token: "ya29.token",
        })
        .expect("oauthbearer auth");
}

#[test]
fn authenticate_oauthbearer_continuation_payload_uses_gs2_header() {
    let server = MockImap::start(|conn| {
        conn.write_line("* OK Hello")?;
        let (tag, _) = conn.read_command()?;
        conn.write_line("* CAPABILITY IMAP4rev2 AUTH=OAUTHBEARER")?;
        conn.write_line(&format!("{tag} OK done"))?;
        let (tag, cmd) = conn.read_command()?;
        assert_eq!(cmd, "AUTHENTICATE OAUTHBEARER");
        conn.write_line("+ ")?;
        let payload = conn.read_line()?;
        let decoded = STANDARD.decode(&payload).expect("base64");
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
        .authenticate(SaslPayload::OAuthBearer {
            user: "alice@example.com",
            token: "ya29.token",
        })
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
    client
        .authenticate(SaslPayload::Plain {
            authcid: "alice",
            password: "p@ss",
        })
        .expect("auth");
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
        let payload = conn.read_line()?;
        assert!(!payload.is_empty(), "client should send the base64 payload");
        conn.write_line(&format!("{tag} OK welcome"))?;
        let _ = conn.read_command();
        Ok(())
    });
    let mut client = connect_mock(&server).expect("connect");
    client
        .authenticate(SaslPayload::Plain {
            authcid: "alice",
            password: "p@ss",
        })
        .expect("auth");
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
    let err = client
        .authenticate(SaslPayload::Plain {
            authcid: "alice",
            password: "p@ss",
        })
        .unwrap_err();
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
        .run_collect(&Command::Raw("UID FETCH 5 (UID BODY.PEEK[])"))
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

fn serve_one_folder(
    conn: &mut MockConn,
    uidvalidity: u32,
    uidnext: u32,
    uids: &[u32],
) -> std::io::Result<()> {
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
}

fn control_script_one_folder(uidvalidity: u32, uidnext: u32, uids: &'static [u32]) -> Script {
    Box::new(move |conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN")?;
        serve_one_folder(conn, uidvalidity, uidnext, uids)
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
    let archive = common::tmp_archive("happy");
    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("import");
    let mailbox = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == ObjectType::Mailbox)
        .unwrap();
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == ObjectType::Email)
        .unwrap();
    assert_eq!(mailbox.1.created, 1);
    assert_eq!(email.1.created, 1, "summary={summary:?}");
    assert_eq!(email.1.fetched, 1);
    let db = Archive::open(&archive).unwrap();
    let conn = db.connection();
    assert_eq!(common::count(conn, "mailboxes"), 1);
    assert_eq!(common::count(conn, "emails"), 1);
    assert_eq!(common::count(conn, "blobs"), 1);
    assert_eq!(folder_role(conn, "INBOX"), Some("inbox".to_owned()));
}

const DOVECOT_PRE_LOGIN_CAPS: &str =
    "IMAP4rev1 SASL-IR LOGIN-REFERRALS ID ENABLE IDLE LITERAL+ AUTH=PLAIN AUTH=LOGIN";
const DOVECOT_POST_LOGIN_CAPS: &str = "IMAP4rev1 SASL-IR LOGIN-REFERRALS ID ENABLE IDLE SORT \
    UIDPLUS LITERAL+ NOTIFY IMAPSIEVE=sieve://127.0.0.1:4190 \
    QUOTA ACL RIGHTS=texk";

#[test]
fn coordinator_accepts_dovecot_post_login_capability_with_imapsieve_url() {
    let control: Script = Box::new(|conn: &mut MockConn| -> std::io::Result<()> {
        conn.write_line(&format!(
            "* OK [CAPABILITY {DOVECOT_PRE_LOGIN_CAPS}] Dovecot (Debian) ready."
        ))?;
        let (tag, cmd) = conn.read_command()?;
        assert_eq!(cmd, "CAPABILITY");
        write_capability(conn, DOVECOT_PRE_LOGIN_CAPS)?;
        conn.write_line(&format!(
            "{tag} OK Pre-login capabilities listed, post-login capabilities have more."
        ))?;
        let (tag, cmd) = conn.read_command()?;
        assert!(
            cmd.starts_with("AUTHENTICATE PLAIN "),
            "expected SASL-IR form, got {cmd}"
        );
        write_capability(conn, DOVECOT_POST_LOGIN_CAPS)?;
        conn.write_line(&format!(
            "{tag} OK [CAPABILITY {DOVECOT_POST_LOGIN_CAPS}] Logged in"
        ))?;
        let (tag, cmd) = conn.read_command()?;
        assert_eq!(cmd, "CAPABILITY");
        write_capability(conn, DOVECOT_POST_LOGIN_CAPS)?;
        conn.write_line(&format!("{tag} OK Capability completed."))?;
        serve_one_folder(conn, 12345, 2, &[1])
    });
    let server = MockImap::start_scripts(vec![
        control,
        worker_fetch_script(
            "IMAP4rev1 SASL-IR LITERAL+ AUTH=PLAIN IMAPSIEVE=sieve://127.0.0.1:4190",
            "INBOX",
            12345,
            2,
            1,
            vec![(1, 1, MSG_BODY)],
        ),
    ]);
    let archive = common::tmp_archive("dovecot-imapsieve");
    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("import");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == ObjectType::Email)
        .unwrap();
    assert_eq!(email.1.created, 1, "summary={summary:?}");
    let db = Archive::open(&archive).unwrap();
    let conn = db.connection();
    assert_eq!(common::count(conn, "emails"), 1);
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
    let archive = common::tmp_archive("esearch");
    let _ = run_import(&server, "alice", archive.clone(), |_| {}).expect("import");
    let conn = Connection::open(&archive).unwrap();
    assert!(common::count(&conn, "emails") >= 1);
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
    let archive = common::tmp_archive("esearch_fallback");
    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("import");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == ObjectType::Email)
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
    let archive = common::tmp_archive("search_fallback");
    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("import");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == ObjectType::Email)
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
    let archive = common::tmp_archive("size_mismatch");
    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("import");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == ObjectType::Email)
        .unwrap();
    assert_eq!(email.1.skipped, 0);
    assert_eq!(email.1.created, 1);
    let dbc = Connection::open(&archive).unwrap();
    assert_eq!(common::count(&dbc, "emails"), 1);
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
    let archive = common::tmp_archive("uidvalidity");
    run_import(&server, "alice", archive.clone(), |_| {}).expect("first import");
    let dbc = Connection::open(&archive).unwrap();
    assert_eq!(common::count(&dbc, "emails"), 1);
    drop(dbc);

    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("second import");
    let dbc = Connection::open(&archive).unwrap();
    assert_eq!(common::count(&dbc, "emails"), 1);
    assert_eq!(common::count(&dbc, "blobs"), 1);
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
        .find(|(k, _)| *k == ObjectType::Email)
        .unwrap()
        .1
        .clone();
    assert_eq!(email_after.deleted, 1, "stale UV emails wiped");
    assert_eq!(email_after.created, 1, "re-imported under new UV");
}

#[test]
fn coordinator_vanished_message_leaves_no_orphan_blob() {
    // R14: the IMAP importer runs blob GC after a clean run
    let mut scripts: Vec<Script> = Vec::new();
    scripts.extend(single_inbox_scripts(100, 2, MSG_BODY));
    scripts.push(control_script_one_folder(100, 2, &[]));
    scripts.push(worker_idle_script("IMAP4rev2 LITERAL+ AUTH=PLAIN"));
    let server = MockImap::start_scripts(scripts);
    let archive = common::tmp_archive("orphan_blob");
    run_import(&server, "alice", archive.clone(), |_| {}).expect("first import");
    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("second import");
    assert_eq!(summary.get(ObjectType::Email).unwrap().deleted, 1);
    let dbc = Connection::open(&archive).unwrap();
    assert_eq!(common::count(&dbc, "emails"), 0);
    assert_eq!(common::count(&dbc, "blobs"), 0);
}

#[test]
fn coordinator_dry_run_after_import_predicts_nothing_new() {
    // R7: a dry run diffs against the registered source, not an empty one
    let mut scripts: Vec<Script> = Vec::new();
    scripts.extend(single_inbox_scripts(100, 2, MSG_BODY));
    scripts.push(control_script_one_folder(100, 2, &[1]));
    let server = MockImap::start_scripts(scripts);
    let archive = common::tmp_archive("dry_after_import");
    run_import(&server, "alice", archive.clone(), |_| {}).expect("first import");
    let summary =
        run_import_in(&server, "alice", archive.clone(), RunMode::DryRun, |_| {}).expect("dry run");
    let mut summary = summary;
    let mailbox = *summary.plan(ObjectType::Mailbox);
    let email = *summary.plan(ObjectType::Email);
    assert_eq!((mailbox.new, mailbox.vanished, mailbox.present), (0, 0, 1));
    assert_eq!((email.new, email.vanished, email.present), (0, 0, 1));
}

#[test]
fn coordinator_present_run_is_convergent() {
    let mut scripts: Vec<Script> = Vec::new();
    scripts.extend(single_inbox_scripts(100, 2, MSG_BODY));

    scripts.push(control_script_present_flags(100, 2, &[1], &[(1, "\\Seen")]));
    scripts.push(worker_idle_script("IMAP4rev2 LITERAL+ AUTH=PLAIN"));
    let server = MockImap::start_scripts(scripts);
    let archive = common::tmp_archive("converge");
    run_import(&server, "alice", archive.clone(), |_| {}).expect("first import");
    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("second import");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == ObjectType::Email)
        .unwrap();
    assert_eq!(email.1.created, 0, "convergent run creates nothing");
    assert_eq!(email.1.deleted, 0, "convergent run deletes nothing");
    assert_eq!(email.1.updated, 0, "unchanged flags update nothing");
    let dbc = Connection::open(&archive).unwrap();
    assert_eq!(
        common::count(&dbc, "blobs"),
        1,
        "no body re-fetched on a present-only run"
    );
}

#[test]
fn coordinator_deletes_only_the_folders_the_server_no_longer_lists() {
    // a mapped folder absent from LIST is swept; listed folders stay
    let mut scripts: Vec<Script> = Vec::new();
    scripts.extend(single_inbox_scripts(100, 2, MSG_BODY));
    scripts.push(control_script_present_flags(100, 2, &[1], &[(1, "\\Seen")]));
    scripts.push(worker_idle_script("IMAP4rev2 LITERAL+ AUTH=PLAIN"));
    let server = MockImap::start_scripts(scripts);
    let archive = common::tmp_archive("vanished_folder");
    run_import(&server, "alice", archive.clone(), |_| {}).expect("first import");
    let dbc = Connection::open(&archive).unwrap();
    dbc.execute_batch(
        "INSERT INTO mailboxes (id, name) VALUES (900, 'Old');
         INSERT INTO sync_id_imap (source_id, type_name, folder, uidvalidity, uid, local_id)
         SELECT id, 'mailbox', 'Old', 0, 0, 900 FROM sources;",
    )
    .unwrap();
    drop(dbc);

    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("second import");
    let dbc = Connection::open(&archive).unwrap();
    let names: Vec<String> = dbc
        .prepare("SELECT name FROM mailboxes ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(names, ["INBOX"]);
    assert_eq!(summary.get(ObjectType::Mailbox).unwrap().deleted, 1);
    assert_eq!(common::count(&dbc, "emails"), 1);
}

#[test]
fn coordinator_treats_a_zero_fetch_batch_as_one() {
    // --fetch-batch 0 is clamped to 1 instead of panicking in the chunker
    let server = MockImap::start_scripts(single_inbox_scripts(100, 2, MSG_BODY));
    let archive = common::tmp_archive("fetch_batch_zero");
    let summary =
        run_import(&server, "alice", archive.clone(), |c| c.fetch_batch = 0).expect("import");
    assert_eq!(summary.get(ObjectType::Email).unwrap().created, 1);
}

#[test]
fn coordinator_fetches_new_uids_in_fetch_batch_sized_chunks() {
    // ten new UIDs with --fetch-batch 3 go out as 1:3, 4:6, 7:9 and 10
    static SETS: Mutex<Vec<String>> = Mutex::new(Vec::new());
    SETS.lock().unwrap().clear();
    let worker: Script = Box::new(|conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN")?;
        loop {
            let Ok((tag, cmd)) = conn.read_command() else {
                return Ok(());
            };
            if cmd.starts_with("SELECT") {
                write_select(conn, &tag, 100, 11, 10)?;
            } else if let Some(rest) = cmd.strip_prefix("UID FETCH ") {
                let set = rest.split_whitespace().next().unwrap_or("");
                SETS.lock().unwrap().push(set.to_owned());
                for (seq, uid) in parse_uid_set(set).into_iter().enumerate() {
                    write_fetch_message(conn, seq as u32 + 1, uid, MSG_BODY)?;
                }
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
        control_script_one_folder(100, 11, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]),
        worker,
    ]);
    let archive = common::tmp_archive("fetch_batch_chunks");
    let summary =
        run_import(&server, "alice", archive.clone(), |c| c.fetch_batch = 3).expect("import");
    assert_eq!(summary.get(ObjectType::Email).unwrap().created, 10);
    assert_eq!(*SETS.lock().unwrap(), ["1:3", "4:6", "7:9", "10"]);
}

#[test]
fn coordinator_counts_no_flag_updates_when_a_later_flag_batch_fails() {
    // a BAD on the second flag batch rolls back the first, so nothing counts as updated
    let mut scripts: Vec<Script> = vec![
        control_script_one_folder(100, 3, &[1, 2]),
        worker_fetch_script(
            "IMAP4rev2 LITERAL+ AUTH=PLAIN",
            "INBOX",
            100,
            3,
            2,
            vec![(1, 1, MSG_BODY), (2, 2, MSG_BODY_KEYWORDS)],
        ),
    ];
    scripts.push(Box::new(|conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN")?;
        let (tag, _) = conn.read_command()?;
        conn.write_line("* LIST () \"/\" \"INBOX\"")?;
        conn.write_line(&format!("{tag} OK LIST done"))?;
        let (tag, _) = conn.read_command()?;
        conn.write_line(&format!("{tag} OK LSUB done"))?;
        let (tag, _) = conn.read_command()?;
        write_select(conn, &tag, 100, 3, 2)?;
        let (tag, _) = conn.read_command()?;
        conn.write_line("* SEARCH 1 2")?;
        conn.write_line(&format!("{tag} OK SEARCH done"))?;
        let (tag, cmd) = conn.read_command()?;
        assert!(cmd.starts_with("UID FETCH 1 "), "got {cmd}");
        conn.write_line("* 1 FETCH (UID 1 FLAGS (\\Seen \\Flagged))")?;
        conn.write_line(&format!("{tag} OK FETCH done"))?;
        let (tag, cmd) = conn.read_command()?;
        assert!(cmd.starts_with("UID FETCH 2 "), "got {cmd}");
        conn.write_line(&format!("{tag} BAD flags unavailable"))?;
        drain_until_close(conn);
        Ok(())
    }));
    scripts.push(worker_idle_script("IMAP4rev2 LITERAL+ AUTH=PLAIN"));
    let server = MockImap::start_scripts(scripts);
    let archive = common::tmp_archive("flag_batch_fails");
    run_import(&server, "alice", archive.clone(), |_| {}).expect("first import");
    let result = run_import(&server, "alice", archive.clone(), |c| c.fetch_batch = 1);
    let summary = match result {
        Ok(summary) => summary,
        Err(e) => panic!("a failed flag batch is per folder: {e:?}"),
    };
    let email = summary.get(ObjectType::Email).unwrap();
    assert_eq!(email.updated, 0, "{summary:?}");
    assert!(summary.any_failed());
    let dbc = Connection::open(&archive).unwrap();
    let flagged: i64 = dbc
        .query_row(
            "SELECT count(*) FROM emails WHERE keywords LIKE '%$flagged%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(flagged, 0, "batch 1 was rolled back");
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
    let archive = common::tmp_archive("flagupdate");
    run_import(&server, "alice", archive.clone(), |_| {}).expect("first import");
    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("second import");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == ObjectType::Email)
        .unwrap();
    assert_eq!(
        email.1.updated, 1,
        "a changed flag set is counted as updated"
    );
    assert_eq!(email.1.created, 0, "present message is not re-created");
    assert_eq!(email.1.fetched, 0, "no body fetched on a present-only run");
    let dbc = Connection::open(&archive).unwrap();
    assert_eq!(common::count(&dbc, "blobs"), 1, "body not re-fetched");
    let kw: String = dbc
        .query_row("SELECT keywords FROM emails LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert!(
        kw.contains("$seen") && kw.contains("$flagged"),
        "keywords should reflect the new \\Flagged: {kw}"
    );
}

const MSG_BODY_KEYWORDS: &[u8] = b"From: a@b\r\nTo: c@d\r\nSubject: hi\r\nMessage-ID: <m2@h>\r\nKeywords: bearbeitet von KL, VIP\r\nDate: Mon, 12 May 2025 10:00:00 +0000\r\n\r\nhello";

fn stored_keywords(archive: &PathBuf) -> Vec<String> {
    let dbc = Connection::open(archive).unwrap();
    let kw: String = dbc
        .query_row("SELECT keywords FROM emails LIMIT 1", [], |r| r.get(0))
        .unwrap();
    let mut kws: Vec<String> = serde_json::from_str(&kw).unwrap();
    kws.sort_unstable();
    kws
}

// With --mime-keywords the Keywords header is imported and survives a later IMAP flag refresh.
#[test]
fn coordinator_mime_keywords_imported_and_kept_on_flag_refresh() {
    let mut scripts: Vec<Script> = Vec::new();
    scripts.extend(single_inbox_scripts(100, 2, MSG_BODY_KEYWORDS));
    scripts.push(control_script_present_flags(
        100,
        2,
        &[1],
        &[(1, "\\Seen \\Flagged")],
    ));
    scripts.push(worker_idle_script("IMAP4rev2 LITERAL+ AUTH=PLAIN"));
    let server = MockImap::start_scripts(scripts);
    let archive = common::tmp_archive("mime_keywords");
    run_import(&server, "alice", archive.clone(), |c| {
        c.mime_keywords = MimeKeywords::Extract
    })
    .expect("first import");
    let first = stored_keywords(&archive);
    assert!(
        first.contains(&"bearbeitet_von_kl".to_owned()) && first.contains(&"vip".to_owned()),
        "{first:?}"
    );
    let summary = run_import(&server, "alice", archive.clone(), |c| {
        c.mime_keywords = MimeKeywords::Extract
    })
    .expect("second import");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == ObjectType::Email)
        .unwrap();
    assert_eq!(email.1.created, 0);
    assert_eq!(email.1.updated, 1);
    let mut expected = first;
    expected.push("$flagged".to_owned());
    expected.sort_unstable();
    assert_eq!(stored_keywords(&archive), expected);
}

// Without --mime-keywords the Keywords header is ignored.
#[test]
fn coordinator_ignores_mime_keywords_by_default() {
    let server = MockImap::start_scripts(single_inbox_scripts(100, 2, MSG_BODY_KEYWORDS));
    let archive = common::tmp_archive("mime_keywords_off");
    run_import(&server, "alice", archive.clone(), |_| {}).expect("import");
    let kws = stored_keywords(&archive);
    assert!(
        !kws.iter().any(|k| k == "vip" || k == "bearbeitet_von_kl"),
        "{kws:?}"
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
    let archive = common::tmp_archive("presentdeleted");
    run_import(&server, "alice", archive.clone(), |_| {}).expect("first import");
    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("second import");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == ObjectType::Email)
        .unwrap();
    assert_eq!(
        email.1.updated, 0,
        "a present message that newly gained \\Deleted is skipped, not updated"
    );
    let dbc = Connection::open(&archive).unwrap();
    assert_eq!(
        common::count(&dbc, "emails"),
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
    let archive = common::tmp_archive("specialuse");
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
    let archive = common::tmp_archive("nospecialuse");
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
    let archive = common::tmp_archive("deleted");
    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("import");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == ObjectType::Email)
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
    let archive = common::tmp_archive("srcchange");
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
    let archive = common::tmp_archive("parallel");
    let summary = run_import(&server, "alice", archive.clone(), |c| {
        c.imap_connections = 3;
        c.fetch_batch = 3;
    })
    .expect("import");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == ObjectType::Email)
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
    let archive = common::tmp_archive("liststatus");
    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("import");

    let n = SELECTS.load(Ordering::SeqCst);
    assert_eq!(n, 1, "expected exactly 1 control-side SELECT, got {n}");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == ObjectType::Email)
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
    let archive = common::tmp_archive("noop");
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
    let archive = common::tmp_archive("retry");
    let summary =
        run_import(&server, "alice", archive.clone(), |_| {}).expect("import succeeds via retry");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == ObjectType::Email)
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
    let archive = common::tmp_archive("plain_to_login");
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
    let archive = common::tmp_archive("dual_refuse");
    let err = run_import(&server, "alice", archive, |_| {}).unwrap_err();
    assert!(matches!(err, vandelay::error::Error::Connection(_)));
    assert_eq!(err.exit_status(), ExitStatus::Connection);
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
    let archive = common::tmp_archive("select_no");
    let summary = run_import(&server, "alice", archive, |_| {}).expect("import");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == ObjectType::Email)
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
    let archive = common::tmp_archive("include_deleted");
    let summary = run_import(&server, "alice", archive.clone(), |c| {
        c.deleted = DeletedMessages::Keep;
    })
    .expect("import");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == ObjectType::Email)
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
    let archive = common::tmp_archive("noautomap");
    run_import(&server, "alice", archive.clone(), |c| {
        c.automap = Automap::Disabled;
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
    let archive = common::tmp_archive("subscribed_only");
    run_import(&server, "alice", archive.clone(), |c| {
        c.scope = FolderScope::Subscribed;
    })
    .expect("import");
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(common::count(&conn, "mailboxes"), 1);
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
    let archive = common::tmp_archive("imap4rev1");
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
    let archive = common::tmp_archive("utf8accept");
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
    let archive = common::tmp_archive("noutf8");
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
    let archive = common::tmp_archive("inbox_casefold");
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
    let archive = common::tmp_archive("authfail");
    let err = run_import(&server, "alice", archive, |_| {}).unwrap_err();
    assert!(matches!(err, vandelay::error::Error::Connection(_)));
    assert_eq!(err.exit_status(), ExitStatus::Connection);
}

fn read_select_mailbox(conn: &mut MockConn) -> std::io::Result<(String, String)> {
    let (tag, cmd) = conn.read_command()?;
    let arg = cmd
        .strip_prefix("SELECT ")
        .unwrap_or_else(|| panic!("expected SELECT, got {cmd}"));
    if arg.starts_with('{') {
        Ok((tag, conn.read_line()?))
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
    let archive = common::tmp_archive("utf8_accept_name");
    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("import");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == ObjectType::Email)
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
    let archive = common::tmp_archive("mutf7_name");
    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("import");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == ObjectType::Email)
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
fn utf8_name_from_a_server_that_never_enabled_utf8_is_selected_as_listed() {
    assert_name_selected_as_listed(FRENCH_SENT_UTF8, FRENCH_SENT_UTF8, "utf8_no_enable");
}

#[test]
fn raw_ampersand_name_is_selected_as_listed_not_reencoded() {
    assert_name_selected_as_listed("R&D", "R&D", "raw_ampersand");
}

#[test]
fn modified_utf7_name_is_selected_as_listed() {
    assert_name_selected_as_listed(FRENCH_SENT_MUTF7, FRENCH_SENT_UTF8, "mutf7_as_listed");
}

fn assert_name_selected_as_listed(listed: &'static str, stored: &str, archive_name: &str) {
    let control: Script = Box::new(move |conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN")?;
        let (tag, cmd) = conn.read_command()?;
        assert_eq!(cmd, "LIST \"\" \"*\"");
        conn.write_line(&format!("* LIST () \"/\" \"{listed}\""))?;
        conn.write_line(&format!("{tag} OK LIST done"))?;
        let (tag, _) = conn.read_command()?;
        conn.write_line(&format!("{tag} OK LSUB done"))?;
        let (tag, name) = read_select_mailbox(conn)?;
        assert_eq!(
            name, listed,
            "the name must go back exactly as the server listed it"
        );
        write_select(conn, &tag, 902, 2, 1)?;
        let (tag, cmd) = conn.read_command()?;
        assert_eq!(cmd, "UID SEARCH ALL");
        conn.write_line("* SEARCH 1")?;
        conn.write_line(&format!("{tag} OK SEARCH done"))?;
        drain_until_close(conn);
        Ok(())
    });
    let worker: Script = Box::new(move |conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN")?;
        let (tag, name) = read_select_mailbox(conn)?;
        assert_eq!(
            name, listed,
            "the fetch worker must agree with the coordinator"
        );
        write_select(conn, &tag, 902, 2, 1)?;
        let (tag, _) = conn.read_command()?;
        write_fetch_message(conn, 1, 1, MSG_BODY)?;
        conn.write_line(&format!("{tag} OK FETCH done"))?;
        drain_until_close(conn);
        Ok(())
    });

    let server = MockImap::start_scripts(vec![control, worker]);
    let archive = common::tmp_archive(archive_name);
    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("import");
    let email = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == ObjectType::Email)
        .unwrap();
    assert_eq!(email.1.failed, 0, "summary={summary:?}");
    assert_eq!(email.1.created, 1);
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(
        conn.query_row::<String, _, _>("SELECT name FROM mailboxes", [], |r| r.get(0))
            .unwrap(),
        stored
    );
    let _ = std::fs::remove_file(&archive);
}

const NOSELECT_LIST: &[&str] = &[
    "* LIST (\\HasChildren) \"/\" \"INBOX\"",
    "* LIST (\\Noselect \\HasChildren) \"/\" \"INBOX/Projects\"",
    "* LIST (\\HasNoChildren) \"/\" \"INBOX/Projects/Invoices\"",
    "* LIST (\\Noselect \\HasChildren) \"/\" \"A\"",
    "* LIST (\\HasNoChildren) \"/\" \"A/B/Reports\"",
    "* LIST (\\Noselect \\HasChildren) \"/\" \"Clients \"",
    "* LIST (\\HasNoChildren) \"/\" \"Clients /Reports\"",
    "* LIST (\\HasChildren) \"/\" \"Clients\"",
    "* LIST (\\HasNoChildren) \"/\" \"Clients/Reports\"",
];

const NOSELECT_SELECTABLE: &[&str] = &[
    "INBOX",
    "INBOX/Projects/Invoices",
    "A/B/Reports",
    "Clients /Reports",
    "Clients",
    "Clients/Reports",
];

fn noselect_control_script(selected: std::sync::Arc<Mutex<Vec<String>>>) -> Script {
    Box::new(move |conn: &mut MockConn| -> std::io::Result<()> {
        auth_preamble(conn, "IMAP4rev2 LITERAL+ AUTH=PLAIN")?;
        while let Ok((tag, cmd)) = conn.read_command() {
            if cmd.starts_with("LIST") {
                for line in NOSELECT_LIST {
                    conn.write_line(line)?;
                }
                conn.write_line(&format!("{tag} OK LIST done"))?;
            } else if let Some(name) = cmd
                .strip_prefix("SELECT ")
                .or_else(|| cmd.strip_prefix("EXAMINE "))
            {
                selected
                    .lock()
                    .expect("selected")
                    .push(name.trim_matches('"').to_owned());
                write_select(conn, &tag, 7, 1, 0)?;
            } else if cmd.starts_with("UID SEARCH") {
                conn.write_line("* SEARCH")?;
                conn.write_line(&format!("{tag} OK SEARCH done"))?;
            } else if cmd.starts_with("LOGOUT") {
                conn.write_line("* BYE bye")?;
                conn.write_line(&format!("{tag} OK"))?;
                break;
            } else {
                assert!(
                    !cmd.starts_with("STATUS") && !cmd.contains("FETCH"),
                    "unexpected {cmd}"
                );
                conn.write_line(&format!("{tag} OK"))?;
            }
        }
        Ok(())
    })
}

fn mailbox_tree(conn: &Connection) -> Vec<(i64, String, Option<String>)> {
    let mut stmt = conn
        .prepare(
            "SELECT m.id, s.folder, p.name FROM mailboxes m
             JOIN sync_id_imap s ON s.local_id = m.id AND s.type_name = 'mailbox'
             LEFT JOIN mailboxes p ON p.id = m.parent_id
             ORDER BY s.folder",
        )
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn assert_noselect_tree(conn: &Connection) {
    let tree = mailbox_tree(conn);
    let pairs: Vec<(&str, Option<&str>)> = tree
        .iter()
        .map(|(_, f, p)| (f.as_str(), p.as_deref()))
        .collect();
    assert_eq!(
        pairs,
        [
            ("A", None),
            ("A/B", Some("A")),
            ("A/B/Reports", Some("B")),
            ("Clients", None),
            ("Clients ", None),
            ("Clients /Reports", Some("Clients ")),
            ("Clients/Reports", Some("Clients")),
            ("INBOX", None),
            ("INBOX/Projects", Some("INBOX")),
            ("INBOX/Projects/Invoices", Some("Projects")),
        ]
    );
    let roles: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM mailboxes WHERE role IS NOT NULL AND name IN ('Projects', 'A', 'B', 'Clients ')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(roles, 0);
}

fn assert_only_selectable_selected(selected: &Mutex<Vec<String>>) {
    let mut got = selected.lock().expect("selected").clone();
    got.sort_unstable();
    let mut want: Vec<String> = NOSELECT_SELECTABLE
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    want.sort_unstable();
    assert_eq!(got, want);
}

#[test]
fn coordinator_keeps_noselect_ancestors_and_never_selects_them() {
    let first = std::sync::Arc::new(Mutex::new(Vec::new()));
    let second = std::sync::Arc::new(Mutex::new(Vec::new()));
    let server = MockImap::start_scripts(vec![
        noselect_control_script(first.clone()),
        noselect_control_script(second.clone()),
    ]);
    let archive = common::tmp_archive("noselect");
    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("import");
    let mailbox = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == ObjectType::Mailbox)
        .unwrap();
    assert_eq!(mailbox.1.created, 10);
    assert_eq!(mailbox.1.failed, 0);
    assert_only_selectable_selected(&first);
    let before = {
        let conn = Connection::open(&archive).unwrap();
        assert_noselect_tree(&conn);
        mailbox_tree(&conn)
    };

    let summary = run_import(&server, "alice", archive.clone(), |_| {}).expect("reimport");
    let mailbox = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == ObjectType::Mailbox)
        .unwrap();
    assert_eq!((mailbox.1.created, mailbox.1.deleted), (0, 0));
    assert_only_selectable_selected(&second);
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(mailbox_tree(&conn), before);
}

#[test]
fn coordinator_heals_archive_missing_noselect_parents() {
    let first = std::sync::Arc::new(Mutex::new(Vec::new()));
    let second = std::sync::Arc::new(Mutex::new(Vec::new()));
    let server = MockImap::start_scripts(vec![
        noselect_control_script(first),
        noselect_control_script(second),
    ]);
    let archive = common::tmp_archive("noselect_heal");
    run_import(&server, "alice", archive.clone(), |_| {}).expect("import");
    let child_id = {
        let conn = Connection::open(&archive).unwrap();
        let tree = mailbox_tree(&conn);
        let child = tree
            .iter()
            .find(|(_, f, _)| f == "INBOX/Projects/Invoices")
            .unwrap()
            .0;
        // simulate an archive written before structural parents were kept
        conn.execute(
            "UPDATE mailboxes SET parent_id = NULL WHERE id = ?1",
            [child],
        )
        .unwrap();
        let parent: i64 = conn
            .query_row(
                "SELECT local_id FROM sync_id_imap WHERE type_name = 'mailbox' AND folder = 'INBOX/Projects'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "DELETE FROM sync_id_imap WHERE type_name = 'mailbox' AND folder = 'INBOX/Projects'",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM mailboxes WHERE id = ?1", [parent])
            .unwrap();
        child
    };
    run_import(&server, "alice", archive.clone(), |_| {}).expect("reimport");
    let conn = Connection::open(&archive).unwrap();
    assert_noselect_tree(&conn);
    let healed = mailbox_tree(&conn)
        .into_iter()
        .find(|(_, f, _)| f == "INBOX/Projects/Invoices")
        .unwrap()
        .0;
    assert_eq!(healed, child_id);
}

#[test]
fn dry_run_never_selects_noselect_ancestors() {
    let selected = std::sync::Arc::new(Mutex::new(Vec::new()));
    let server = MockImap::start_scripts(vec![noselect_control_script(selected.clone())]);
    let archive = common::tmp_archive("noselect_dry");
    let common = RunOptions {
        archive: archive.clone(),
        threads: 1,
        mode: RunMode::DryRun,
        max_retries: 1,
        tls: TlsPolicy::Verify,
        logger: Logger::new(Level::Quiet),
    };
    let config = ImapImport {
        url: server.url(),
        auth: SaslAuth::Basic {
            user: "alice".to_owned(),
            password: "p@ss".to_owned(),
        },
        cleartext: Cleartext::Allow,
        compress: Compress::Off,
        include: Vec::new(),
        exclude: Vec::new(),
        exclude_special: Vec::new(),
        folder: Vec::new(),
        scope: FolderScope::All,
        automap: Automap::Enabled,
        deleted: DeletedMessages::Skip,
        mime_keywords: MimeKeywords::Skip,
        fetch_batch: 256,
        imap_connections: 1,
        source_change: SourceChange::Reject,
    };
    let mut summary = config.run(common).into_result().expect("dry run");
    assert_eq!(summary.plan(ObjectType::Mailbox).new, 10);
    assert_only_selectable_selected(&selected);
}
