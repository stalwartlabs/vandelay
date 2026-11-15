/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod common;

use std::path::{Path, PathBuf};

use common::line_server::{LineServer, MockConn, Script};
use rusqlite::Connection;
use vandelay::archive::sources::SourceChange;
use vandelay::import::managesieve::{ManageSieveImport, SaslAuth};
use vandelay::logging::{Level, Logger};
use vandelay::net::http::TlsPolicy;
use vandelay::net::stream::Cleartext;
use vandelay::sync::{RunMode, RunOptions};
use vandelay::types::ObjectType;

struct MockSieveServer(LineServer);

impl MockSieveServer {
    fn start<H>(handler: H) -> MockSieveServer
    where
        H: FnOnce(&mut MockConn) -> std::io::Result<()> + Send + 'static,
    {
        Self::start_scripts(vec![Box::new(handler)])
    }

    fn start_scripts(scripts: Vec<Script>) -> MockSieveServer {
        MockSieveServer(LineServer::start(scripts, "BYE \"no script left\""))
    }

    fn url(&self) -> String {
        self.0.url("sieve")
    }
}

impl MockConn {
    fn write_capability(&mut self, sasl: &str, starttls: bool) -> std::io::Result<()> {
        self.write_line("\"IMPLEMENTATION\" \"Mock ManageSieve\"")?;
        self.write_line("\"VERSION\" \"1.0\"")?;
        if starttls {
            self.write_line("\"STARTTLS\"")?;
        }
        self.write_line(&format!("\"SASL\" \"{sasl}\""))?;
        self.write_line("\"SIEVE\" \"fileinto vacation\"")?;
        Ok(())
    }
}

fn run_basic(
    server: &MockSieveServer,
    archive: &Path,
) -> Result<vandelay::sync::Summary, vandelay::error::Error> {
    let common = RunOptions {
        archive: archive.to_path_buf(),
        threads: 1,
        mode: RunMode::Apply,
        max_retries: 1,
        tls: TlsPolicy::Verify,
        logger: Logger::new(Level::Quiet),
    };
    let config = ManageSieveImport {
        url: server.url(),
        auth: SaslAuth::Basic {
            user: "alice".to_owned(),
            password: "p@ss".to_owned(),
        },
        cleartext: Cleartext::Allow,
        source_change: SourceChange::Reject,
    };
    config.run(common).into_result()
}

#[test]
fn capability_after_auth_is_consumed_when_present() {
    let server = MockSieveServer::start(|conn| {
        conn.write_capability("PLAIN", false)?;
        conn.write_line("OK \"hi\"")?;
        let cmd = conn.read_line()?;
        assert!(cmd.starts_with("AUTHENTICATE \"PLAIN\""), "got {cmd}");

        conn.write_line("OK")?;

        let cmd = conn.read_line()?;
        assert!(cmd.starts_with("CAPABILITY"), "got {cmd}");
        conn.write_capability("PLAIN", false)?;
        conn.write_line("OK")?;

        let cmd = conn.read_line()?;
        assert!(cmd.starts_with("LISTSCRIPTS"), "got {cmd}");
        conn.write_line("OK")?;

        let _ = conn.read_line();
        conn.write_line("OK \"bye\"")?;
        Ok(())
    });
    let archive = common::tmp_archive("caps_after_auth");
    let summary = run_basic(&server, &archive).expect("import");
    assert!(!summary.any_failed());
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn sasl_plain_succeeds_when_advertised() {
    let server = MockSieveServer::start(|conn| {
        conn.write_capability("PLAIN", false)?;
        conn.write_line("OK")?;
        let cmd = conn.read_line()?;
        assert!(cmd.starts_with("AUTHENTICATE \"PLAIN\""), "got {cmd}");
        conn.write_line("OK \"auth ok\"")?;

        let _ = conn.read_line();
        conn.write_capability("PLAIN", false)?;
        conn.write_line("OK")?;
        let _ = conn.read_line();
        conn.write_line("OK")?;
        let _ = conn.read_line();
        conn.write_line("OK \"bye\"")?;
        Ok(())
    });
    let archive = common::tmp_archive("plain");
    let summary = run_basic(&server, &archive).expect("import");
    assert!(!summary.any_failed());
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn sasl_login_fallback_when_plain_rejected() {
    let server = MockSieveServer::start(|conn| {
        conn.write_capability("PLAIN LOGIN", false)?;
        conn.write_line("OK")?;
        let cmd = conn.read_line()?;
        assert!(cmd.starts_with("AUTHENTICATE \"PLAIN\""), "got {cmd}");

        conn.write_line("NO \"PLAIN not allowed here\"")?;

        let cmd = conn.read_line()?;
        assert!(
            cmd.starts_with("AUTHENTICATE \"LOGIN\""),
            "expected LOGIN fallback, got {cmd}"
        );

        conn.write_line("\"VXNlcm5hbWU6\"")?;
        let _user = conn.read_line()?;

        conn.write_line("\"UGFzc3dvcmQ6\"")?;
        let _pass = conn.read_line()?;
        conn.write_line("OK \"welcome\"")?;
        let _ = conn.read_line();
        conn.write_capability("PLAIN LOGIN", false)?;
        conn.write_line("OK")?;
        let _ = conn.read_line();
        conn.write_line("OK")?;
        let _ = conn.read_line();
        conn.write_line("OK \"bye\"")?;
        Ok(())
    });
    let archive = common::tmp_archive("login_fallback");
    let summary = run_basic(&server, &archive).expect("import");
    assert!(!summary.any_failed());
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn auth_no_translates_to_connection_error() {
    let server = MockSieveServer::start(|conn| {
        conn.write_capability("PLAIN", false)?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_line("NO \"bad creds\"")?;
        Ok(())
    });
    let archive = common::tmp_archive("auth_fail");
    let err = run_basic(&server, &archive).unwrap_err();
    match err {
        vandelay::error::Error::Connection(msg) => assert!(
            msg.contains("auth failed") || msg.contains("LOGIN"),
            "got {msg}"
        ),
        other => panic!("expected Connection, got {other:?}"),
    }
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn server_lacking_plain_login_for_basic_is_refused() {
    let server = MockSieveServer::start(|conn| {
        conn.write_capability("OAUTHBEARER", false)?;
        conn.write_line("OK")?;
        Ok(())
    });
    let archive = common::tmp_archive("no_basic_mech");
    let err = run_basic(&server, &archive).unwrap_err();
    assert!(matches!(err, vandelay::error::Error::Connection(_)));
    let _ = std::fs::remove_file(&archive);
}

fn auth_then<F>(conn: &mut MockConn, after_auth: F) -> std::io::Result<()>
where
    F: FnOnce(&mut MockConn) -> std::io::Result<()>,
{
    conn.write_capability("PLAIN", false)?;
    conn.write_line("OK")?;
    let _ = conn.read_line()?;
    conn.write_line("OK")?;
    let _ = conn.read_line()?;
    conn.write_capability("PLAIN", false)?;
    conn.write_line("OK")?;
    after_auth(conn)?;
    let _ = conn.read_line();
    conn.write_line("OK \"bye\"")?;
    Ok(())
}

#[test]
fn empty_listscripts_imports_zero_rows() {
    let server = MockSieveServer::start(|conn| {
        auth_then(conn, |c| {
            let cmd = c.read_line()?;
            assert!(cmd.starts_with("LISTSCRIPTS"));
            c.write_line("OK")?;
            Ok(())
        })
    });
    let archive = common::tmp_archive("empty_list");
    let summary = run_basic(&server, &archive).expect("import");
    assert!(!summary.any_failed());
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(common::count(&conn, "sieve_scripts"), 0);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn malformed_listscripts_line_is_a_listscripts_parse_error() {
    // a LISTSCRIPTS data line without a script name aborts with the parse text
    let server = MockSieveServer::start(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("(\"x\")")?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    let archive = common::tmp_archive("listscripts_parse");
    let err = run_basic(&server, &archive).unwrap_err();
    match err {
        vandelay::error::Error::Connection(msg) => assert!(
            msg.starts_with(
                "LISTSCRIPTS parse: response parse error: LISTSCRIPTS line missing name"
            ),
            "got {msg}"
        ),
        other => panic!("expected Connection, got {other:?}"),
    }
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn listscripts_with_one_active_imports_with_active_flag() {
    let script = b"require [\"fileinto\"];\nfileinto \"INBOX\";\n";
    let script_owned = script.to_vec();
    let server = MockSieveServer::start(move |conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"vacation\" ACTIVE")?;
            c.write_line("\"other\"")?;
            c.write_line("OK")?;

            let _ = c.read_line()?;
            c.write_line(&format!("{{{}}}", script_owned.len()))?;
            c.write_raw(&script_owned)?;
            c.write_raw(b"\r\n")?;
            c.write_line("OK")?;

            let _ = c.read_line()?;
            c.write_line(&format!("{{{}}}", script_owned.len()))?;
            c.write_raw(&script_owned)?;
            c.write_raw(b"\r\n")?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    let archive = common::tmp_archive("active");
    let summary = run_basic(&server, &archive).expect("import");
    assert!(!summary.any_failed(), "{summary:?}");
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(common::count(&conn, "sieve_scripts"), 2);
    let active_name: String = conn
        .query_row(
            "SELECT name FROM sieve_scripts WHERE is_active = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(active_name, "vacation");
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn getscript_quoted_body_is_imported_verbatim() {
    let server = MockSieveServer::start(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"oneliner\"")?;
            c.write_line("OK")?;
            let _ = c.read_line()?;
            c.write_line("\"stop;\"")?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    let archive = common::tmp_archive("quoted_body");
    let summary = run_basic(&server, &archive).expect("import");
    assert!(!summary.any_failed());
    let conn = Connection::open(&archive).unwrap();
    let bytes: Vec<u8> = conn
        .query_row(
            "SELECT b.data FROM blobs b JOIN sieve_scripts s ON s.blob_id=b.id LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(bytes, b"stop;");
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn getscript_nonexistent_is_skipped_and_counted() {
    let server = MockSieveServer::start(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"ok-one\"")?;
            c.write_line("\"missing\"")?;
            c.write_line("OK")?;

            let _ = c.read_line()?;
            c.write_line("\"keep;\"")?;
            c.write_line("OK")?;

            let _ = c.read_line()?;
            c.write_line("NO (NONEXISTENT) \"vanished\"")?;
            Ok(())
        })
    });
    let archive = common::tmp_archive("nonexistent");
    let summary = run_basic(&server, &archive).expect("import");
    assert!(summary.any_failed(), "expected at least one failure");
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(common::count(&conn, "sieve_scripts"), 1);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn abort_after_a_skipped_script_keeps_the_counts() {
    // R15: a referral after one counted failure aborts, and the outcome still carries the count.
    let server = MockSieveServer::start(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"missing\"")?;
            c.write_line("\"moved\"")?;
            c.write_line("OK")?;

            let _ = c.read_line()?;
            c.write_line("NO (NONEXISTENT) \"vanished\"")?;

            let _ = c.read_line()?;
            c.write_line("NO (REFERRAL \"sieve://other.example/\") \"go\"")?;
            Ok(())
        })
    });
    let archive = common::tmp_archive("abort_counts");
    let options = RunOptions {
        archive: archive.clone(),
        threads: 1,
        mode: RunMode::Apply,
        max_retries: 1,
        tls: TlsPolicy::Verify,
        logger: Logger::new(Level::Quiet),
    };
    let outcome = ManageSieveImport {
        url: server.url(),
        auth: SaslAuth::Basic {
            user: "alice".to_owned(),
            password: "p@ss".to_owned(),
        },
        cleartext: Cleartext::Allow,
        source_change: SourceChange::Reject,
    }
    .run(options);
    assert!(
        matches!(&outcome.error, Some(vandelay::error::Error::Connection(msg)) if msg.contains("referral")),
        "{:?}",
        outcome.error
    );
    assert_eq!(
        outcome
            .summary
            .get(ObjectType::SieveScript)
            .map(|c| c.failed),
        Some(1)
    );
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn referral_aborts_run_with_connection_error() {
    let server = MockSieveServer::start(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"x\"")?;
            c.write_line("OK")?;
            let _ = c.read_line()?;
            c.write_line("NO (REFERRAL \"sieve://other.example/\") \"go\"")?;
            Ok(())
        })
    });
    let archive = common::tmp_archive("referral");
    let err = run_basic(&server, &archive).unwrap_err();
    match err {
        vandelay::error::Error::Connection(msg) => assert!(msg.contains("referral"), "{msg}"),
        other => panic!("expected Connection, got {other:?}"),
    }
    let _ = std::fs::remove_file(&archive);
}

fn seed_then_run(seed: Script, second: Script) -> (MockSieveServer, PathBuf) {
    let server = MockSieveServer::start_scripts(vec![seed, second]);
    let archive = common::tmp_archive("multi");
    (server, archive)
}

fn run_against(
    server: &MockSieveServer,
    archive: &Path,
) -> Result<vandelay::sync::Summary, vandelay::error::Error> {
    run_basic(server, archive)
}

fn seed_one_then(seed_name: &str, seed_bytes: &str, second: Script) -> (MockSieveServer, PathBuf) {
    let seed_name_owned = seed_name.to_owned();
    let seed_bytes_owned = seed_bytes.to_owned();
    let seed: Script = Box::new(move |conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line(&format!("\"{seed_name_owned}\""))?;
            c.write_line("OK")?;
            let _ = c.read_line()?;
            c.write_line(&format!("\"{seed_bytes_owned}\""))?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    seed_then_run(seed, second)
}

#[test]
fn present_unchanged_script_does_not_rewrite_blob() {
    let second: Script = Box::new(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"a\"")?;
            c.write_line("OK")?;
            let _ = c.read_line()?;
            c.write_line("\"stop;\"")?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    let (server, archive) = seed_one_then("a", "stop;", second);
    let _ = run_against(&server, &archive).expect("seed");
    let summary = run_against(&server, &archive).expect("reconcile");
    let (_, c) = &summary.per_type[0];
    assert_eq!(c.created, 0);
    assert_eq!(c.deleted, 0);
    assert_eq!(c.skipped, 1, "present unchanged should count as skipped");
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn present_content_updated_swaps_blob_pointer() {
    let second: Script = Box::new(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"a\"")?;
            c.write_line("OK")?;
            let _ = c.read_line()?;
            c.write_line("\"v2\"")?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    let (server, archive) = seed_one_then("a", "v1", second);
    let _ = run_against(&server, &archive).expect("seed");
    let summary = run_against(&server, &archive).expect("reconcile");
    let (_, c) = &summary.per_type[0];
    assert_eq!(c.fetched, 1, "content update should count as fetched");
    let conn = Connection::open(&archive).unwrap();
    let bytes: Vec<u8> = conn
        .query_row(
            "SELECT b.data FROM blobs b JOIN sieve_scripts s ON s.blob_id=b.id WHERE s.name='a'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(bytes, b"v2");
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn vanished_script_is_deleted_locally() {
    let second: Script = Box::new(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    let (server, archive) = seed_one_then("a", "x", second);
    let _ = run_against(&server, &archive).expect("seed");
    let summary = run_against(&server, &archive).expect("reconcile");
    let (_, c) = &summary.per_type[0];
    assert_eq!(c.deleted, 1, "vanished script should be counted deleted");
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(common::count(&conn, "sieve_scripts"), 0);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn active_flag_flip_does_not_violate_partial_unique_index() {
    let seed: Script = Box::new(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"a\" ACTIVE")?;
            c.write_line("OK")?;
            let _ = c.read_line()?;
            c.write_line("\"x\"")?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    let second: Script = Box::new(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"a\"")?;
            c.write_line("\"b\" ACTIVE")?;
            c.write_line("OK")?;

            let _ = c.read_line()?;
            c.write_line("\"x\"")?;
            c.write_line("OK")?;

            let _ = c.read_line()?;
            c.write_line("\"y\"")?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    let (server, archive) = seed_then_run(seed, second);
    let _ = run_against(&server, &archive).expect("seed");
    let summary = run_against(&server, &archive).expect("reconcile");
    assert!(!summary.any_failed(), "{summary:?}");
    let conn = Connection::open(&archive).unwrap();
    let active: String = conn
        .query_row(
            "SELECT name FROM sieve_scripts WHERE is_active = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(active, "b");
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn resume_after_partial_run_completes_remainder() {
    let seed: Script = Box::new(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"a\"")?;
            c.write_line("OK")?;
            let _ = c.read_line()?;
            c.write_line("\"first\"")?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    let second: Script = Box::new(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"a\"")?;
            c.write_line("\"b\"")?;
            c.write_line("OK")?;
            let _ = c.read_line()?;
            c.write_line("\"first\"")?;
            c.write_line("OK")?;
            let _ = c.read_line()?;
            c.write_line("\"second\"")?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    let (server, archive) = seed_then_run(seed, second);
    let _ = run_against(&server, &archive).expect("seed");
    let summary = run_against(&server, &archive).expect("second");
    let (_, c) = &summary.per_type[0];
    assert_eq!(c.created, 1);
    assert_eq!(c.skipped, 1);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn second_run_is_convergent() {
    let s1: Script = Box::new(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"only\" ACTIVE")?;
            c.write_line("OK")?;
            let _ = c.read_line()?;
            c.write_line("\"keep;\"")?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    let s2: Script = Box::new(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"only\" ACTIVE")?;
            c.write_line("OK")?;
            let _ = c.read_line()?;
            c.write_line("\"keep;\"")?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    let (server, archive) = seed_then_run(s1, s2);
    let _ = run_against(&server, &archive).expect("first");
    let summary = run_against(&server, &archive).expect("second");
    let (_, c) = &summary.per_type[0];
    assert_eq!(c.created, 0);
    assert_eq!(c.fetched, 0);
    assert_eq!(c.deleted, 0);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn transient_no_on_getscript_retries_then_succeeds() {
    let archive = common::tmp_archive("transient");
    let server = MockSieveServer::start(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"a\"")?;
            c.write_line("OK")?;

            let _ = c.read_line()?;
            c.write_line("NO \"try again later\"")?;

            let _ = c.read_line()?;
            c.write_line("\"keep;\"")?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    let common = RunOptions {
        archive: archive.to_path_buf(),
        threads: 1,
        mode: RunMode::Apply,
        max_retries: 3,
        tls: TlsPolicy::Verify,
        logger: Logger::new(Level::Quiet),
    };
    let config = ManageSieveImport {
        url: server.url(),
        auth: SaslAuth::Basic {
            user: "alice".to_owned(),
            password: "p@ss".to_owned(),
        },
        cleartext: Cleartext::Allow,
        source_change: SourceChange::Reject,
    };
    let summary = config.run(common).into_result().expect("import");
    assert!(!summary.any_failed());
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(common::count(&conn, "sieve_scripts"), 1);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn bye_mid_listscripts_reconnects_and_succeeds() {
    let first: Script = Box::new(|conn| {
        conn.write_capability("PLAIN", false)?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_capability("PLAIN", false)?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_line("BYE \"server restarting\"")?;
        Ok(())
    });
    let second: Script = Box::new(|conn| {
        conn.write_capability("PLAIN", false)?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_capability("PLAIN", false)?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_line("OK")?;
        let _ = conn.read_line();
        conn.write_line("OK \"bye\"")?;
        Ok(())
    });
    let server = MockSieveServer::start_scripts(vec![first, second]);
    let archive = common::tmp_archive("bye_listscripts");
    let common = RunOptions {
        archive: archive.clone(),
        threads: 1,
        mode: RunMode::Apply,
        max_retries: 3,
        tls: TlsPolicy::Verify,
        logger: Logger::new(Level::Quiet),
    };
    let config = ManageSieveImport {
        url: server.url(),
        auth: SaslAuth::Basic {
            user: "alice".to_owned(),
            password: "p@ss".to_owned(),
        },
        cleartext: Cleartext::Allow,
        source_change: SourceChange::Reject,
    };
    let summary = config.run(common).into_result().expect("import");
    assert!(!summary.any_failed(), "{summary:?}");
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(common::count(&conn, "sieve_scripts"), 0);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn bye_mid_getscript_reconnects_and_completes_remaining_scripts() {
    let first: Script = Box::new(|conn| {
        conn.write_capability("PLAIN", false)?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_capability("PLAIN", false)?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_line("\"a\"")?;
        conn.write_line("\"b\"")?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_line("\"keep;\"")?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_line("BYE \"server restarting\"")?;
        Ok(())
    });
    let second: Script = Box::new(|conn| {
        conn.write_capability("PLAIN", false)?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_capability("PLAIN", false)?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_line("\"keep;\"")?;
        conn.write_line("OK")?;
        let _ = conn.read_line();
        conn.write_line("OK \"bye\"")?;
        Ok(())
    });
    let server = MockSieveServer::start_scripts(vec![first, second]);
    let archive = common::tmp_archive("bye_getscript");
    let common = RunOptions {
        archive: archive.clone(),
        threads: 1,
        mode: RunMode::Apply,
        max_retries: 3,
        tls: TlsPolicy::Verify,
        logger: Logger::new(Level::Quiet),
    };
    let config = ManageSieveImport {
        url: server.url(),
        auth: SaslAuth::Basic {
            user: "alice".to_owned(),
            password: "p@ss".to_owned(),
        },
        cleartext: Cleartext::Allow,
        source_change: SourceChange::Reject,
    };
    let summary = config.run(common).into_result().expect("import");
    assert!(!summary.any_failed(), "{summary:?}");
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(common::count(&conn, "sieve_scripts"), 2);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn post_auth_unsolicited_capability_is_consumed() {
    let server = MockSieveServer::start(|conn| {
        conn.write_capability("PLAIN", false)?;
        conn.write_line("OK")?;
        let _ = conn.read_line()?;
        conn.write_capability("PLAIN", false)?;
        conn.write_line("OK")?;
        let cmd = conn.read_line()?;
        assert!(
            cmd.starts_with("LISTSCRIPTS"),
            "expected LISTSCRIPTS directly (no CAPABILITY refresh), got {cmd}"
        );
        conn.write_line("OK")?;
        let _ = conn.read_line();
        conn.write_line("OK \"bye\"")?;
        Ok(())
    });
    let archive = common::tmp_archive("post_auth_caps");
    let summary = run_basic(&server, &archive).expect("import");
    assert!(!summary.any_failed());
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn dry_run_does_not_mutate_archive() {
    let server = MockSieveServer::start(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"a\"")?;
            c.write_line("OK")?;
            let _ = c.read_line()?;
            c.write_line("\"keep;\"")?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    let archive = common::tmp_archive("dryrun");
    let common = RunOptions {
        archive: archive.to_path_buf(),
        threads: 1,
        mode: RunMode::DryRun,
        max_retries: 1,
        tls: TlsPolicy::Verify,
        logger: Logger::new(Level::Quiet),
    };
    let config = ManageSieveImport {
        url: server.url(),
        auth: SaslAuth::Basic {
            user: "alice".to_owned(),
            password: "p@ss".to_owned(),
        },
        cleartext: Cleartext::Allow,
        source_change: SourceChange::Reject,
    };
    let mut summary = config.run(common).into_result().expect("dryrun");
    assert_eq!(summary.plan(ObjectType::SieveScript).new, 1);
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(
        common::count(&conn, "sieve_scripts"),
        0,
        "dry-run must not write"
    );
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn oauthbearer_authenticates_end_to_end_when_advertised() {
    let server = MockSieveServer::start(|conn| {
        conn.write_capability("OAUTHBEARER", false)?;
        conn.write_line("OK")?;
        let cmd = conn.read_line()?;
        assert!(
            cmd.starts_with("AUTHENTICATE \"OAUTHBEARER\""),
            "expected OAUTHBEARER, got {cmd}"
        );
        conn.write_line("OK \"hi\"")?;
        let _ = conn.read_line();
        conn.write_capability("OAUTHBEARER", false)?;
        conn.write_line("OK")?;
        let cmd = conn.read_line()?;
        assert!(cmd.starts_with("LISTSCRIPTS"), "got {cmd}");
        conn.write_line("OK")?;
        let _ = conn.read_line();
        conn.write_line("OK \"bye\"")?;
        Ok(())
    });
    let archive = common::tmp_archive("oauthbearer");
    let common = RunOptions {
        archive: archive.to_path_buf(),
        threads: 1,
        mode: RunMode::Apply,
        max_retries: 1,
        tls: TlsPolicy::Verify,
        logger: Logger::new(Level::Quiet),
    };
    let config = ManageSieveImport {
        url: server.url(),
        auth: SaslAuth::Bearer {
            user: "alice@example.com".to_owned(),
            token: "tok-xyz".to_owned(),
        },
        cleartext: Cleartext::Allow,
        source_change: SourceChange::Reject,
    };
    let summary = config.run(common).into_result().expect("import");
    assert!(!summary.any_failed(), "{summary:?}");
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn plain_then_login_both_refused_aborts_run_with_combined_message() {
    let server = MockSieveServer::start(|conn| {
        conn.write_capability("PLAIN LOGIN", false)?;
        conn.write_line("OK")?;
        let cmd = conn.read_line()?;
        assert!(cmd.starts_with("AUTHENTICATE \"PLAIN\""), "{cmd}");
        conn.write_line("NO \"plain refused\"")?;
        let cmd = conn.read_line()?;
        assert!(cmd.starts_with("AUTHENTICATE \"LOGIN\""), "{cmd}");

        conn.write_line("\"VXNlcm5hbWU6\"")?;
        let _ = conn.read_line()?;

        conn.write_line("\"UGFzc3dvcmQ6\"")?;
        let _ = conn.read_line()?;
        conn.write_line("NO \"login refused too\"")?;
        Ok(())
    });
    let archive = common::tmp_archive("plain_and_login_refused");
    let err = run_basic(&server, &archive).unwrap_err();
    match err {
        vandelay::error::Error::Connection(msg) => {
            assert!(
                msg.contains("LOGIN") && msg.contains("PLAIN"),
                "expected combined LOGIN/PLAIN error text, got {msg}"
            );
        }
        other => panic!("expected Connection, got {other:?}"),
    }
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn three_scripts_with_middle_active_lands_the_middle_active() {
    let server = MockSieveServer::start(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"first\"")?;
            c.write_line("\"middle\" ACTIVE")?;
            c.write_line("\"third\"")?;
            c.write_line("OK")?;
            for body in &["one", "two", "three"] {
                let _ = c.read_line()?;
                c.write_line(&format!("\"{body}\""))?;
                c.write_line("OK")?;
            }
            Ok(())
        })
    });
    let archive = common::tmp_archive("middle_active");
    let summary = run_basic(&server, &archive).expect("import");
    assert!(!summary.any_failed(), "{summary:?}");
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(common::count(&conn, "sieve_scripts"), 3);
    let active: String = conn
        .query_row(
            "SELECT name FROM sieve_scripts WHERE is_active = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(active, "middle");
    let total_active: i64 = conn
        .query_row(
            "SELECT count(*) FROM sieve_scripts WHERE is_active = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(total_active, 1);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn transient_no_exhausting_max_retries_skips_script_and_followup_picks_it_up() {
    let archive = common::tmp_archive("retry_exhaustion");
    let first = MockSieveServer::start(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"flaky\"")?;
            c.write_line("OK")?;

            for _ in 0..3 {
                let _ = c.read_line()?;
                c.write_line("NO \"try again later\"")?;
            }
            Ok(())
        })
    });
    let common = |archive: &Path| RunOptions {
        archive: archive.to_path_buf(),
        threads: 1,
        mode: RunMode::Apply,
        max_retries: 2,
        tls: TlsPolicy::Verify,
        logger: Logger::new(Level::Quiet),
    };
    let cfg = |url: String| ManageSieveImport {
        url,
        auth: SaslAuth::Basic {
            user: "alice".to_owned(),
            password: "p@ss".to_owned(),
        },
        cleartext: Cleartext::Allow,
        source_change: SourceChange::Reject,
    };
    let summary = cfg(first.url())
        .run(common(&archive))
        .into_result()
        .expect("first run");
    assert!(
        summary.any_failed(),
        "expected per-script failure to count toward exit 5: {summary:?}"
    );
    let (_, c) = &summary.per_type[0];
    assert_eq!(c.failed, 1, "exactly one failure recorded");
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(
        common::count(&conn, "sieve_scripts"),
        0,
        "the script must NOT have landed locally"
    );
    drop(conn);

    let second = MockSieveServer::start(|conn| {
        auth_then(conn, |c| {
            let _ = c.read_line()?;
            c.write_line("\"flaky\"")?;
            c.write_line("OK")?;
            let _ = c.read_line()?;
            c.write_line("\"keep;\"")?;
            c.write_line("OK")?;
            Ok(())
        })
    });
    let mut cfg2 = cfg(second.url());
    cfg2.source_change = SourceChange::Allow;
    let summary2 = cfg2
        .run(common(&archive))
        .into_result()
        .expect("second run");
    assert!(!summary2.any_failed(), "{summary2:?}");
    let (_, c2) = &summary2.per_type[0];
    assert_eq!(c2.created, 1);
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(common::count(&conn, "sieve_scripts"), 1);
    let _ = std::fs::remove_file(&archive);
}
