/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use quick_xml::events::Event;

use crate::exchange::ews::error::EwsError;
use crate::exchange::ews::parse::attachment::AttachmentElement;
use crate::exchange::ews::parse::{Attrs, ItemDepth, Ns, RawAttachmentRef, TextBuf, Trim};
use crate::exchange::ews::types::ItemId;

#[derive(Debug, Clone, Default)]
pub struct ContactItemRaw {
    pub id: ItemId,
    pub parent_folder_id: Option<String>,
    pub display_name: Option<String>,
    pub given_name: Option<String>,
    pub middle_name: Option<String>,
    pub surname: Option<String>,
    pub initials: Option<String>,
    pub nickname: Option<String>,
    pub company_name: Option<String>,
    pub department: Option<String>,
    pub job_title: Option<String>,
    pub generation: Option<String>,
    pub office_location: Option<String>,
    pub url: Option<String>,
    pub birthday: Option<String>,
    pub wedding_anniversary: Option<String>,
    pub notes: Option<String>,
    pub categories: Vec<String>,
    pub emails: Vec<(String, String)>,
    pub phones: Vec<(String, String)>,
    pub addresses: Vec<RawContactAddress>,
    pub ims: Vec<(String, String)>,
    pub manager: Option<String>,
    pub spouse: Option<String>,
    pub assistant: Option<String>,
    pub children: Vec<String>,
    pub companies: Vec<String>,
    pub profession: Option<String>,
    pub postal_address_index: Option<String>,
    pub created: Option<String>,
    pub last_modified: Option<String>,
    pub attachments: Vec<RawAttachmentRef>,
    pub is_group: bool,
    pub members: Vec<RawGroupMember>,
}

#[derive(Debug, Clone, Default)]
pub struct RawGroupMember {
    pub name: Option<String>,
    pub email: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RawContactAddress {
    pub key: String,
    pub street: Option<String>,
    pub city: Option<String>,
    pub state: Option<String>,
    pub country: Option<String>,
    pub postal_code: Option<String>,
}

#[derive(Clone, Copy)]
enum ContactText {
    DisplayName,
    GivenName,
    MiddleName,
    Surname,
    Initials,
    Nickname,
    CompanyName,
    Department,
    JobTitle,
    Generation,
    OfficeLocation,
    Url,
    Birthday,
    WeddingAnniversary,
    Manager,
    Spouse,
    Assistant,
    Profession,
    PostalAddressIndex,
    Notes,
    Created,
    LastModified,
    MemberName,
    MemberEmail,
    Category,
    Child,
    Company,
    EntryValue,
}

#[derive(Clone, Copy)]
enum AddressText {
    Street,
    City,
    State,
    Country,
    Postal,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryContainer {
    Email,
    Phone,
    Im,
    Address,
}

#[derive(Clone, Copy)]
enum ContactRoot {
    Contact,
    DistributionList,
}

#[derive(Clone, Copy)]
enum ContactField {
    Root(ContactRoot),
    ItemId,
    ParentFolderId,
    Text(ContactText),
    Members,
    Member,
    Mailbox,
    Name,
    EmailAddress,
    Categories,
    String,
    Children,
    Companies,
    Container(EntryContainer),
    Entry,
    Address(AddressText),
    Attachment(AttachmentElement),
}

impl ContactField {
    fn parse(local: &str) -> Option<ContactField> {
        hashify::map_ignore_case!(
            local.as_bytes(),
            ContactField,
            "Contact" => ContactField::Root(ContactRoot::Contact),
            "DistributionList" => ContactField::Root(ContactRoot::DistributionList),
            "ItemId" => ContactField::ItemId,
            "ParentFolderId" => ContactField::ParentFolderId,
            "DisplayName" => ContactField::Text(ContactText::DisplayName),
            "GivenName" => ContactField::Text(ContactText::GivenName),
            "MiddleName" => ContactField::Text(ContactText::MiddleName),
            "Surname" => ContactField::Text(ContactText::Surname),
            "Initials" => ContactField::Text(ContactText::Initials),
            "Nickname" => ContactField::Text(ContactText::Nickname),
            "CompanyName" => ContactField::Text(ContactText::CompanyName),
            "Department" => ContactField::Text(ContactText::Department),
            "JobTitle" => ContactField::Text(ContactText::JobTitle),
            "Generation" => ContactField::Text(ContactText::Generation),
            "OfficeLocation" => ContactField::Text(ContactText::OfficeLocation),
            "BusinessHomePage" => ContactField::Text(ContactText::Url),
            "Birthday" => ContactField::Text(ContactText::Birthday),
            "WeddingAnniversary" => ContactField::Text(ContactText::WeddingAnniversary),
            "Manager" => ContactField::Text(ContactText::Manager),
            "SpouseName" => ContactField::Text(ContactText::Spouse),
            "AssistantName" => ContactField::Text(ContactText::Assistant),
            "Profession" => ContactField::Text(ContactText::Profession),
            "PostalAddressIndex" => ContactField::Text(ContactText::PostalAddressIndex),
            "Body" => ContactField::Text(ContactText::Notes),
            "DateTimeCreated" => ContactField::Text(ContactText::Created),
            "LastModifiedTime" => ContactField::Text(ContactText::LastModified),
            "Members" => ContactField::Members,
            "Member" => ContactField::Member,
            "Mailbox" => ContactField::Mailbox,
            "Name" => ContactField::Name,
            "EmailAddress" => ContactField::EmailAddress,
            "Categories" => ContactField::Categories,
            "String" => ContactField::String,
            "Children" => ContactField::Children,
            "Companies" => ContactField::Companies,
            "EmailAddresses" => ContactField::Container(EntryContainer::Email),
            "PhoneNumbers" => ContactField::Container(EntryContainer::Phone),
            "ImAddresses" => ContactField::Container(EntryContainer::Im),
            "PhysicalAddresses" => ContactField::Container(EntryContainer::Address),
            "Entry" => ContactField::Entry,
            "Street" => ContactField::Address(AddressText::Street),
            "City" => ContactField::Address(AddressText::City),
            "State" => ContactField::Address(AddressText::State),
            "CountryOrRegion" => ContactField::Address(AddressText::Country),
            "PostalCode" => ContactField::Address(AddressText::Postal),
            "FileAttachment" => ContactField::Attachment(AttachmentElement::File),
            "ItemAttachment" => ContactField::Attachment(AttachmentElement::Item),
        )
        .copied()
    }
}

#[derive(Default)]
struct ContactState {
    item: ContactItemRaw,
    text_target: Option<ContactText>,
    address_text: Option<AddressText>,
    category_collecting: bool,
    children_collecting: bool,
    companies_collecting: bool,
    entry_container: Option<EntryContainer>,
    entry_key: Option<String>,
    current_address: Option<RawContactAddress>,
    in_members: bool,
    member_mailbox: bool,
    current_member: Option<RawGroupMember>,
}

impl ContactState {
    fn close_text(&mut self, target: ContactText, text: String) {
        let item = &mut self.item;
        let slot = match target {
            ContactText::DisplayName => &mut item.display_name,
            ContactText::GivenName => &mut item.given_name,
            ContactText::MiddleName => &mut item.middle_name,
            ContactText::Surname => &mut item.surname,
            ContactText::Initials => &mut item.initials,
            ContactText::Nickname => &mut item.nickname,
            ContactText::CompanyName => &mut item.company_name,
            ContactText::Department => &mut item.department,
            ContactText::JobTitle => &mut item.job_title,
            ContactText::Generation => &mut item.generation,
            ContactText::OfficeLocation => &mut item.office_location,
            ContactText::Url => &mut item.url,
            ContactText::Birthday => &mut item.birthday,
            ContactText::WeddingAnniversary => &mut item.wedding_anniversary,
            ContactText::Manager => &mut item.manager,
            ContactText::Spouse => &mut item.spouse,
            ContactText::Assistant => &mut item.assistant,
            ContactText::Profession => &mut item.profession,
            ContactText::PostalAddressIndex => &mut item.postal_address_index,
            ContactText::Notes => &mut item.notes,
            ContactText::Created => &mut item.created,
            ContactText::LastModified => &mut item.last_modified,
            ContactText::MemberName => match self.current_member.as_mut() {
                Some(m) => &mut m.name,
                None => return,
            },
            ContactText::MemberEmail => match self.current_member.as_mut() {
                Some(m) => &mut m.email,
                None => return,
            },
            ContactText::Category => return item.categories.push(text),
            ContactText::Child => return item.children.push(text),
            ContactText::Company => return item.companies.push(text),
            ContactText::EntryValue => {
                let key = self.entry_key.clone().unwrap_or_default();
                let list = match self.entry_container {
                    Some(EntryContainer::Email) => &mut item.emails,
                    Some(EntryContainer::Phone) => &mut item.phones,
                    Some(EntryContainer::Im) => &mut item.ims,
                    Some(EntryContainer::Address) | None => return,
                };
                return list.push((key, text));
            }
        };
        *slot = Some(text);
    }

    fn close_address(&mut self, target: AddressText, text: String) {
        let Some(addr) = self.current_address.as_mut() else {
            return;
        };
        let slot = match target {
            AddressText::Street => &mut addr.street,
            AddressText::City => &mut addr.city,
            AddressText::State => &mut addr.state,
            AddressText::Country => &mut addr.country,
            AddressText::Postal => &mut addr.postal_code,
        };
        *slot = Some(text);
    }

    fn close_element(&mut self, field: ContactField) {
        match field {
            ContactField::Categories => self.category_collecting = false,
            ContactField::Children => self.children_collecting = false,
            ContactField::Companies => self.companies_collecting = false,
            ContactField::Container(_) => self.entry_container = None,
            ContactField::Entry => {
                if let Some(addr) = self.current_address.take() {
                    self.item.addresses.push(addr);
                }
                self.entry_key = None;
            }
            ContactField::Members => self.in_members = false,
            ContactField::Mailbox => self.member_mailbox = false,
            ContactField::Member => {
                if let Some(m) = self.current_member.take()
                    && (m.email.is_some() || m.name.is_some())
                {
                    self.item.members.push(m);
                }
            }
            _ => {}
        }
    }
}

impl ContactItemRaw {
    pub fn parse(inner_xml: &str) -> Result<ContactItemRaw, EwsError> {
        let mut xml = Trim::Keep.reader(inner_xml.as_bytes());
        let mut buf = Vec::new();
        let mut st = ContactState::default();
        let mut seen_root_contact = false;
        let mut depth = ItemDepth::default();
        let mut cur = TextBuf::default();
        loop {
            buf.clear();
            let (ns, ev) = xml.read_resolved_event_into(&mut buf)?;
            match ev {
                Event::Start(ref e) | Event::Empty(ref e) => {
                    let field = ContactField::parse(e.local_name().into_inner());
                    let is_empty = matches!(ev, Event::Empty(_));
                    let level = depth.open(is_empty);
                    if !seen_root_contact && let Some(ContactField::Root(root)) = field {
                        seen_root_contact = true;
                        depth.enter_item(level);
                        if let ContactRoot::DistributionList = root {
                            st.item.is_group = true;
                        }
                        continue;
                    }
                    if Ns::of(&ns) != Ns::Types {
                        continue;
                    }
                    cur.0.clear();
                    match field {
                        Some(ContactField::ItemId) => {
                            if depth.is_item_child(level) {
                                Attrs(e).ids(&mut st.item.id.id, &mut st.item.id.change_key);
                            }
                        }
                        Some(ContactField::ParentFolderId) => {
                            if let Some(pid) = Attrs(e).parent_id() {
                                st.item.parent_folder_id = Some(pid);
                            }
                        }
                        Some(ContactField::Text(target)) => st.text_target = Some(target),
                        Some(ContactField::Members) => st.in_members = true,
                        Some(ContactField::Member) if st.in_members => {
                            st.current_member = Some(RawGroupMember::default());
                        }
                        Some(ContactField::Mailbox) if st.in_members => st.member_mailbox = true,
                        Some(ContactField::Name) if st.member_mailbox => {
                            st.text_target = Some(ContactText::MemberName);
                        }
                        Some(ContactField::EmailAddress) if st.member_mailbox => {
                            st.text_target = Some(ContactText::MemberEmail);
                        }
                        Some(ContactField::Categories) => st.category_collecting = true,
                        Some(ContactField::String) if st.category_collecting => {
                            st.text_target = Some(ContactText::Category);
                        }
                        Some(ContactField::Children) => st.children_collecting = true,
                        Some(ContactField::String) if st.children_collecting => {
                            st.text_target = Some(ContactText::Child);
                        }
                        Some(ContactField::Companies) => st.companies_collecting = true,
                        Some(ContactField::String) if st.companies_collecting => {
                            st.text_target = Some(ContactText::Company);
                        }
                        Some(ContactField::Container(container)) => {
                            st.entry_container = Some(container);
                        }
                        Some(ContactField::Entry) => {
                            st.entry_key = Attrs(e).value("Key");
                            if st.entry_container == Some(EntryContainer::Address) {
                                st.current_address = Some(RawContactAddress {
                                    key: st.entry_key.clone().unwrap_or_default(),
                                    ..RawContactAddress::default()
                                });
                            } else {
                                st.text_target = Some(ContactText::EntryValue);
                            }
                        }
                        _ if st.current_address.is_some() => {
                            st.address_text = match field {
                                Some(ContactField::Address(target)) => Some(target),
                                _ => None,
                            };
                        }
                        Some(ContactField::Attachment(element)) => {
                            let att = RawAttachmentRef::read(&mut xml, element)?;
                            st.item.attachments.push(att);
                        }
                        _ => st.text_target = None,
                    }
                    if is_empty {
                        st.text_target = None;
                        st.address_text = None;
                    }
                }
                Event::End(e) => {
                    depth.close();
                    if let Some(target) = st.address_text {
                        if st.current_address.is_some() {
                            st.close_address(target, cur.take());
                        }
                    } else if let Some(target) = st.text_target {
                        st.close_text(target, cur.take());
                    }
                    cur.0.clear();
                    st.text_target = None;
                    st.address_text = None;
                    if let Some(field) = ContactField::parse(e.local_name().into_inner()) {
                        st.close_element(field);
                    }
                }
                Event::Text(ref t) => cur.0.push_str(t),
                Event::CData(ref c) => cur.0.push_str(c),
                Event::GeneralRef(ref g) => cur.push_ref(g),
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(st.item)
    }
}
