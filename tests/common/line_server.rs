/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::io::{self, BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const IO_TIMEOUT: Duration = Duration::from_secs(5);

pub type Script = Box<dyn FnOnce(&mut MockConn) -> io::Result<()> + Send + 'static>;

pub struct MockConn {
    pub reader: BufReader<TcpStream>,
    pub writer: TcpStream,
}

impl MockConn {
    pub fn write_line(&mut self, s: &str) -> io::Result<()> {
        self.writer.write_all(s.as_bytes())?;
        self.writer.write_all(b"\r\n")?;
        self.writer.flush()
    }

    pub fn write_raw(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }

    pub fn read_line(&mut self) -> io::Result<String> {
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "client closed",
            ));
        }
        Ok(line.trim_end_matches(['\r', '\n']).to_owned())
    }
}

pub struct LineServer {
    pub addr: String,
    pub port: u16,
    _thread: thread::JoinHandle<()>,
}

impl LineServer {
    pub fn start(scripts: Vec<Script>, exhausted: &'static str) -> LineServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        let queue: Arc<Mutex<Vec<Script>>> =
            Arc::new(Mutex::new(scripts.into_iter().rev().collect()));
        let thread = thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else {
                    return;
                };
                let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
                let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
                let writer = stream.try_clone().expect("clone");
                let mut conn = MockConn {
                    reader: BufReader::new(stream),
                    writer,
                };
                let script = queue.lock().expect("queue").pop();
                match script {
                    Some(script) => {
                        thread::spawn(move || {
                            let _ = script(&mut conn);
                        });
                    }
                    None => {
                        let _ = conn.write_line(exhausted);
                    }
                }
            }
        });
        LineServer {
            addr: format!("127.0.0.1:{port}"),
            port,
            _thread: thread,
        }
    }

    pub fn url(&self, scheme: &str) -> String {
        format!("{scheme}://{}", self.addr)
    }
}
