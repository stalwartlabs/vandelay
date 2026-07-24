<h1 align="center">Vandelay</h1>

<h3 align="center">
  The JMAP importer-exporter
  <br/>
  <sub>One-shot account migration and backup across JMAP, IMAP, CalDAV, CardDAV, WebDAV, ManageSieve, Maildir, Google Takeout and Microsoft Exchange.</sub>
</h3>

<br>

<p align="center">
  <a href="https://github.com/stalwartlabs/vandelay/actions/workflows/release.yml"><img src="https://img.shields.io/github/actions/workflow/status/stalwartlabs/vandelay/release.yml?style=flat-square" alt="build"></a>
  &nbsp;
  <a href="https://github.com/stalwartlabs/vandelay/releases/latest"><img src="https://img.shields.io/github/v/release/stalwartlabs/vandelay?style=flat-square" alt="release"></a>
  &nbsp;
  <a href="https://www.npmjs.com/package/@stalwartlabs/vandelay"><img src="https://img.shields.io/npm/v/@stalwartlabs/vandelay?style=flat-square&logo=npm" alt="npm"></a>
  &nbsp;
  <a href="https://github.com/stalwartlabs/vandelay/releases"><img src="https://img.shields.io/github/downloads/stalwartlabs/vandelay/total?style=flat-square" alt="downloads"></a>
  &nbsp;
  <img src="https://img.shields.io/badge/platforms-macOS%20%7C%20Linux%20%7C%20Windows-blue?style=flat-square" alt="platforms">
  &nbsp;
  <a href="#license"><img src="https://img.shields.io/badge/license-MIT%2FApache--2.0-blue?style=flat-square" alt="license"></a>
</p>

<br>

<img align="right" src="assets/importer-exporter.jpg" alt="He's an importer-exporter." hspace="20" vspace="6">

- Jerry: Well, what does *he* do?
- George: He's an importer.
- Jerry: Just imports, no exports?
- George: He's an importer/exporter, okay?

<br clear="all">
&nbsp;

## About

Vandelay is a one-shot account-migration utility for JMAP, the JMAP analogue of `imapsync` generalised to every JMAP data type (mail, contacts, calendars, identities, sieve scripts, file storage). It imports an account from a wide range of source protocols into a local SQLite "archive", then exports that archive into a target JMAP server. Import and export never talk to each other, only to the SQLite archive. One archive holds exactly one account.

Because the archive is a self-contained SQLite file that fully describes one account, vandelay doubles as a per-account backup tool: run an import on a schedule to capture a fresh snapshot, keep the resulting SQLite file as your backup, and restore it later by running an export against a JMAP target.

## Features

- **Many source protocols:**
    - JMAP
    - IMAP
    - CalDAV
    - CardDAV
    - WebDAV
    - ManageSieve
    - Maildir++
    - Google Takeout
    - Microsoft Exchange via EWS (*experimental*)
    - Microsoft Exchange Online via Graph (*experimental*)
- **One target protocol:** JMAP, with type-by-type stateless re-matching on every run.
- **Convergent:** Re-running an interrupted import or export picks up where it left off without bookkeeping flags.
- **Multi-threaded, no async runtime:** Blocking HTTP with per-server concurrency caps respected automatically.
- **Content-addressed blobs:** Emails, sieve scripts and file-storage payloads are stored once by BLAKE3 hash and deduplicated across the archive.
- **Dry-run everywhere:** Every command supports `--dry-run` to compute the full plan without writing.
- **Source-change protection:** An archive remembers which account it was filled from; pointing it at a different one fails unless explicitly permitted.
- **Read-only inspection:** A built-in `inspect` command dumps any object type from an archive for verification.

## Install

```sh
# macOS / Linux
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/stalwartlabs/vandelay/releases/latest/download/vandelay-installer.sh | sh

# Homebrew
brew install stalwartlabs/tap/vandelay

# Windows
powershell -ExecutionPolicy Bypass -c "irm https://github.com/stalwartlabs/vandelay/releases/latest/download/vandelay-installer.ps1 | iex"

# npm
npm install -g @stalwartlabs/vandelay

# From source
cargo install --path .
```

A signed `.msi` is also published with each release.

## Quick start

A typical run is two commands, one to capture a source account into a local SQLite archive and one to push that archive into a JMAP target.

```sh
# 1. Import an IMAP mailbox into a fresh archive.
export VANDELAY_PASSWORD='source-app-password'
vandelay import imap \
  --url imaps://imap.example.com \
  --auth-basic alice@example.com \
  alice.sqlite

# 2. Peek at what landed.
vandelay inspect alice.sqlite                 # per-type summary
vandelay inspect alice.sqlite mailbox         # mailbox tree
vandelay inspect alice.sqlite email --limit 5

# 3. Push the archive into a target JMAP server.
export VANDELAY_PASSWORD='target-password'
vandelay export \
  --url https://jmap.example.org \
  --auth-basic alice@example.org \
  --account-name alice@example.org \
  alice.sqlite
```

Both commands are convergent: rerun either to resume an interrupted run, or rerun `import` later to pick up new mail since the last snapshot. Use `--dry-run` on either side to compute the full plan without writing.

## CLI quick reference

```
vandelay <import|export|inspect> [args...]
```

All actions accept a set of global flags (verbosity, worker pool size, retry policy, TLS handling) in addition to action-specific ones. The most useful are:

| Flag | Purpose |
| --- | --- |
| `-j, --threads <N>` | Worker pool size (default: logical CPUs). Concurrent JMAP requests are additionally capped by the target server's `maxConcurrentRequests` / `maxConcurrentUpload` session limits, so raising this beyond what the server advertises has no effect. |
| `--dry-run` | Compute the full plan; perform no writes. |
| `-v`, `-vv`, `-vvv` | Increase log verbosity. |
| `-q, --quiet` | Warnings and errors only. |
| `--max-retries <N>` | Max retries per request on transient failures (default 5). |
| `--allow-invalid-certs` | Accept self-signed / invalid TLS certs. |

Credentials should be supplied via the `VANDELAY_PASSWORD` / `VANDELAY_TOKEN` / `VANDELAY_EWS_CLIENT_SECRET` / `VANDELAY_GRAPH_TOKEN` environment variables, or via an interactive prompt; passing them on the command line is supported but not recommended.

### Import

```
vandelay import <source> [source-args...] <ARCHIVE>
```

Reads a source account into the local SQLite `ARCHIVE` (created if absent). Every importer accepts `--allow-source-change` to override the archive's source-identity guard.

#### JMAP

```
vandelay import jmap \
  --url <URL> \
  (--auth-basic <USER> [--auth-password <PASS>] | --auth-bearer [TOKEN]) \
  (--account-id <ID> | --account-name <NAME>) \
  [--objects <list>] \
  <ARCHIVE>
```

Imports a single JMAP account. `--objects` accepts a comma-separated list of object tokens (`mailbox,email,calendar,calendarevent,addressbook,contactcard,identity,sievescript,participantidentity,filenode`); default is everything the server advertises.

#### IMAP

```
vandelay import imap \
  --url imap(s)://host[:port] \
  (--auth-basic <USER> [--auth-password <PASS>] | --auth-bearer [TOKEN] --auth-user <USER>) \
  [--include <REGEX>...] [--exclude <REGEX>...] [--exclude-special <ROLE>...] \
  [--folder <NAME>...] [--subscribed-only] [--noautomap] \
  [--include-deleted] [--allow-cleartext] [--compress] \
  [--fetch-batch <N>] [--imap-connections <1..8>] \
  <ARCHIVE>
```

Imports mail (and only mail) from any IMAP server. Folder selection is via `--include`/`--exclude` regexes (mutually exclusive with the exact-match `--folder`); `--exclude-special` drops by SPECIAL-USE role.

#### CalDAV

```
vandelay import caldav \
  --url <http(s)://host[/path]> \
  (--auth-basic <USER> [--auth-password <PASS>] | --auth-bearer [TOKEN]) \
  [--allow-cleartext] [--dav-connections <1..8>] [--multiget-batch <N>] \
  <ARCHIVE>
```

Discovers the user's CalDAV principal (or accepts a URL pointing straight at a calendar-home or calendar), then imports calendars and events.

#### CardDAV

```
vandelay import carddav \
  --url <http(s)://host[/path]> \
  (--auth-basic <USER> [--auth-password <PASS>] | --auth-bearer [TOKEN]) \
  [--allow-cleartext] [--dav-connections <1..8>] [--multiget-batch <N>] \
  <ARCHIVE>
```

Same shape as `caldav`, but for address books and contacts.

#### WebDAV

```
vandelay import webdav \
  --url <http(s)://host[/path]> \
  (--auth-basic <USER> [--auth-password <PASS>] | --auth-bearer [TOKEN]) \
  [--allow-cleartext] [--dav-connections <1..8>] [--multiget-batch <N>] \
  <ARCHIVE>
```

Imports a plain WebDAV file collection as a JMAP `FileNode` tree.

#### ManageSieve

```
vandelay import managesieve \
  --url sieve(s)://host[:port] \
  (--auth-basic <USER> [--auth-password <PASS>] | --auth-bearer [TOKEN] --auth-user <USER>) \
  [--allow-cleartext] \
  <ARCHIVE>
```

Imports sieve scripts only. Each script is content-addressed in the blob table; the active script is recorded.

#### Maildir

```
vandelay import maildir <MAILDIR> <ARCHIVE> \
  [--include <REGEX>...] [--exclude <REGEX>...] [--folder <NAME>...] \
  [--noautomap] [--include-deleted]
```

Reads a local Maildir++ tree (a directory with `cur/`, `new/`, `tmp/`). No network. Folder selection mirrors the IMAP importer.

#### Google Takeout

```
vandelay import takeout <PATH> <ARCHIVE> [--noautomap]
```

Scans a directory tree recursively for `.mbox`, `.ics` and `.vcf` files and imports them. Tailored to Google Takeout layouts but works on any such tree; system-label role assignment can be disabled with `--noautomap`.

#### Microsoft Exchange (EWS)

```
vandelay import exchange-ews \
  [--url <EWS-ENDPOINT>] [--mailbox <SMTP>] \
  [--mailbox-kind primary|archive|public-folders] \
  (--auth-basic <USER> [--auth-password <PASS>] \
   | --auth-bearer [TOKEN] [--ews-tenant <T> --ews-client-id <ID> \
                            (--ews-device-code | --ews-client-secret <SECRET>)]) \
  [--ews-connections <1..8>] [--ews-getitem-batch <N>] [--ews-attachment-batch <N>] \
  [--ews-no-syncfolderitems] \
  <ARCHIVE>
```

Imports a mailbox via EWS, against either on-prem Exchange Server or Exchange Online. Autodiscover is used when `--url` is omitted (a `--mailbox` SMTP address is then required). Supports Basic, pre-acquired bearer, interactive device-code OAuth, and app-only client-credentials OAuth.

#### Microsoft Exchange (Graph)

```
vandelay import exchange-graph \
  (--client-id <UUID> [--tenant <ID>] | --access-token [TOKEN]) \
  [--user <UPN|UUID>] \
  [--mailbox-kind primary|archive] \
  [--objects mail,calendar,contacts] \
  [--event-body-format text|html] \
  [--graph-connections <1..16>] [--top <1..1000>] \
  <ARCHIVE>
```

Imports a mailbox from Exchange Online via Microsoft Graph. Without `--access-token`, the interactive device-code flow is used. `public-folders` is rejected here (use `exchange-ews` instead).

### Export

```
vandelay export \
  --url <URL> \
  (--auth-basic <USER> [--auth-password <PASS>] | --auth-bearer [TOKEN]) \
  (--account-id <ID> | --account-name <NAME>) \
  [--objects <list>] [--prune [--yes]] \
  <ARCHIVE>
```

Stateless re-export of `ARCHIVE` into a target JMAP server account. The default behaviour is upsert-only: matched items are updated, unmatched local items are created, but pre-existing target items not covered by the archive are left alone.

`--prune` enables destructive reconciliation: target objects that do not match anything in the archive are deleted. The confirmation prompt can be skipped with `--yes` for automation. Export speaks JMAP only; no other target protocols are currently supported.

### Inspect

```
vandelay inspect <ARCHIVE> [TYPE] [--limit <N>] [--offset <N>]
```

Read-only dump of a local archive. This command never opens a network connection and never writes to the archive.

- Omit `TYPE` for a per-type summary (counts of every object kind plus blob storage stats).
- Pass an object type to dump it: `mailbox`, `email`, `identity`, `sievescript`, `addressbook`, `contactcard`, `calendar`, `calendarevent`, `participantidentity`, `filenode`.
- `mailbox` and `filenode` render as a tree (`--limit`/`--offset` are ignored); all other types use a paginated list and respect `--limit` and `--offset`.

## Testing

The default suite is hermetic (unit tests plus `mockito`-scripted JMAP/DAV/EWS/Graph behaviours) and needs no network or Docker:

```sh
cargo build
cargo clippy --all-targets
cargo test
```

### Live and integration tests (Docker required)

Live integration tests against a Stalwart server and container-based  tests against third-party servers (Dovecot, Cyrus, Radicale, Baikal, Apache `mod_dav`) are gated behind `--ignored`. **They require a running Docker daemon**: each test binary boots its own throwaway container via `testcontainers` (images are pulled automatically on first run), so Docker must be installed and `docker info` must succeed before invoking them.

Run them per binary, and always with `--test-threads=1`:

```sh
cargo test --test sync_jmap          -- --ignored --test-threads=1   # live JMAP import/export/convergence/prune
cargo test --test sync_imap          -- --ignored --test-threads=1
cargo test --test sync_managesieve   -- --ignored --test-threads=1
cargo test --test sync_maildir       -- --ignored --test-threads=1
cargo test --test sync_caldav        -- --ignored --test-threads=1
cargo test --test sync_carddav       -- --ignored --test-threads=1
cargo test --test sync_webdav        -- --ignored --test-threads=1
cargo test --test live_stalwart      -- --ignored --test-threads=1
cargo test --test seed_smoke         -- --ignored --test-threads=1
cargo test --test seed_only          -- --ignored --test-threads=1

# Third-party-server tests (one container each):
cargo test --test integration_radicale -- --ignored --test-threads=1
cargo test --test integration_baikal   -- --ignored --test-threads=1
cargo test --test integration_webdav   -- --ignored --test-threads=1
cargo test --test integration_dovecot  -- --ignored --test-threads=1
cargo test --test integration_cyrus    -- --ignored --test-threads=1

# Slow tests
cargo test --test mock_jmap -- --ignored
```

`--test-threads=1` is mandatory, not just advisory: within a binary every test shares a single per-binary container, and each test provisions then tears down the same disposable `vandelay.org` domain (and opens the archive with SQLite `EXCLUSIVE` locking). Separate binaries are isolated (each boots its own container on dynamic host ports), so plain `cargo test --test <name>` invocations are safe to run one after another.

## License

Licensed under either of

 * Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

## Copyright

Copyright (C) 2020, Stalwart Labs LLC
