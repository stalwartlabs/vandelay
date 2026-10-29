/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod integration;

use std::collections::HashSet;

use integration::layouts::FileSpec;
use integration::validate::{
    blob_bytes, cleanup, common, count, file_node_id_by_path, file_path, open_archive, tmp_archive,
};
use integration::webdav::{AccountSeed, WebDav};
use integration::{Account, Endpoint};

use vandelay::error::Error;
use vandelay::sync::import_dav;
use vandelay::sync::import_dav::{DavAuth, DavImportConfig, DavKindArg, SourceChange};

fn webdav_config(account: &Account, http: &Endpoint) -> DavImportConfig {
    DavImportConfig {
        kind: DavKindArg::Webdav,
        url: format!("http://{}:{}/{}/", http.host, http.port, account.username),
        auth: DavAuth::Basic {
            user: account.username.clone(),
            password: account.password.clone(),
        },
        allow_cleartext: true,
        dav_connections: 2,
        multiget_batch: 25,
        source_change: SourceChange::Reject,
    }
}

#[test]
#[ignore = "requires Docker"]
fn webdav_starts_seeds_and_imports() {
    let w = WebDav::start().expect("webdav start");
    let seeds = w.seed_all().expect("webdav seed");
    assert_eq!(
        seeds.len(),
        w.accounts.len(),
        "seed should return stats for every account"
    );
    w.verify_seed().expect("webdav verify");
    for seed in &seeds {
        assert!(
            seed.directories > 0,
            "{}: expected directories seeded",
            seed.username
        );
        assert!(
            !seed.files.is_empty(),
            "{}: expected files seeded",
            seed.username
        );
    }

    for seed in &seeds {
        let account = w
            .accounts
            .iter()
            .find(|a| a.username == seed.username)
            .expect("account lookup");
        let archive = tmp_archive(&format!("webdav-{}", seed.username));
        let summary = import_dav::run(common(&archive), webdav_config(account, &w.http))
            .expect("webdav import");
        assert!(
            !summary.any_failed(),
            "{}: webdav import had failures: {summary:?}",
            seed.username
        );

        let conn = open_archive(&archive);
        let nodes = count(&conn, "file_nodes") as usize;
        let dirs: i64 = conn
            .query_row(
                "SELECT count(*) FROM file_nodes WHERE node_type = 'directory'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let files: i64 = conn
            .query_row(
                "SELECT count(*) FROM file_nodes WHERE node_type = 'file'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let expected_dirs = account.layout.files.iter().filter(|s| s.directory).count();
        let expected_files = account.layout.files.len() - expected_dirs;

        assert_eq!(
            files as usize, expected_files,
            "{}: every seeded file must land in archive (seeded {expected_files}, imported {files})",
            seed.username
        );
        assert_eq!(
            dirs as usize, expected_dirs,
            "{}: directory count mismatch (seeded layout dirs = {expected_dirs}, imported {dirs}); the account root is a virtual mount point, not a node (issue #18)",
            seed.username
        );
        assert_eq!(
            nodes,
            account.layout.files.len(),
            "{}: total file_nodes mismatch (layout only, no synthetic account-root node)",
            seed.username
        );

        let admin_root_nodes: i64 = conn
            .query_row(
                "SELECT count(*) FROM file_nodes WHERE name = ?1",
                [&account.username],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            admin_root_nodes, 0,
            "{}: the account root must not be materialised as a directory named after the account (issue #18)",
            seed.username
        );
        for spec in account.layout.files.iter().filter(|s| s.parent.is_none()) {
            let parent_id: Option<i64> = conn
                .query_row(
                    "SELECT parent_id FROM file_nodes WHERE name = ?1",
                    [spec.name],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                parent_id, None,
                "{}: top-level node {} must map to the target's implicit root (NULL parent), not nest under an account-root directory (issue #18)",
                seed.username, spec.name
            );
        }

        for spec in account.layout.files {
            let segments = layout_segments(account.layout.files, spec.key);
            let mut probes: Vec<Vec<&str>> = Vec::new();
            probes.push(segments.clone());
            probes.push({
                let mut s = vec![account.username.as_str()];
                s.extend(segments.iter().copied());
                s
            });
            let node_id = probes
                .iter()
                .find_map(|p| file_node_id_by_path(&conn, p))
                .unwrap_or_else(|| {
                    panic!(
                        "{}: file_node at layout path {segments:?} missing in archive",
                        seed.username
                    )
                });
            let reconstructed = file_path(&conn, node_id);
            let expected_relative = segments.join("/");
            assert!(
                reconstructed.ends_with(&expected_relative),
                "{}: path '{reconstructed}' does not end with seeded layout path '{expected_relative}'",
                seed.username
            );
            let (blob_id, node_type, media_type, created, modified): (
                Option<i64>,
                String,
                Option<String>,
                String,
                Option<String>,
            ) = conn
                .query_row(
                    "SELECT blob_id, node_type, media_type, created, modified
                     FROM file_nodes WHERE id = ?1",
                    [node_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .unwrap();
            let expected_type = if spec.directory { "directory" } else { "file" };
            assert_eq!(
                node_type, expected_type,
                "{}: {} node_type mismatch",
                seed.username, spec.name
            );
            assert!(
                !created.is_empty(),
                "{}: {} missing created timestamp",
                seed.username,
                spec.name
            );
            if !spec.directory {
                let blob_id = blob_id.expect("file blob_id");
                let actual = blob_bytes(&conn, blob_id);
                let expected = integration::webdav::synth_payload(spec.name).into_bytes();
                assert_eq!(
                    actual, expected,
                    "{}: file {} content round-tripped incorrectly",
                    seed.username, spec.name
                );
                let mt = media_type.unwrap_or_default();
                assert!(
                    !mt.is_empty(),
                    "{}: file {} missing media_type",
                    seed.username,
                    spec.name
                );
                let m = modified.unwrap_or_default();
                assert!(
                    !m.is_empty(),
                    "{}: file {} missing modified timestamp",
                    seed.username,
                    spec.name
                );
                let (blob_size, blob_hash): (i64, Vec<u8>) = conn
                    .query_row(
                        "SELECT length(data), hash FROM blobs WHERE id = ?1",
                        [blob_id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .unwrap();
                assert_eq!(
                    blob_size as usize,
                    expected.len(),
                    "{}: file {} blob length mismatch",
                    seed.username,
                    spec.name
                );
                assert_eq!(
                    blob_hash,
                    blake3::hash(&expected).as_bytes().to_vec(),
                    "{}: file {} blob.hash mismatch",
                    seed.username,
                    spec.name
                );
            }
        }

        let payload_groups = duplicate_payload_groups(account.layout.files);
        for (name, occurrences) in payload_groups {
            if occurrences < 2 {
                continue;
            }
            let payload = integration::webdav::synth_payload(&name).into_bytes();
            let hash = blake3::hash(&payload);
            let blob_id: i64 = conn
                .query_row(
                    "SELECT id FROM blobs WHERE hash = ?1",
                    [hash.as_bytes()],
                    |r| r.get(0),
                )
                .unwrap_or_else(|e| {
                    panic!(
                        "{}: duplicate-named payload {} missing single blob: {e}",
                        seed.username, name
                    )
                });
            let refs: i64 = conn
                .query_row(
                    "SELECT count(*) FROM file_nodes WHERE blob_id = ?1",
                    [blob_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                refs as usize, occurrences,
                "{}: blob {blob_id} for duplicate payload {name} should be referenced {occurrences} times, got {refs}",
                seed.username
            );
        }

        drop(conn);

        let summary2 = import_dav::run(common(&archive), webdav_config(account, &w.http))
            .expect("idempotent webdav re-import");
        assert!(
            !summary2.any_failed(),
            "{}: idempotent re-import had failures: {summary2:?}",
            seed.username
        );
        let conn = open_archive(&archive);
        let nodes2 = count(&conn, "file_nodes") as usize;
        let blobs1 = count(&conn, "blobs") as usize;
        assert_eq!(
            nodes2, nodes,
            "{}: idempotent re-import changed file_nodes count",
            seed.username
        );
        drop(conn);

        let vanish = seed.files.first().expect("at least one file seeded");
        w.delete_resource(account, &vanish.href)
            .expect("webdav delete file");
        let summary_after = import_dav::run(common(&archive), webdav_config(account, &w.http))
            .expect("re-import after delete");
        assert!(
            !summary_after.any_failed(),
            "{}: re-import after delete had failures",
            seed.username
        );
        let conn = open_archive(&archive);
        let files_after: i64 = conn
            .query_row(
                "SELECT count(*) FROM file_nodes WHERE node_type = 'file'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            files_after as usize,
            expected_files - 1,
            "{}: vanished file not pruned",
            seed.username
        );
        let blobs2 = count(&conn, "blobs") as usize;
        assert!(
            blobs2 <= blobs1,
            "{}: blob count after delete grew unexpectedly ({blobs1} -> {blobs2})",
            seed.username
        );
        drop(conn);

        let new_payload = format!(
            "added-{} content for vandelay webdav add-after-import probe\n",
            seed.username
        );
        let new_payload_bytes = new_payload.as_bytes().to_vec();
        let added_name = format!("added-{}.bin", seed.username);
        let _added_href = w
            .add_file(account, &[], &added_name, &new_payload_bytes)
            .expect("webdav add file");
        let summary_after_add = import_dav::run(common(&archive), webdav_config(account, &w.http))
            .expect("re-import after add");
        assert!(
            !summary_after_add.any_failed(),
            "{}: re-import after add had failures",
            seed.username
        );
        let conn = open_archive(&archive);
        let files_after_add: i64 = conn
            .query_row(
                "SELECT count(*) FROM file_nodes WHERE node_type = 'file'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            files_after_add as usize, expected_files,
            "{}: add-after-delete should restore baseline file count",
            seed.username
        );
        let added_hash = blake3::hash(&new_payload_bytes);
        let added_blob: i64 = conn
            .query_row(
                "SELECT id FROM blobs WHERE hash = ?1",
                [added_hash.as_bytes()],
                |r| r.get(0),
            )
            .expect("added file blob should be present");
        let refs: i64 = conn
            .query_row(
                "SELECT count(*) FROM file_nodes WHERE blob_id = ?1",
                [added_blob],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            refs, 1,
            "{}: added file should be referenced by exactly one file_node",
            seed.username
        );
        let added_node_id: i64 = conn
            .query_row(
                "SELECT id FROM file_nodes WHERE name = ?1",
                [&added_name],
                |r| r.get(0),
            )
            .expect("added file_node by name");
        let rebuilt = file_path(&conn, added_node_id);
        assert!(
            rebuilt.ends_with(&added_name),
            "{}: reconstructed path for added node should end with {added_name}, got {rebuilt}",
            seed.username
        );
        drop(conn);

        cleanup(&archive);
    }

    let primary = &w.accounts[0];
    let other = &w.accounts[1];
    let shared_archive = tmp_archive("webdav-source-change");
    import_dav::run(common(&shared_archive), webdav_config(primary, &w.http))
        .expect("seed archive with primary user");
    let err = import_dav::run(common(&shared_archive), webdav_config(other, &w.http))
        .expect_err("expected source-change abort");
    assert!(
        matches!(err, Error::SourceChange(_)),
        "expected SourceChange, got {err:?}"
    );
    cleanup(&shared_archive);

    w.stop().expect("webdav stop");
}

fn layout_segments<'a>(specs: &'a [FileSpec], key: &str) -> Vec<&'a str> {
    let mut parts: Vec<&'a str> = Vec::new();
    let mut cur = key;
    loop {
        let spec = specs.iter().find(|s| s.key == cur).expect("layout key");
        parts.push(spec.name);
        match spec.parent {
            Some(p) => cur = p,
            None => break,
        }
    }
    parts.reverse();
    parts
}

fn duplicate_payload_groups(specs: &[FileSpec]) -> Vec<(String, usize)> {
    let mut seen: Vec<(String, usize)> = Vec::new();
    let mut names: HashSet<String> = HashSet::new();
    for spec in specs.iter().filter(|s| !s.directory) {
        names.insert(spec.name.to_owned());
    }
    for name in names {
        let occurrences = specs
            .iter()
            .filter(|s| !s.directory && s.name == name)
            .count();
        seen.push((name, occurrences));
    }
    seen
}

fn _unused_seed_marker(_: &AccountSeed) {}
