//! The phone's control channel (JSON lines over the adb-forwarded tpl_ctl socket): battery, media and wallpaper
//! arrive here; play/pause/duck/volume go out.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

use crate::adb::CTL_PORT;

#[derive(Debug)]
pub enum PhoneEvent {
    /// `bt_name` is what Windows lists the phone as under Bluetooth (older phone servers don't send it).
    Hello { name: String, model: String, manufacturer: String, android: String, bt_name: String },
    /// The phone refused something the shell user is normally allowed (some brands gate it behind a switch).
    Refused { what: String, error: String },
    Battery { level: u32, charging: bool },
    Media { playing: bool, package: String, title: String },
    Volume(f64),
    /// The lock-screen wallpaper as a base64 JPEG (sent on connect and whenever it changes).
    Wallpaper(String),
    /// The answer to `ping()`.
    Pong,
    Gone,
}

#[derive(Clone, Default)]
pub struct PhoneControl {
    sock: Arc<Mutex<Option<TcpStream>>>,
}

impl PhoneControl {
    /// Connects and starts a reader thread. Returns false when the phone server isn't answering yet.
    pub fn connect(&self, events: Sender<PhoneEvent>) -> bool {
        let Ok(sock) = TcpStream::connect_timeout(&([127, 0, 0, 1], CTL_PORT).into(), Duration::from_secs(1)) else {
            return false;
        };
        let Ok(read) = sock.try_clone() else { return false };
        *self.sock.lock().unwrap() = Some(sock);
        let me = self.clone();
        thread::Builder::new()
            .name("tpl-phone".into())
            .spawn(move || {
                for line in BufReader::new(read).lines() {
                    let Ok(line) = line else { break };
                    let Ok(j) = serde_json::from_str::<Value>(&line) else { continue };
                    let s = |k: &str| j.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
                    let ev = match j.get("t").and_then(Value::as_str) {
                        Some("hello") => PhoneEvent::Hello {
                            name: s("name"),
                            model: s("model"),
                            manufacturer: s("manufacturer"),
                            android: s("android"),
                            bt_name: s("bt"),
                        },
                        Some("refused") => PhoneEvent::Refused { what: s("what"), error: s("error") },
                        Some("battery") => PhoneEvent::Battery {
                            level: j["level"].as_u64().unwrap_or(0) as u32,
                            charging: j["charging"].as_bool().unwrap_or(false),
                        },
                        Some("media") => PhoneEvent::Media {
                            playing: j["playing"].as_bool().unwrap_or(false),
                            package: s("pkg"),
                            title: s("title"),
                        },
                        Some("volume") => PhoneEvent::Volume(j["level"].as_f64().unwrap_or(0.0)),
                        Some("wallpaper") => PhoneEvent::Wallpaper(s("jpeg")),
                        Some("pong") => PhoneEvent::Pong,
                        _ => continue,
                    };
                    if events.send(ev).is_err() {
                        break;
                    }
                }
                *me.sock.lock().unwrap() = None;
                let _ = events.send(PhoneEvent::Gone);
            })
            .is_ok()
    }

    pub fn connected(&self) -> bool {
        self.sock.lock().unwrap().is_some()
    }

    /// Drops the connection (switching phones); the reader thread then reports `Gone`.
    pub fn disconnect(&self) {
        if let Some(s) = self.sock.lock().unwrap().take() {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }

    fn send(&self, v: Value) {
        if let Some(s) = self.sock.lock().unwrap().as_mut() {
            let _ = s.write_all(format!("{v}\n").as_bytes());
        }
    }

    pub fn play(&self) {
        self.send(json!({"c": "play"}));
    }
    pub fn pause(&self) {
        self.send(json!({"c": "pause"}));
    }
    pub fn duck(&self) {
        self.send(json!({"c": "duck", "level": 0.3}));
    }
    pub fn unduck(&self) {
        self.send(json!({"c": "unduck"}));
    }
    pub fn ping(&self) {
        self.send(json!({"c": "ping"}));
    }
    pub fn volume(&self, level: f64) {
        self.send(json!({"c": "volume", "level": level}));
    }
}
