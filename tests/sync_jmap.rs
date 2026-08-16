/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod integration;
mod seeder;

use std::path::{Path, PathBuf};

use integration::stalwart::shared as shared_stalwart;
use rusqlite::Connection;
use serde_json::{Map, Value, json};
use vandelay::jmap::account::{self, AccountSelector};
use vandelay::jmap::http::{Auth, HttpClient, RetryPolicy};
use vandelay::jmap::request::Request;
use vandelay::jmap::session::Session;
use vandelay::logging::Logger;
use vandelay::sync::{self, CommonConfig, ConnectConfig, ExportConfig, ImportConfig};

fn base_url() -> &'static str {
    shared_stalwart().base_url()
}

fn tmp_archive(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "vandelay-{tag}-{}-{}.sqlite",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

fn common(archive: &Path, dry_run: bool) -> CommonConfig {
    CommonConfig {
        archive: archive.to_path_buf(),
        threads: 4,
        dry_run,
        max_retries: 5,
        allow_invalid_certs: true,
        logger: Logger::from_flags(false, 0),
    }
}

fn basic(localpart: &str) -> Auth {
    Auth::Basic {
        user: format!("{localpart}@{}", seeder::DOMAIN),
        password: seeder::USER_PASSWORD.to_owned(),
    }
}

fn import_cfg(account: AccountSelector) -> ImportConfig {
    ImportConfig {
        connect: ConnectConfig {
            url: base_url().to_owned(),
            auth: basic("test1"),
            account,
        },
        objects: None,
        allow_source_change: false,
    }
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

#[test]
#[ignore = "requires Docker"]
fn import_test1_full_matches_seed_stats() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test1").expect("test1");
    let archive = tmp_archive("imp1");

    assert_eq!(fx.domain, seeder::DOMAIN, "fixture domain");
    assert!(!fx.domain_id.is_empty(), "fixture resolved a domain id");
    assert_eq!(
        fx.admin_login,
        (
            seeder::ADMIN_USER.to_owned(),
            seeder::ADMIN_PASSWORD.to_owned()
        ),
        "fixture exposes the instance admin login"
    );

    let cfg = ImportConfig {
        connect: ConnectConfig {
            url: fx.base_url.clone(),
            auth: basic("test1"),
            account: AccountSelector::Id(acc.account_id.clone()),
        },
        objects: None,
        allow_source_change: false,
    };
    let summary = sync::import_jmap::run(common(&archive, false), cfg).expect("import run");
    assert!(!summary.any_failed(), "import had failures: {summary:?}");

    let seeded = acc.seeded.as_ref().expect("seed stats");
    let conn = Connection::open(&archive).unwrap();

    let mailboxes = count(&conn, "mailboxes") as usize;
    assert!(
        mailboxes >= seeded.mailboxes_created + 4,
        "mailboxes ({mailboxes}) = seeded leaf tree ({}) + Stalwart default roles",
        seeded.mailboxes_created
    );
    assert_eq!(count(&conn, "emails") as usize, seeded.emails);
    assert!(count(&conn, "file_nodes") as usize >= seeded.file_nodes);
    assert!(count(&conn, "contact_cards") as usize >= seeded.contacts);
    assert!(count(&conn, "calendar_events") as usize >= seeded.events);
    assert!(count(&conn, "blobs") > 0);

    assert_eq!(
        count(&conn, "address_books") as usize,
        seeded.address_books,
        "address books mirror the source exactly"
    );
    assert_eq!(
        count(&conn, "calendars") as usize,
        seeded.calendars,
        "calendars mirror the source exactly"
    );
    assert_eq!(
        count(&conn, "identities") as usize,
        usize::from(seeded.identity),
        "identity count matches the seeded flag"
    );
    match seeded.sieve_active {
        Some(active) => {
            assert_eq!(
                count(&conn, "sieve_scripts"),
                2,
                "seeder lays down two scripts per active layout"
            );
            let active_count: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sieve_scripts WHERE is_active = 1",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                active_count,
                i64::from(active),
                "sieve activation state matches SeedStats"
            );
        }
        None => assert_eq!(count(&conn, "sieve_scripts"), 0, "no sieve seeded"),
    }

    let _ = std::fs::remove_file(&archive);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn import_resolves_account_via_admin_principal_path() {
    let fx = seeder::provision(base_url()).expect("provision");
    let admin_fx = fx.account(seeder::ADMIN_LOCALPART).expect("vandeladmin");
    assert!(
        admin_fx.admin_role,
        "vandeladmin must have the Admin role to list principals"
    );
    let target = fx.account("test1").expect("test1");
    let archive = tmp_archive("impadmin");

    let cfg = ImportConfig {
        connect: ConnectConfig {
            url: fx.base_url.clone(),
            auth: Auth::Basic {
                user: admin_fx.email.clone(),
                password: seeder::ADMIN_ACCOUNT_PASSWORD.to_owned(),
            },
            account: AccountSelector::Name(target.email.clone()),
        },
        objects: Some(vec![vandelay::types::ObjectType::Mailbox]),
        allow_source_change: false,
    };
    let summary = sync::import_jmap::run(common(&archive, false), cfg)
        .expect("admin-principal-resolved import");
    assert!(!summary.any_failed(), "import had failures: {summary:?}");

    let conn = Connection::open(&archive).unwrap();
    assert!(
        count(&conn, "mailboxes") > 0,
        "mailboxes imported via the admin-resolved account id"
    );
    let resolved: String = conn
        .query_row("SELECT account_id FROM sources", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        resolved, target.account_id,
        "admin Principal path resolved to test1's data account id"
    );

    let _ = std::fs::remove_file(&archive);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn import_dry_run_writes_nothing() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test3").expect("test3");
    let archive = tmp_archive("impdry");

    let mut cfg = import_cfg(AccountSelector::Id(acc.account_id.clone()));
    cfg.connect.auth = basic("test3");
    let summary = sync::import_jmap::run(common(&archive, true), cfg).expect("dry-run import");
    assert!(summary.per_type.is_empty(), "dry-run returns empty summary");

    let conn = Connection::open(&archive).unwrap();
    assert_eq!(count(&conn, "emails"), 0);
    assert_eq!(
        count(&conn, "sources"),
        0,
        "dry-run must not upsert sources"
    );

    let _ = std::fs::remove_file(&archive);
    seeder::teardown(base_url()).expect("teardown");
}

fn export_cfg(localpart: &str, account_id: &str, prune: bool) -> ExportConfig {
    ExportConfig {
        connect: ConnectConfig {
            url: base_url().to_owned(),
            auth: Auth::Basic {
                user: format!("{localpart}@{}", seeder::DOMAIN),
                password: seeder::USER_PASSWORD.to_owned(),
            },
            account: AccountSelector::Id(account_id.to_owned()),
        },
        objects: None,
        prune,
        yes: true,
    }
}

fn jmap_for(localpart: &str) -> seeder::jmap::Jmap {
    seeder::jmap::Jmap::connect(
        base_url(),
        &format!("{localpart}@{}", seeder::DOMAIN),
        seeder::USER_PASSWORD,
    )
    .expect("user session")
}

fn seed_deep_mailbox_chain(jmap: &seeder::jmap::Jmap, account_id: &str, depth: usize) {
    const CORE: &str = "urn:ietf:params:jmap:core";
    const MAIL: &str = "urn:ietf:params:jmap:mail";
    let mut create = serde_json::Map::new();
    for i in 0..depth {
        let key = format!("d{i}");
        let mut obj = serde_json::Map::new();
        obj.insert(
            "name".to_owned(),
            serde_json::Value::String(format!("deep-{i}")),
        );
        if i > 0 {
            obj.insert(
                "parentId".to_owned(),
                serde_json::Value::String(format!("#d{}", i - 1)),
            );
        }
        create.insert(key, serde_json::Value::Object(obj));
    }
    jmap.set_create(
        &[CORE, MAIL],
        "Mailbox/set",
        account_id,
        serde_json::Value::Object(create),
        &[],
    )
    .expect("seed deep mailbox chain");
}

fn seed_deep_filenode_chain(jmap: &seeder::jmap::Jmap, account_id: &str, depth: usize) {
    const CORE: &str = "urn:ietf:params:jmap:core";
    const FILENODE: &str = "urn:ietf:params:jmap:filenode";
    let mut create = serde_json::Map::new();
    for i in 0..depth {
        let key = format!("fn{i}");
        let mut obj = serde_json::Map::new();
        obj.insert(
            "name".to_owned(),
            serde_json::Value::String(format!("dir-{i}")),
        );
        obj.insert(
            "nodeType".to_owned(),
            serde_json::Value::String("directory".to_owned()),
        );
        if i > 0 {
            obj.insert(
                "parentId".to_owned(),
                serde_json::Value::String(format!("#fn{}", i - 1)),
            );
        }
        create.insert(key, serde_json::Value::Object(obj));
    }
    jmap.set_create(
        &[CORE, FILENODE],
        "FileNode/set",
        account_id,
        serde_json::Value::Object(create),
        &[],
    )
    .expect("seed deep filenode chain");
}

fn mailbox_depth_in_archive(conn: &Connection, leaf_name: &str) -> Option<usize> {
    let mut id: Option<i64> = conn
        .query_row(
            "SELECT id FROM mailboxes WHERE name = ?1",
            rusqlite::params![leaf_name],
            |r| r.get(0),
        )
        .ok();
    let mut depth = 0usize;
    while let Some(cur) = id {
        let parent: Option<i64> = conn
            .query_row(
                "SELECT parent_id FROM mailboxes WHERE id = ?1",
                rusqlite::params![cur],
                |r| r.get(0),
            )
            .ok()?;
        match parent {
            Some(p) => {
                depth += 1;
                id = Some(p);
            }
            None => break,
        }
    }
    Some(depth)
}

fn filenode_depth_in_archive(conn: &Connection, leaf_name: &str) -> Option<usize> {
    let mut id: Option<i64> = conn
        .query_row(
            "SELECT id FROM file_nodes WHERE name = ?1",
            rusqlite::params![leaf_name],
            |r| r.get(0),
        )
        .ok();
    let mut depth = 0usize;
    while let Some(cur) = id {
        let parent: Option<i64> = conn
            .query_row(
                "SELECT parent_id FROM file_nodes WHERE id = ?1",
                rusqlite::params![cur],
                |r| r.get(0),
            )
            .ok()?;
        match parent {
            Some(p) => {
                depth += 1;
                id = Some(p);
            }
            None => break,
        }
    }
    Some(depth)
}

#[test]
#[ignore = "requires Docker"]
fn import_export_deeply_nested_mailbox_tree() {
    const DEPTH: usize = 10;
    let fx = seeder::provision(base_url()).expect("provision");
    let src = fx.account("test4").expect("test4 (deep source)");
    let tgt = fx.account("test5").expect("test5 (deep target)");

    let src_jmap = jmap_for(&src.localpart);
    seed_deep_mailbox_chain(&src_jmap, &src.account_id, DEPTH);

    let archive = tmp_archive("deep_mbox");
    let imp = ImportConfig {
        connect: ConnectConfig {
            url: base_url().to_owned(),
            auth: basic("test4"),
            account: AccountSelector::Id(src.account_id.clone()),
        },
        objects: Some(vec![vandelay::types::ObjectType::Mailbox]),
        allow_source_change: false,
    };
    let s = sync::import_jmap::run(common(&archive, false), imp).expect("import");
    assert!(!s.any_failed(), "import had failures: {s:?}");

    {
        let conn = Connection::open(&archive).unwrap();
        let d = mailbox_depth_in_archive(&conn, &format!("deep-{}", DEPTH - 1));
        assert_eq!(
            d,
            Some(DEPTH - 1),
            "imported leaf must be {} hops from a root",
            DEPTH - 1
        );
        let dn: i64 = conn
            .query_row(
                "SELECT count(*) FROM mailboxes WHERE name LIKE 'deep-%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(dn as usize, DEPTH, "all deep levels imported");
    }

    let exp_s = sync::export::run(
        common(&archive, false),
        export_cfg("test5", &tgt.account_id, false),
    )
    .expect("export deep tree");
    assert!(
        !exp_s.per_type.iter().any(|(_, c)| c.failed > 0),
        "export had failures: {exp_s:?}"
    );

    let again_archive = tmp_archive("deep_mbox_round");
    let imp_again = ImportConfig {
        connect: ConnectConfig {
            url: base_url().to_owned(),
            auth: basic("test5"),
            account: AccountSelector::Id(tgt.account_id.clone()),
        },
        objects: Some(vec![vandelay::types::ObjectType::Mailbox]),
        allow_source_change: false,
    };
    sync::import_jmap::run(common(&again_archive, false), imp_again)
        .expect("re-import from target");
    {
        let conn = Connection::open(&again_archive).unwrap();
        let d = mailbox_depth_in_archive(&conn, &format!("deep-{}", DEPTH - 1));
        assert_eq!(
            d,
            Some(DEPTH - 1),
            "round-trip preserves the {DEPTH}-level chain"
        );
    }

    let _ = std::fs::remove_file(&archive);
    let _ = std::fs::remove_file(&again_archive);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn import_export_deeply_nested_filenode_tree() {
    const DEPTH: usize = 10;
    let fx = seeder::provision(base_url()).expect("provision");
    let src = fx.account("test4").expect("test4 (deep source)");
    let tgt = fx.account("test5").expect("test5 (deep target)");

    let src_jmap = jmap_for(&src.localpart);
    seed_deep_filenode_chain(&src_jmap, &src.account_id, DEPTH);

    let archive = tmp_archive("deep_fn");
    let imp = ImportConfig {
        connect: ConnectConfig {
            url: base_url().to_owned(),
            auth: basic("test4"),
            account: AccountSelector::Id(src.account_id.clone()),
        },
        objects: Some(vec![vandelay::types::ObjectType::FileNode]),
        allow_source_change: false,
    };
    let s = sync::import_jmap::run(common(&archive, false), imp).expect("import");
    assert!(!s.any_failed(), "import had failures: {s:?}");

    {
        let conn = Connection::open(&archive).unwrap();
        let d = filenode_depth_in_archive(&conn, &format!("dir-{}", DEPTH - 1));
        assert_eq!(
            d,
            Some(DEPTH - 1),
            "imported leaf must be {} hops from a root",
            DEPTH - 1
        );
        let dn: i64 = conn
            .query_row(
                "SELECT count(*) FROM file_nodes WHERE name LIKE 'dir-%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(dn as usize, DEPTH, "all deep levels imported");
    }

    let exp_s = sync::export::run(
        common(&archive, false),
        export_cfg("test5", &tgt.account_id, false),
    )
    .expect("export deep filenode tree");
    assert!(
        !exp_s.per_type.iter().any(|(_, c)| c.failed > 0),
        "export had failures: {exp_s:?}"
    );

    let again_archive = tmp_archive("deep_fn_round");
    sync::import_jmap::run(
        common(&again_archive, false),
        ImportConfig {
            connect: ConnectConfig {
                url: base_url().to_owned(),
                auth: basic("test5"),
                account: AccountSelector::Id(tgt.account_id.clone()),
            },
            objects: Some(vec![vandelay::types::ObjectType::FileNode]),
            allow_source_change: false,
        },
    )
    .expect("re-import from target");
    {
        let conn = Connection::open(&again_archive).unwrap();
        let d = filenode_depth_in_archive(&conn, &format!("dir-{}", DEPTH - 1));
        assert_eq!(
            d,
            Some(DEPTH - 1),
            "round-trip preserves the {DEPTH}-level filenode chain"
        );
    }

    let _ = std::fs::remove_file(&archive);
    let _ = std::fs::remove_file(&again_archive);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn prune_multilevel_mailbox_tree_destroys_leaf_first() {
    const DEPTH: usize = 6;
    let fx = seeder::provision(base_url()).expect("provision");
    let src = fx.account("test4").expect("test4");
    let tgt = fx.account("test5").expect("test5");

    let src_jmap = jmap_for(&src.localpart);
    seed_deep_mailbox_chain(&src_jmap, &src.account_id, DEPTH);

    let arch_src = tmp_archive("prune_deep_src");
    sync::import_jmap::run(
        common(&arch_src, false),
        ImportConfig {
            connect: ConnectConfig {
                url: base_url().to_owned(),
                auth: basic("test4"),
                account: AccountSelector::Id(src.account_id.clone()),
            },
            objects: Some(vec![vandelay::types::ObjectType::Mailbox]),
            allow_source_change: false,
        },
    )
    .expect("import full deep tree");
    sync::export::run(
        common(&arch_src, false),
        export_cfg("test5", &tgt.account_id, false),
    )
    .expect("seed target with deep tree");

    let arch_empty = tmp_archive("prune_deep_empty");
    {
        let _ = vandelay::db::init::open(&arch_empty).expect("init empty archive");
    }

    let small_jmap = jmap_for(&fx.account("test6").expect("test6").localpart);
    let mut create = serde_json::Map::new();
    let mut obj = serde_json::Map::new();
    obj.insert(
        "name".to_owned(),
        serde_json::Value::String("Smallbox".to_owned()),
    );
    create.insert("m".to_owned(), serde_json::Value::Object(obj));
    small_jmap
        .set_create(
            &["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            "Mailbox/set",
            &fx.account("test6").unwrap().account_id,
            serde_json::Value::Object(create),
            &[],
        )
        .expect("seed tiny mailbox in test6");
    sync::import_jmap::run(
        common(&arch_empty, false),
        ImportConfig {
            connect: ConnectConfig {
                url: base_url().to_owned(),
                auth: basic("test6"),
                account: AccountSelector::Id(fx.account("test6").unwrap().account_id.clone()),
            },
            objects: Some(vec![vandelay::types::ObjectType::Mailbox]),
            allow_source_change: false,
        },
    )
    .expect("populate archive with one trivial mailbox");

    let pruned = sync::export::run(
        common(&arch_empty, false),
        export_cfg("test5", &tgt.account_id, true),
    )
    .expect("prune deep tree on target");

    let mb_counts = pruned
        .per_type
        .iter()
        .find(|(t, _)| *t == "Mailbox")
        .map(|(_, c)| c.clone())
        .expect("mailbox counts");
    assert!(
        mb_counts.deleted as usize >= DEPTH,
        "prune destroyed >= {DEPTH} deep mailboxes (leaf-first); got {mb_counts:?}",
    );
    assert_eq!(
        mb_counts.failed, 0,
        "no prune failures (leaf-first ordering must avoid FK errors): {mb_counts:?}"
    );

    let _ = std::fs::remove_file(&arch_src);
    let _ = std::fs::remove_file(&arch_empty);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn prune_non_tree_type_contact_card() {
    let fx = seeder::provision(base_url()).expect("provision");
    let src = fx.account("test1").expect("test1");
    let tgt = fx.account("test5").expect("test5");

    let arch_full = tmp_archive("prune_cc_full");
    sync::import_jmap::run(
        common(&arch_full, false),
        ImportConfig {
            connect: ConnectConfig {
                url: base_url().to_owned(),
                auth: basic("test1"),
                account: AccountSelector::Id(src.account_id.clone()),
            },
            objects: Some(vec![
                vandelay::types::ObjectType::AddressBook,
                vandelay::types::ObjectType::ContactCard,
            ]),
            allow_source_change: false,
        },
    )
    .expect("import contact cards");
    sync::export::run(
        common(&arch_full, false),
        export_cfg("test5", &tgt.account_id, false),
    )
    .expect("seed target with contact cards");

    let arch_empty = tmp_archive("prune_cc_empty");
    {
        let conn = vandelay::db::init::open(&arch_empty).expect("init empty archive");
        conn.execute(
            "INSERT INTO address_books (id,name,is_default) VALUES (1,'AB',1)",
            [],
        )
        .unwrap();
    }

    let pruned = sync::export::run(
        common(&arch_empty, false),
        export_cfg("test5", &tgt.account_id, true),
    )
    .expect("prune contact cards");
    let cc = pruned
        .per_type
        .iter()
        .find(|(t, _)| *t == "ContactCard")
        .map(|(_, c)| c.clone());
    if let Some(cc) = cc {
        assert!(
            cc.deleted > 0,
            "prune destroyed unmatched ContactCards: {cc:?}"
        );
        assert_eq!(cc.failed, 0, "no per-unit failures: {cc:?}");
    }

    let _ = std::fs::remove_file(&arch_full);
    let _ = std::fs::remove_file(&arch_empty);
    seeder::teardown(base_url()).expect("teardown");
}

const ISSUE30_CARD_UID: &str = "vandelay-issue30-card";
const ISSUE30_EVENT_UID: &str = "vandelay-issue30-event";

fn data_uri_bytes(resource: &Value, uri_key: &str, expect_media_type: &str) -> Vec<u8> {
    use base64::Engine;
    let uri = resource
        .get(uri_key)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("no {uri_key} on {resource}"));
    assert!(
        resource.get("blobId").is_none(),
        "target must not hold a blobId for {uri_key}: {resource}"
    );
    let prefix = format!("data:{expect_media_type};base64,");
    let payload = uri
        .strip_prefix(&prefix)
        .unwrap_or_else(|| panic!("{uri_key} is not a {prefix}... data URI: {resource}"));
    base64::engine::general_purpose::STANDARD
        .decode(payload)
        .expect("base64 payload")
}

#[test]
#[ignore = "requires Docker"]
fn export_inlines_contact_and_event_blobs_instead_of_blob_ids() {
    let fx = seeder::provision(base_url()).expect("provision");
    let tgt = fx.account("test6").expect("test6");
    let archive = tmp_archive("issue30");

    let photo: &[u8] = b"\x89PNG\r\n\x1a\nvandelay issue 30 contact photo bytes";
    let agenda: &[u8] = b"vandelay issue 30 calendar enclosure bytes";
    {
        let conn = vandelay::db::init::open(&archive).expect("init archive");
        conn.execute(
            "INSERT INTO address_books (id,name,is_default) VALUES (1,'Issue30 Book',1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO calendars (id,name,is_default) VALUES (1,'Issue30 Calendar',1)",
            [],
        )
        .unwrap();
        let photo_blob = vandelay::db::blobs::intern_blob(&conn, photo).unwrap();
        let agenda_blob = vandelay::db::blobs::intern_blob(&conn, agenda).unwrap();
        let card = json!({
            "@type": "Card",
            "version": "1.0",
            "name": { "full": "Issue Thirty" },
            "media": { "photo": {
                "@type": "Media", "kind": "photo",
                "@blob": photo_blob, "mediaType": "image/png"
            } }
        })
        .to_string();
        conn.execute(
            "INSERT INTO contact_cards (id,uid,address_book_ids,data) VALUES (1,?1,'[1]',?2)",
            [ISSUE30_CARD_UID, card.as_str()],
        )
        .unwrap();
        let event = json!({
            "@type": "Event",
            "uid": ISSUE30_EVENT_UID,
            "title": "Issue 30 review",
            "start": "2026-03-01T09:00:00",
            "duration": "PT1H",
            "timeZone": "Etc/UTC",
            "links": { "1": {
                "@type": "Link", "rel": "enclosure",
                "@blob": agenda_blob, "contentType": "text/plain", "title": "agenda.txt"
            } }
        })
        .to_string();
        conn.execute(
            "INSERT INTO calendar_events (id,calendar_ids,is_draft,use_default_alerts,data)
             VALUES (1,'[1]',0,0,?1)",
            [event.as_str()],
        )
        .unwrap();
    }

    let summary = sync::export::run(
        common(&archive, false),
        export_cfg("test6", &tgt.account_id, false),
    )
    .expect("export");
    for name in ["ContactCard", "CalendarEvent"] {
        let counts = summary
            .per_type
            .iter()
            .find(|(t, _)| *t == name)
            .map(|(_, c)| c.clone())
            .unwrap_or_else(|| panic!("{name} counts missing: {summary:?}"));
        assert_eq!(counts.created, 1, "{name} created exactly one: {counts:?}");
        assert_eq!(counts.failed, 0, "{name} had no failures: {counts:?}");
    }

    let client = HttpClient::new(basic("test6"), RetryPolicy::new(5), true);
    let session = Session::discover(&client, base_url()).expect("discover target session");
    let api = session.api_url.clone();

    let cards = client
        .post_json(
            &api,
            &json!({
                "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:contacts"],
                "methodCalls": [["ContactCard/get",
                    { "accountId": tgt.account_id, "ids": Value::Null }, "g"]]
            }),
        )
        .expect("ContactCard/get");
    let card = cards
        .pointer("/methodResponses/0/1/list")
        .and_then(Value::as_array)
        .expect("ContactCard list")
        .iter()
        .find(|c| c.get("uid").and_then(Value::as_str) == Some(ISSUE30_CARD_UID))
        .unwrap_or_else(|| panic!("exported card not found on the target: {cards}"))
        .clone();
    let photo_resource = card
        .pointer("/media/photo")
        .unwrap_or_else(|| panic!("card has no media.photo: {card}"));
    assert_eq!(
        data_uri_bytes(photo_resource, "uri", "image/png"),
        photo,
        "contact photo bytes survived the export: {card}"
    );

    let events = client
        .post_json(
            &api,
            &json!({
                "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:calendars"],
                "methodCalls": [["CalendarEvent/get",
                    { "accountId": tgt.account_id, "ids": Value::Null }, "g"]]
            }),
        )
        .expect("CalendarEvent/get");
    let event = events
        .pointer("/methodResponses/0/1/list")
        .and_then(Value::as_array)
        .expect("CalendarEvent list")
        .iter()
        .find(|e| e.get("uid").and_then(Value::as_str) == Some(ISSUE30_EVENT_UID))
        .unwrap_or_else(|| panic!("exported event not found on the target: {events}"))
        .clone();
    let link = event
        .get("links")
        .and_then(Value::as_object)
        .and_then(|m| m.values().next())
        .unwrap_or_else(|| panic!("event has no links: {event}"));
    assert_eq!(
        data_uri_bytes(link, "href", "text/plain"),
        agenda,
        "calendar enclosure bytes survived the export: {event}"
    );

    let _ = std::fs::remove_file(&archive);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn export_round_trip_and_convergence() {
    let fx = seeder::provision(base_url()).expect("provision");
    let src = fx.account("test1").expect("test1");
    let tgt = fx.account("test4").expect("test4");
    let archive = tmp_archive("rt");

    let imp = ImportConfig {
        connect: ConnectConfig {
            url: base_url().to_owned(),
            auth: basic("test1"),
            account: AccountSelector::Id(src.account_id.clone()),
        },
        objects: None,
        allow_source_change: false,
    };
    let s = sync::import_jmap::run(common(&archive, false), imp).expect("import");
    assert!(!s.any_failed());

    let mut tight = serde_json::Map::new();
    tight.insert("uploadTtl".to_owned(), serde_json::json!(3_000));
    tight.insert("uploadQuota".to_owned(), serde_json::json!(150_000));
    let _quota_guard = JmapSettingsGuard::override_settings(tight);

    let started_e1 = std::time::Instant::now();
    let e1 = sync::export::run(
        common(&archive, false),
        export_cfg("test4", &tgt.account_id, false),
    )
    .expect("export 1");
    let e1_elapsed = started_e1.elapsed();
    let created_1: u64 = e1.per_type.iter().map(|(_, c)| c.created).sum();
    assert!(created_1 > 0, "first export created objects: {e1:?}");
    let emails = e1
        .per_type
        .iter()
        .find(|(t, _)| *t == "Email")
        .map(|(_, c)| c.created)
        .unwrap_or(0);
    assert!(emails > 0, "emails were imported to target");
    assert!(
        !e1.per_type.iter().any(|(_, c)| c.failed > 0),
        "export 1 had failures: {e1:?}"
    );

    let e2 = sync::export::run(
        common(&archive, false),
        export_cfg("test4", &tgt.account_id, false),
    )
    .expect("export 2");
    let created_2: u64 = e2.per_type.iter().map(|(_, c)| c.created).sum();
    assert_eq!(
        created_2, 0,
        "second export must be convergent (no new creates): {e2:?}"
    );
    let skipped_2: u64 = e2.per_type.iter().map(|(_, c)| c.skipped).sum();
    assert!(skipped_2 > 0, "second export matched existing objects");

    eprintln!(
        "round-trip: emails created={emails} export-1 elapsed={:.1}s \
         retries={} retry_after_sleeps={}",
        e1_elapsed.as_secs_f64(),
        e1.retries_observed,
        e1.retry_after_sleeps,
    );
    assert!(
        e1.retry_after_sleeps > 0,
        "tight blob-upload quota should have provoked at least one Retry-After sleep \
         during export-1; counters retries={} retry_after_sleeps={}",
        e1.retries_observed,
        e1.retry_after_sleeps,
    );

    drop(_quota_guard);
    let _ = std::fs::remove_file(&archive);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn export_prune_destroys_unmatched() {
    let fx = seeder::provision(base_url()).expect("provision");
    let big = fx.account("test1").expect("test1");
    let small = fx.account("test3").expect("test3");
    let tgt = fx.account("test5").expect("test5");

    let arch_big = tmp_archive("prbig");
    sync::import_jmap::run(
        common(&arch_big, false),
        ImportConfig {
            connect: ConnectConfig {
                url: base_url().to_owned(),
                auth: basic("test1"),
                account: AccountSelector::Id(big.account_id.clone()),
            },
            objects: Some(vec![vandelay::types::ObjectType::Mailbox]),
            allow_source_change: false,
        },
    )
    .expect("import big");
    sync::export::run(
        common(&arch_big, false),
        export_cfg("test5", &tgt.account_id, false),
    )
    .expect("seed target with big mailbox tree");

    let arch_small = tmp_archive("prsmall");
    sync::import_jmap::run(
        common(&arch_small, false),
        ImportConfig {
            connect: ConnectConfig {
                url: base_url().to_owned(),
                auth: basic("test3"),
                account: AccountSelector::Id(small.account_id.clone()),
            },
            objects: Some(vec![vandelay::types::ObjectType::Mailbox]),
            allow_source_change: false,
        },
    )
    .expect("import small");

    let pruned = sync::export::run(
        common(&arch_small, false),
        export_cfg("test5", &tgt.account_id, true),
    )
    .expect("export with prune");
    let deleted: u64 = pruned.per_type.iter().map(|(_, c)| c.deleted).sum();
    assert!(
        deleted > 0,
        "prune destroyed target mailboxes absent from the small archive: {pruned:?}"
    );

    let _ = std::fs::remove_file(&arch_big);
    let _ = std::fs::remove_file(&arch_small);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn import_resume_converges_after_partial() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test2").expect("test2");
    let archive = tmp_archive("impresume");

    let mk = || ImportConfig {
        connect: ConnectConfig {
            url: base_url().to_owned(),
            auth: basic("test2"),
            account: AccountSelector::Id(acc.account_id.clone()),
        },
        objects: None,
        allow_source_change: false,
    };

    let mut first = mk();
    first.objects = Some(vec![vandelay::types::ObjectType::Mailbox]);
    sync::import_jmap::run(common(&archive, false), first).expect("partial import");

    let conn = Connection::open(&archive).unwrap();
    assert_eq!(count(&conn, "emails"), 0, "only mailboxes imported so far");
    drop(conn);

    let summary = sync::import_jmap::run(common(&archive, false), mk()).expect("resume");
    assert!(!summary.any_failed());
    let seeded = acc.seeded.as_ref().unwrap();
    {
        let conn = Connection::open(&archive).unwrap();
        assert_eq!(count(&conn, "emails") as usize, seeded.emails);
    }

    let again = sync::import_jmap::run(common(&archive, false), mk()).expect("idempotent rerun");
    assert!(!again.any_failed());
    let fetched_on_rerun: u64 = again.per_type.iter().map(|(_, c)| c.fetched).sum();
    assert_eq!(fetched_on_rerun, 0, "rerun fetched nothing new");
    {
        let conn2 = Connection::open(&archive).unwrap();
        assert_eq!(count(&conn2, "emails") as usize, seeded.emails);
    }

    let _ = std::fs::remove_file(&archive);
    seeder::teardown(base_url()).expect("teardown");
}

// TODO: re-enable once we have a way to make Stalwart enforce
// maxConcurrentRequests strictly under testcontainers.
#[cfg(any())]
#[test]
#[ignore = "requires Docker"]
fn live_burst_exceeds_concurrent_requests_and_recovers() {
    use std::sync::Arc;
    use vandelay::jmap::http::{HttpClient, RetryPolicy};
    use vandelay::jmap::request::{Request, check_method_error};
    use vandelay::jmap::session::{Limits, Session};

    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test1").expect("test1");

    let client = HttpClient::new(basic("test1"), RetryPolicy::new(20), true);
    let session = Session::discover(&client, base_url()).expect("discover session");
    let server_limits = session.core_limits().expect("core limits");

    let bypass_gate = Limits {
        max_concurrent_requests: 1024,
        max_concurrent_upload: 1024,
        ..server_limits
    };
    client.set_limits(&bypass_gate);

    let n_threads = (server_limits.max_concurrent_requests as usize) * 4;
    let calls_per_thread = 4u32;

    let client = Arc::new(client);
    let api = Arc::new(session.api_url.clone());
    let account_id = Arc::new(acc.account_id.clone());

    let mut handles = Vec::with_capacity(n_threads);
    for _ in 0..n_threads {
        let client = client.clone();
        let api = api.clone();
        let account_id = account_id.clone();
        handles.push(std::thread::spawn(move || {
            let mut failures: Vec<String> = Vec::new();
            for _ in 0..calls_per_thread {
                let mut req = Request::new();
                req.call(
                    "Mailbox/query",
                    serde_json::json!({
                        "accountId": &*account_id,
                        "filter": { "name": "__sleep" }
                    }),
                    "q",
                );
                match req.send(&client, &api) {
                    Ok(resp) => match resp.first().and_then(|mr| {
                        check_method_error(mr)?;
                        Ok(())
                    }) {
                        Ok(()) => {}
                        Err(e) => {
                            failures.push(format!("method: {e}"));
                            break;
                        }
                    },
                    Err(e) => {
                        failures.push(format!("send: {e}"));
                        break;
                    }
                }
            }
            failures
        }));
    }

    let mut all_failures = Vec::new();
    for h in handles {
        all_failures.extend(h.join().expect("worker panicked"));
    }
    let retries = client.retries_observed();
    let retry_after = client.retry_after_sleeps();
    eprintln!(
        "burst against server-cap maxConcurrentRequests={} with {} threads x {} calls: \
         retries={retries} retry_after_sleeps={retry_after} failures={}",
        server_limits.max_concurrent_requests,
        n_threads,
        calls_per_thread,
        all_failures.len()
    );
    assert!(
        all_failures.is_empty(),
        "all calls should eventually succeed via retry/backoff; first failure: {:?}",
        all_failures.first()
    );
    assert!(
        retries > 0,
        "burst should have provoked at least one 400 limit:maxConcurrentRequests retry; \
         counters retries={retries} retry_after_sleeps={retry_after}"
    );

    seeder::teardown(base_url()).expect("teardown");
}

struct JmapSettingsGuard {
    admin: vandelay::jmap::http::HttpClient,
    admin_api: String,
    admin_account: String,
    previous: serde_json::Map<String, serde_json::Value>,
}

impl JmapSettingsGuard {
    fn override_settings(updates: serde_json::Map<String, serde_json::Value>) -> Self {
        use vandelay::jmap::http::{Auth, HttpClient, RetryPolicy};
        use vandelay::jmap::session::Session;
        let admin = HttpClient::new(
            Auth::Basic {
                user: seeder::ADMIN_USER.to_owned(),
                password: seeder::ADMIN_PASSWORD.to_owned(),
            },
            RetryPolicy::new(5),
            true,
        );
        let session = Session::discover(&admin, base_url()).expect("admin discover");
        let admin_account = session
            .accounts
            .first()
            .map(|(id, _)| id.clone())
            .expect("admin session has an account id");
        let admin_api = session.api_url.clone();
        let previous = read_jmap_settings(
            &admin,
            &admin_api,
            &admin_account,
            updates
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .as_slice(),
        );
        apply_jmap_settings(&admin, &admin_api, &admin_account, &updates);
        JmapSettingsGuard {
            admin,
            admin_api,
            admin_account,
            previous,
        }
    }
}

impl Drop for JmapSettingsGuard {
    fn drop(&mut self) {
        apply_jmap_settings(
            &self.admin,
            &self.admin_api,
            &self.admin_account,
            &self.previous,
        );
    }
}

fn read_jmap_settings(
    admin: &vandelay::jmap::http::HttpClient,
    api: &str,
    account: &str,
    properties: &[&str],
) -> serde_json::Map<String, serde_json::Value> {
    let body = serde_json::json!({
        "using": ["urn:ietf:params:jmap:core", "urn:stalwart:jmap"],
        "methodCalls": [["x:Jmap/get", {
            "accountId": account,
            "ids": ["singleton"],
            "properties": properties,
        }, "g"]]
    });
    let resp = admin.post_json(api, &body).expect("x:Jmap/get");
    let obj = resp
        .pointer("/methodResponses/0/1/list/0")
        .and_then(serde_json::Value::as_object)
        .expect("x:Jmap/get returned a singleton")
        .clone();
    let mut out = serde_json::Map::new();
    for p in properties {
        if let Some(v) = obj.get(*p) {
            out.insert((*p).to_owned(), v.clone());
        }
    }
    out
}

fn apply_jmap_settings(
    admin: &vandelay::jmap::http::HttpClient,
    api: &str,
    account: &str,
    updates: &serde_json::Map<String, serde_json::Value>,
) {
    let body = serde_json::json!({
        "using": ["urn:ietf:params:jmap:core", "urn:stalwart:jmap"],
        "methodCalls": [
            ["x:Jmap/set", {
                "accountId": account,
                "update": { "singleton": serde_json::Value::Object(updates.clone()) }
            }, "u"],
            ["x:Action/set", {
                "accountId": account,
                "create": { "r": { "@type": "ReloadSettings" } }
            }, "r"]
        ]
    });
    let resp = admin
        .post_json(api, &body)
        .expect("x:Jmap/set + ReloadSettings");
    assert!(
        resp.pointer("/methodResponses/0/1/updated/singleton")
            .is_some(),
        "x:Jmap/set not applied: {resp}"
    );
}

#[test]
#[ignore = "requires Docker"]
fn live_blob_quota_429_triggers_retry_after_then_succeeds() {
    use vandelay::jmap::blobxfer;
    use vandelay::jmap::http::{HttpClient, RetryPolicy};
    use vandelay::jmap::session::Session;

    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test1").expect("test1");

    let mut updates = serde_json::Map::new();
    updates.insert("uploadTtl".to_owned(), serde_json::json!(5_000));
    let _ttl_guard = JmapSettingsGuard::override_settings(updates);

    let client = HttpClient::new(basic("test1"), RetryPolicy::new(20), true);
    let session = Session::discover(&client, base_url()).expect("discover session");
    let limits = session.core_limits().expect("core limits");
    client.set_limits(&limits);

    let blob_size = 8 * 1024 * 1024;
    let mut blob = vec![0u8; blob_size];
    let max_uploads = 8u32;
    let mut accepted = 0u32;
    let mut first_failure: Option<String> = None;
    for i in 0..max_uploads {
        for (j, b) in blob.iter_mut().enumerate().take(64) {
            *b = ((i as usize).wrapping_mul(2654435761).wrapping_add(j)) as u8;
        }
        match blobxfer::upload_bytes(
            &client,
            &session,
            &acc.account_id,
            "application/octet-stream",
            &blob,
        ) {
            Ok(_) => accepted += 1,
            Err(e) => {
                first_failure = Some(format!("upload {i}: {e}"));
                break;
            }
        }
    }

    let retries = client.retries_observed();
    let retry_after = client.retry_after_sleeps();
    eprintln!(
        "blob quota burst: accepted={accepted}/{max_uploads} ({} MiB each) \
         retries={retries} retry_after_sleeps={retry_after} first_failure={:?}",
        blob_size / (1024 * 1024),
        first_failure,
    );
    assert!(
        first_failure.is_none(),
        "all uploads should eventually succeed via Retry-After backoff: {:?}",
        first_failure
    );
    assert!(
        retry_after > 0,
        "expected at least one Retry-After sleep from the blob-upload quota class; \
         counters retries={retries} retry_after_sleeps={retry_after}"
    );

    drop(_ttl_guard);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn import_delta_propagates_email_keyword_change_via_changes() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test1").expect("test1");
    let archive = tmp_archive("delta-email");

    sync::import_jmap::run(
        common(&archive, false),
        import_cfg(AccountSelector::Id(acc.account_id.clone())),
    )
    .expect("first import");

    let (jmap_id, local_id): (String, i64) = {
        let conn = Connection::open(&archive).unwrap();
        conn.query_row(
            "SELECT s.jmap_id, s.local_id FROM sync_id_jmap s
             JOIN emails e ON e.id = s.local_id
             WHERE s.type_name = 'Email' AND e.keywords NOT LIKE '%$flagged%'
             LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("an unflagged email exists in the archive")
    };

    let client = HttpClient::new(basic("test1"), RetryPolicy::new(5), true);
    let session = Session::discover(&client, base_url()).expect("session discovered");
    let account = account::resolve(
        &AccountSelector::Id(acc.account_id.clone()),
        &session,
        &client,
    )
    .expect("account resolved");
    let mut update = Map::new();
    update.insert(jmap_id.clone(), json!({ "keywords/$flagged": true }));
    let mut req = Request::new();
    req.call(
        "Email/set",
        json!({ "accountId": account, "update": Value::Object(update) }),
        "s",
    );
    let resp = req.send(&client, &session.api_url).expect("Email/set sent");
    let mr = resp.first().expect("a method response");
    assert!(
        mr.args
            .get("updated")
            .and_then(|u| u.get(&jmap_id))
            .is_some(),
        "server accepted the keyword update: {:?}",
        mr.args
    );

    let s2 = sync::import_jmap::run(
        common(&archive, false),
        import_cfg(AccountSelector::Id(acc.account_id.clone())),
    )
    .expect("second import");

    let conn = Connection::open(&archive).unwrap();
    let kw: String = conn
        .query_row(
            "SELECT keywords FROM emails WHERE id = ?1",
            [local_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        kw.contains("$flagged"),
        "delta re-import propagated the new flag into the archive: {kw}"
    );
    let em = s2
        .per_type
        .iter()
        .find(|(t, _)| *t == "Email")
        .map(|(_, c)| c.clone())
        .expect("email counts");
    assert!(
        em.updated >= 1,
        "the changed email was detected via Email/changes and refreshed (updated={})",
        em.updated
    );
    drop(conn);
    seeder::teardown(base_url()).expect("teardown");
    let _ = std::fs::remove_file(&archive);
}
