/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use serde_json::{Value, json};

use super::error::{SeedError, SeedResult};
use super::jmap::Jmap;

pub const STALWART: &[&str] = &["urn:ietf:params:jmap:core", "urn:stalwart:jmap"];

pub struct Admin {
    jmap: Jmap,
    account_id: String,
}

impl Admin {
    pub fn connect(base: &str, user: &str, password: &str) -> SeedResult<Admin> {
        let jmap = Jmap::connect(base, user, password)?;
        let account_id = jmap.account_id.clone();
        Ok(Admin { jmap, account_id })
    }

    fn created_id(response: &Value, key: &str) -> SeedResult<String> {
        response
            .get("created")
            .and_then(|c| c.get(key))
            .and_then(|o| o.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| SeedError::Shape(format!("create {key} missing id: {response}")))
    }

    pub fn domain_id(&self, name: &str) -> SeedResult<Option<String>> {
        let calls = json!([
            ["x:Domain/query", { "accountId": self.account_id, "filter": { "name": name } }, "q"],
            ["x:Domain/get", {
                "accountId": self.account_id,
                "#ids": { "resultOf": "q", "name": "x:Domain/query", "path": "/ids" },
                "properties": ["id", "name"]
            }, "g"]
        ]);
        let parsed = self.jmap.request(STALWART, calls)?;
        let responses = parsed
            .get("methodResponses")
            .and_then(Value::as_array)
            .ok_or_else(|| SeedError::Shape("missing methodResponses".to_owned()))?;
        let get = responses
            .get(1)
            .and_then(|r| r.get(1))
            .ok_or_else(|| SeedError::Shape("missing x:Domain/get".to_owned()))?;
        let list = get.get("list").and_then(Value::as_array);
        if let Some(list) = list {
            for entry in list {
                if entry.get("name").and_then(Value::as_str) == Some(name)
                    && let Some(id) = entry.get("id").and_then(Value::as_str)
                {
                    return Ok(Some(id.to_owned()));
                }
            }
        }
        Ok(None)
    }

    pub fn ensure_domain(&self, name: &str) -> SeedResult<String> {
        if let Some(id) = self.domain_id(name)? {
            return Ok(id);
        }
        let response = self.jmap.set_create(
            STALWART,
            "x:Domain/set",
            &self.account_id,
            json!({ "v": { "name": name } }),
            &[],
        )?;
        Self::created_id(&response, "v")
    }

    fn accounts_in_domain(&self, domain_id: &str) -> SeedResult<Vec<String>> {
        let response = self.jmap.call(
            STALWART,
            "x:Account/get",
            &self.account_id,
            json!({ "properties": ["id", "domainId"] }),
        )?;
        let mut ids = Vec::new();
        if let Some(list) = response.get("list").and_then(Value::as_array) {
            for entry in list {
                if entry.get("domainId").and_then(Value::as_str) == Some(domain_id)
                    && let Some(id) = entry.get("id").and_then(Value::as_str)
                {
                    ids.push(id.to_owned());
                }
            }
        }
        Ok(ids)
    }

    fn destroy(&self, method: &str, ids: &[String]) -> SeedResult<()> {
        if ids.is_empty() {
            return Ok(());
        }
        self.jmap.call(
            STALWART,
            method,
            &self.account_id,
            json!({ "destroy": ids }),
        )?;
        Ok(())
    }

    pub fn invalidate_caches(&self) -> SeedResult<()> {
        self.jmap.call(
            STALWART,
            "x:Action/set",
            &self.account_id,
            json!({ "create": { "c": { "@type": "InvalidateCaches" } } }),
        )?;
        Ok(())
    }

    pub fn enable_directory_queries(&self) -> SeedResult<()> {
        let response = self.jmap.call(
            STALWART,
            "x:Sharing/set",
            &self.account_id,
            json!({ "update": { "singleton": { "allowDirectoryQueries": true } } }),
        )?;
        if response.pointer("/updated/singleton").is_none() {
            return Err(SeedError::Shape(format!(
                "x:Sharing/set not applied: {response}"
            )));
        }
        let response = self.jmap.call(
            STALWART,
            "x:Action/set",
            &self.account_id,
            json!({ "create": { "r": { "@type": "ReloadSettings" } } }),
        )?;
        if response.pointer("/created/r").is_none() {
            return Err(SeedError::Shape(format!(
                "ReloadSettings failed: {response}"
            )));
        }
        Ok(())
    }

    pub fn teardown_domain(&self, name: &str) -> SeedResult<()> {
        let Some(domain_id) = self.domain_id(name)? else {
            return Ok(());
        };
        let accounts = self.accounts_in_domain(&domain_id)?;
        self.destroy("x:Account/set", &accounts)?;
        for _ in 0..6 {
            let response = self.jmap.call(
                STALWART,
                "x:Domain/set",
                &self.account_id,
                json!({ "destroy": [domain_id] }),
            )?;
            if response
                .get("destroyed")
                .and_then(Value::as_array)
                .map(|d| d.iter().any(|v| v.as_str() == Some(domain_id.as_str())))
                .unwrap_or(false)
            {
                return Ok(());
            }
            let linked = response
                .get("notDestroyed")
                .and_then(|n| n.get(&domain_id))
                .and_then(|e| e.get("linkedObjects"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if linked.is_empty() {
                return Err(SeedError::Method {
                    method: "x:Domain/set".to_owned(),
                    detail: format!("domain {name} not destroyed: {response}"),
                });
            }
            let mut dkim = Vec::new();
            let mut accounts = Vec::new();
            for obj in &linked {
                let kind = obj
                    .get("object")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let Some(id) = obj.get("id").and_then(Value::as_str) else {
                    continue;
                };
                match kind {
                    "DkimSignature" => dkim.push(id.to_owned()),
                    "Account" => accounts.push(id.to_owned()),
                    _ => {}
                }
            }
            self.destroy("x:Account/set", &accounts)?;
            self.destroy("x:DkimSignature/set", &dkim)?;
        }
        Err(SeedError::Method {
            method: "x:Domain/set".to_owned(),
            detail: format!("domain {name} still linked after retries"),
        })
    }

    pub fn create_account(
        &self,
        localpart: &str,
        domain_id: &str,
        password: &str,
        admin_role: bool,
    ) -> SeedResult<String> {
        let role = if admin_role { "Admin" } else { "User" };
        let create = json!({
            "a": {
                "@type": "User",
                "name": localpart,
                "domainId": domain_id,
                "credentials": { "0": { "@type": "Password", "secret": password } },
                "encryptionAtRest": { "@type": "Disabled" },
                "permissions": { "@type": "Inherit" },
                "roles": { "@type": role },
                "locale": "en-US"
            }
        });
        let response =
            self.jmap
                .set_create(STALWART, "x:Account/set", &self.account_id, create, &[])?;
        Self::created_id(&response, "a")
    }
}
