CREATE TABLE IF NOT EXISTS sources (
    id            INTEGER PRIMARY KEY,
    kind          TEXT    NOT NULL CHECK (kind IN ('jmap','imap','caldav','carddav','webdav','managesieve','maildir','exchange_ews','exchange_graph','takeout')),
    session_url   TEXT    NOT NULL,
    account_id    TEXT    NOT NULL,
    account_name  TEXT,
    username      TEXT    NOT NULL,
    UNIQUE (kind, session_url, account_id)
);

CREATE TABLE IF NOT EXISTS blobs (
    id    INTEGER PRIMARY KEY,
    hash  BLOB    NOT NULL UNIQUE,
    data  BLOB    NOT NULL
);

CREATE INDEX IF NOT EXISTS blobs_hash_idx ON blobs (hash);

CREATE TABLE IF NOT EXISTS sync_id_jmap (
    source_id   INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    type_name   TEXT    NOT NULL,
    jmap_id     TEXT    NOT NULL,
    local_id    INTEGER NOT NULL,
    PRIMARY KEY (source_id, type_name, jmap_id),
    UNIQUE (source_id, type_name, local_id)
);

CREATE INDEX IF NOT EXISTS sync_id_jmap_local_idx
    ON sync_id_jmap (type_name, local_id);

CREATE TABLE IF NOT EXISTS sync_state_jmap (
    source_id   INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    type_name   TEXT    NOT NULL,
    state       TEXT    NOT NULL,
    PRIMARY KEY (source_id, type_name)
);

CREATE TABLE IF NOT EXISTS sync_id_imap (
    source_id    INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    type_name    TEXT    NOT NULL CHECK (type_name IN ('mailbox','email')),
    folder       TEXT    NOT NULL,
    uidvalidity  INTEGER NOT NULL,
    uid          INTEGER NOT NULL,
    local_id     INTEGER NOT NULL,
    PRIMARY KEY (source_id, type_name, folder, uidvalidity, uid),
    UNIQUE (source_id, type_name, local_id)
);

CREATE INDEX IF NOT EXISTS sync_id_imap_folder_idx
    ON sync_id_imap (source_id, type_name, folder);

CREATE TABLE IF NOT EXISTS imap_folder_state (
    source_id    INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    folder       TEXT    NOT NULL,
    uidvalidity  INTEGER NOT NULL,
    uidnext      INTEGER NOT NULL,
    last_seen    TEXT    NOT NULL,
    PRIMARY KEY (source_id, folder)
);

CREATE TABLE IF NOT EXISTS sync_id_dav (
    source_id        INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    type_name        TEXT    NOT NULL CHECK (type_name IN (
                                                 'calendar','calendarevent',
                                                 'addressbook','contactcard',
                                                 'filenode')),
    collection_href  TEXT    NOT NULL,
    item_href        TEXT    NOT NULL,
    etag             TEXT    NOT NULL DEFAULT '',
    local_id         INTEGER NOT NULL,
    PRIMARY KEY (source_id, type_name, item_href),
    UNIQUE (source_id, type_name, local_id)
);

CREATE INDEX IF NOT EXISTS sync_id_dav_collection_idx
    ON sync_id_dav (source_id, type_name, collection_href);

CREATE TABLE IF NOT EXISTS sync_id_managesieve (
    source_id  INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    name       TEXT    NOT NULL,
    local_id   INTEGER NOT NULL,
    PRIMARY KEY (source_id, name),
    UNIQUE (source_id, local_id)
);

CREATE TABLE IF NOT EXISTS sync_id_maildir (
    source_id   INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    type_name   TEXT    NOT NULL CHECK (type_name IN ('mailbox','email')),
    folder      TEXT    NOT NULL,
    unique_id   TEXT    NOT NULL,
    local_id    INTEGER NOT NULL,
    PRIMARY KEY (source_id, type_name, folder, unique_id),
    UNIQUE (source_id, type_name, local_id)
);

CREATE INDEX IF NOT EXISTS sync_id_maildir_folder_idx
    ON sync_id_maildir (source_id, type_name, folder);

CREATE TABLE IF NOT EXISTS sync_id_exchange_ews (
    source_id   INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    type_name   TEXT    NOT NULL CHECK (type_name IN (
                                            'mailbox','calendar','addressbook',
                                            'email','calendarevent','contactcard')),
    folder_id   TEXT    NOT NULL DEFAULT '',
    item_id     TEXT    NOT NULL,
    change_key  TEXT    NOT NULL DEFAULT '',
    sync_state  TEXT    NOT NULL DEFAULT '',
    local_id    INTEGER NOT NULL,
    PRIMARY KEY (source_id, type_name, item_id),
    UNIQUE (source_id, type_name, local_id)
);

CREATE INDEX IF NOT EXISTS sync_id_exchange_ews_folder_idx
    ON sync_id_exchange_ews (source_id, type_name, folder_id);

CREATE TABLE IF NOT EXISTS sync_id_exchange_graph (
    source_id   INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    type_name   TEXT    NOT NULL CHECK (type_name IN (
                                            'mailbox','email',
                                            'calendar','calendarevent',
                                            'addressbook','contactcard',
                                            'filenode')),
    graph_id    TEXT    NOT NULL,
    local_id    INTEGER NOT NULL,
    PRIMARY KEY (source_id, type_name, graph_id),
    UNIQUE (source_id, type_name, local_id)
);

CREATE INDEX IF NOT EXISTS sync_id_exchange_graph_type_idx
    ON sync_id_exchange_graph (source_id, type_name);

CREATE TABLE IF NOT EXISTS mailboxes (
    id             INTEGER PRIMARY KEY,
    name           TEXT    NOT NULL,
    parent_id      INTEGER REFERENCES mailboxes(id) ON DELETE RESTRICT,
    role           TEXT,
    sort_order     INTEGER NOT NULL DEFAULT 0,
    is_subscribed  INTEGER NOT NULL DEFAULT 1,
    UNIQUE (parent_id, name)
);

CREATE TABLE IF NOT EXISTS emails (
    id           INTEGER PRIMARY KEY,
    blob_id      INTEGER NOT NULL REFERENCES blobs(id),
    received_at  TEXT    NOT NULL,
    mailbox_ids  TEXT    NOT NULL CHECK (json_valid(mailbox_ids)),
    keywords     TEXT    NOT NULL DEFAULT '[]' CHECK (json_valid(keywords)),
    message_match TEXT   NOT NULL DEFAULT '{}' CHECK (json_valid(message_match))
);

CREATE INDEX IF NOT EXISTS emails_blob_idx ON emails (blob_id);

CREATE TABLE IF NOT EXISTS identities (
    id              INTEGER PRIMARY KEY,
    name            TEXT    NOT NULL DEFAULT '',
    email           TEXT    NOT NULL,
    reply_to        TEXT    CHECK (reply_to IS NULL OR json_valid(reply_to)),
    bcc             TEXT    CHECK (bcc IS NULL OR json_valid(bcc)),
    text_signature  TEXT    NOT NULL DEFAULT '',
    html_signature  TEXT    NOT NULL DEFAULT ''
);

CREATE TABLE IF NOT EXISTS sieve_scripts (
    id         INTEGER PRIMARY KEY,
    name       TEXT,
    is_active  INTEGER NOT NULL DEFAULT 0,
    blob_id    INTEGER NOT NULL REFERENCES blobs(id),
    UNIQUE (name)
);

CREATE UNIQUE INDEX IF NOT EXISTS sieve_scripts_one_active_idx
    ON sieve_scripts (is_active) WHERE is_active = 1;

CREATE TABLE IF NOT EXISTS address_books (
    id             INTEGER PRIMARY KEY,
    name           TEXT    NOT NULL,
    description    TEXT,
    sort_order     INTEGER NOT NULL DEFAULT 0,
    is_default     INTEGER NOT NULL DEFAULT 0,
    is_subscribed  INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS contact_cards (
    id                INTEGER PRIMARY KEY,
    uid               TEXT    NOT NULL UNIQUE,
    address_book_ids  TEXT    NOT NULL CHECK (json_valid(address_book_ids)),
    data              TEXT    NOT NULL CHECK (json_valid(data))
);

CREATE TABLE IF NOT EXISTS calendars (
    id                            INTEGER PRIMARY KEY,
    name                          TEXT    NOT NULL,
    description                   TEXT,
    color                         TEXT,
    sort_order                    INTEGER NOT NULL DEFAULT 0,
    is_subscribed                 INTEGER NOT NULL DEFAULT 1,
    is_visible                    INTEGER NOT NULL DEFAULT 1,
    is_default                    INTEGER NOT NULL DEFAULT 0,
    include_in_availability       TEXT    NOT NULL DEFAULT 'all'
                                      CHECK (include_in_availability IN ('all','attending','none')),
    default_alerts_with_time      TEXT    CHECK (default_alerts_with_time IS NULL
                                                  OR json_valid(default_alerts_with_time)),
    default_alerts_without_time   TEXT    CHECK (default_alerts_without_time IS NULL
                                                  OR json_valid(default_alerts_without_time)),
    time_zone                     TEXT
);

CREATE TABLE IF NOT EXISTS calendar_events (
    id                  INTEGER PRIMARY KEY,
    calendar_ids        TEXT    NOT NULL CHECK (json_valid(calendar_ids)),
    is_draft            INTEGER NOT NULL DEFAULT 0,
    use_default_alerts  INTEGER NOT NULL DEFAULT 0,
    data                TEXT    NOT NULL CHECK (json_valid(data)),
    data_type           TEXT    NOT NULL DEFAULT 'Event'
                         CHECK (data_type IN ('Event','Task','Note','Group'))
);

CREATE TABLE IF NOT EXISTS participant_identities (
    id                INTEGER PRIMARY KEY,
    name              TEXT    NOT NULL DEFAULT '',
    calendar_address  TEXT    NOT NULL,
    is_default        INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS file_nodes (
    id             INTEGER PRIMARY KEY,
    parent_id      INTEGER REFERENCES file_nodes(id) ON DELETE RESTRICT,
    node_type      TEXT    NOT NULL
                          CHECK (node_type IN ('file','directory','symlink')),
    blob_id        INTEGER REFERENCES blobs(id),
    target         TEXT    CHECK (target IS NULL OR json_valid(target)),
    name           TEXT    NOT NULL,
    media_type     TEXT,
    created        TEXT    NOT NULL,
    modified       TEXT,
    is_subscribed  INTEGER NOT NULL DEFAULT 1,
    role           TEXT,
    UNIQUE (parent_id, name)
);

CREATE INDEX IF NOT EXISTS file_nodes_parent_idx ON file_nodes (parent_id);
CREATE INDEX IF NOT EXISTS file_nodes_blob_idx ON file_nodes (blob_id);

CREATE TABLE IF NOT EXISTS sync_id_takeout (
    source_id      INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    type_name      TEXT    NOT NULL CHECK (type_name IN (
                                              'mailbox','email',
                                              'addressbook','contactcard',
                                              'calendar','calendarevent')),
    source_obj_id  TEXT    NOT NULL,
    local_id       INTEGER NOT NULL,
    PRIMARY KEY (source_id, type_name, source_obj_id),
    UNIQUE (source_id, type_name, local_id)
);

CREATE INDEX IF NOT EXISTS sync_id_takeout_type_idx
    ON sync_id_takeout (source_id, type_name);
