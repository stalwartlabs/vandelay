/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use quick_xml::events::Event;

use crate::exchange::ews::error::EwsError;
use crate::exchange::ews::parse::{Attrs, ItemElement, Ns, Trim, XmlBool};
use crate::exchange::ews::types::ItemId;

#[derive(Debug, Clone)]
pub struct SyncFolderItemsResponse {
    pub sync_state: String,
    pub more: bool,
    pub changes: Vec<SyncChange>,
}

#[derive(Debug, Clone)]
pub enum SyncChange {
    Create {
        id: ItemId,
        element: String,
        item_class: Option<String>,
    },
    Update {
        id: ItemId,
        element: String,
        item_class: Option<String>,
    },
    Delete {
        id: ItemId,
    },
    ReadFlagChange {
        id: ItemId,
        is_read: bool,
    },
}

#[derive(Clone, Copy)]
enum SyncElement {
    SyncState,
    IncludesLastItemInRange,
    Changes,
    Create,
    Update,
    Delete,
    ReadFlagChange,
    ItemId,
    IsRead,
    ItemClass,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SyncText {
    SyncState,
    IncludesLast,
}

impl SyncElement {
    fn parse(local: &str) -> Option<SyncElement> {
        hashify::map_ignore_case!(
            local.as_bytes(),
            SyncElement,
            "SyncState" => SyncElement::SyncState,
            "IncludesLastItemInRange" => SyncElement::IncludesLastItemInRange,
            "Changes" => SyncElement::Changes,
            "Create" => SyncElement::Create,
            "Update" => SyncElement::Update,
            "Delete" => SyncElement::Delete,
            "ReadFlagChange" => SyncElement::ReadFlagChange,
            "ItemId" => SyncElement::ItemId,
            "IsRead" => SyncElement::IsRead,
            "ItemClass" => SyncElement::ItemClass,
        )
        .copied()
    }
}

impl SyncChange {
    fn item_id_mut(&mut self) -> &mut ItemId {
        match self {
            SyncChange::Create { id, .. }
            | SyncChange::Update { id, .. }
            | SyncChange::Delete { id }
            | SyncChange::ReadFlagChange { id, .. } => id,
        }
    }
}

impl SyncFolderItemsResponse {
    pub fn parse(body: &[u8]) -> Result<SyncFolderItemsResponse, EwsError> {
        let mut xml = Trim::Text.reader(body);
        let mut buf = Vec::new();
        let mut sync_state = String::new();
        let mut more = false;
        let mut changes: Vec<SyncChange> = Vec::new();
        let mut current_text: Option<SyncText> = None;
        let mut in_changes = false;
        let mut current_change: Option<SyncChange> = None;
        let mut reading_is_read = false;
        let mut pending_is_read: Option<bool> = None;
        let mut reading_item_class = false;
        loop {
            buf.clear();
            let (ns, ev) = xml.read_resolved_event_into(&mut buf)?;
            let ns = Ns::of(&ns);
            match ev {
                Event::Start(ref e) | Event::Empty(ref e) => {
                    let local = e.local_name().into_inner();
                    let is_empty = matches!(ev, Event::Empty(_));
                    let element = SyncElement::parse(local);
                    match element {
                        Some(SyncElement::SyncState) if ns == Ns::Messages => {
                            current_text = Some(SyncText::SyncState);
                        }
                        Some(SyncElement::IncludesLastItemInRange) if ns == Ns::Messages => {
                            current_text = Some(SyncText::IncludesLast);
                        }
                        Some(SyncElement::Changes) if ns == Ns::Messages => in_changes = true,
                        _ if in_changes && ns == Ns::Types => match element {
                            Some(SyncElement::Create) => {
                                current_change = Some(SyncChange::Create {
                                    id: ItemId::default(),
                                    element: String::new(),
                                    item_class: None,
                                });
                            }
                            Some(SyncElement::Update) => {
                                current_change = Some(SyncChange::Update {
                                    id: ItemId::default(),
                                    element: String::new(),
                                    item_class: None,
                                });
                            }
                            Some(SyncElement::Delete) => {
                                current_change = Some(SyncChange::Delete {
                                    id: ItemId::default(),
                                });
                            }
                            Some(SyncElement::ReadFlagChange) => {
                                current_change = Some(SyncChange::ReadFlagChange {
                                    id: ItemId::default(),
                                    is_read: false,
                                });
                                pending_is_read = None;
                            }
                            Some(SyncElement::ItemId) => {
                                if let Some(change) = current_change.as_mut() {
                                    let id = change.item_id_mut();
                                    Attrs(e).ids(&mut id.id, &mut id.change_key);
                                }
                            }
                            Some(SyncElement::IsRead) => reading_is_read = true,
                            Some(SyncElement::ItemClass) => reading_item_class = !is_empty,
                            _ if ItemElement::is(local) => {
                                if let Some(
                                    SyncChange::Create { element, .. }
                                    | SyncChange::Update { element, .. },
                                ) = current_change.as_mut()
                                {
                                    local.clone_into(element);
                                }
                            }
                            _ => {}
                        },
                        _ => {}
                    }
                    if is_empty {
                        current_text = None;
                    }
                }
                Event::End(e) => {
                    match SyncElement::parse(e.local_name().into_inner()) {
                        Some(
                            SyncElement::Create
                            | SyncElement::Update
                            | SyncElement::Delete
                            | SyncElement::ReadFlagChange,
                        ) => {
                            if let Some(mut change) = current_change.take() {
                                if let SyncChange::ReadFlagChange { is_read, .. } = &mut change
                                    && let Some(v) = pending_is_read.take()
                                {
                                    *is_read = v;
                                }
                                changes.push(change);
                            }
                        }
                        Some(SyncElement::IsRead) => reading_is_read = false,
                        Some(SyncElement::ItemClass) => reading_item_class = false,
                        Some(SyncElement::Changes) => in_changes = false,
                        _ => {}
                    }
                    current_text = None;
                }
                Event::Text(t) => {
                    if reading_is_read {
                        pending_is_read = Some(XmlBool::parse(&t));
                    }
                    if reading_item_class
                        && let Some(
                            SyncChange::Create { item_class, .. }
                            | SyncChange::Update { item_class, .. },
                        ) = current_change.as_mut()
                    {
                        *item_class = Some(t.trim().to_owned());
                    }
                    match current_text {
                        Some(SyncText::SyncState) => sync_state = t.into_inner().into_owned(),
                        Some(SyncText::IncludesLast) => more = !XmlBool::parse(&t),
                        None => {}
                    }
                }
                Event::CData(c) => {
                    if current_text == Some(SyncText::SyncState) {
                        sync_state = c.into_inner().into_owned();
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(SyncFolderItemsResponse {
            sync_state,
            more,
            changes,
        })
    }
}
