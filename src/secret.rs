/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::env;
use std::io::{self, BufRead, IsTerminal, Write};
use std::process::{Command, Stdio};

use crate::error::Error;

pub struct Secret(String);

impl Secret {
    pub fn resolve(flag: Option<&str>, env_var: &str, prompt: &str) -> Result<Secret, Error> {
        if let Some(value) = flag {
            return Ok(Secret(value.to_owned()));
        }
        if let Ok(value) = env::var(env_var)
            && !value.is_empty()
        {
            return Ok(Secret(value));
        }
        if io::stdin().is_terminal() {
            return Secret::prompt(prompt);
        }
        Err(Error::Usage(format!(
            "no {prompt} provided: pass the flag, set ${env_var}, or run on a TTY for an interactive prompt"
        )))
    }

    pub fn into_inner(self) -> String {
        self.0
    }

    fn prompt(label: &str) -> Result<Secret, Error> {
        let mut stderr = io::stderr();
        write!(stderr, "{label}: ")?;
        stderr.flush()?;

        let mut line = String::new();
        let read = {
            let _echo = EchoGuard::disable();
            io::stdin().lock().read_line(&mut line)
        };
        writeln!(stderr)?;
        stderr.flush()?;
        read?;

        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            return Err(Error::Usage(format!("empty {label}")));
        }
        line.truncate(trimmed.len());
        Ok(Secret(line))
    }
}

#[derive(Clone, Copy)]
enum Echo {
    On,
    Off,
}

impl Echo {
    fn set(self) -> bool {
        let arg = match self {
            Echo::On => "echo",
            Echo::Off => "-echo",
        };
        Command::new("stty")
            .arg(arg)
            .stdin(Stdio::inherit())
            .status()
            .is_ok_and(|s| s.success())
    }
}

struct EchoGuard;

impl EchoGuard {
    fn disable() -> Option<EchoGuard> {
        Echo::Off.set().then_some(EchoGuard)
    }
}

impl Drop for EchoGuard {
    fn drop(&mut self) {
        Echo::On.set();
    }
}
