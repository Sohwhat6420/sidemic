//! The per-session key on the phone server's two sockets, and the PC's half of the handshake that proves it.
//!
//! adb forwards the phone's `tpl_mic` and `tpl_ctl` sockets to loopback ports on the PC, where any local program
//! can connect to them. So every connection must first prove it knows a key made for this run of the phone server:
//!
//! 1. Each time the desktop starts the phone server it makes a fresh 32-byte [`Key`] and writes it to the server's
//!    standard input ([`Key::send`]), through the `adb shell` session that starts it. It is never on a command line,
//!    in an environment variable, in a file or in a log.
//! 2. On every connection the phone sends a 16-byte nonce `Ns`. The PC answers with its own nonce `Nc` and
//!    `HMAC-SHA-256(key, "sidemic/1 pc <channel>" || Ns || Nc)`. The phone checks that in constant time; if it is
//!    wrong, late or missing, it closes the connection, and the mic was never opened.
//! 3. The phone then sends `HMAC-SHA-256(key, "sidemic/1 phone <channel>" || Ns || Nc)`, which the PC checks in
//!    constant time, so a program that got in the way of the port can't pose as the phone. Only then does the
//!    stream (mic audio, or the control channel's JSON lines) begin.
//!
//! The key itself never crosses the socket, so a program listening on the port learns nothing it can use later,
//! and the two nonces make every proof good for one connection only. `<channel>` is `mic` or `ctl`, so a proof for
//! one socket is no use on the other.
//!
//! The phone's half is `Auth.java` and `Gate.java` in `phone-server/`; `tests/phone_gate.rs` runs this half against
//! that code over a real loopback socket.

use std::fmt;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Bytes in a session key.
pub const KEY_LEN: usize = 32;
/// Bytes in each side's nonce.
pub const NONCE_LEN: usize = 16;
/// Bytes in each side's proof (an HMAC-SHA-256 tag).
pub const MAC_LEN: usize = 32;

/// One run of the phone server's secret. It can't be printed: `Debug` shows no bytes, and there is no `Display`.
#[derive(Clone)]
pub struct Key([u8; KEY_LEN]);

impl Key {
    /// A fresh key from the operating system's random number generator.
    pub fn generate() -> io::Result<Key> {
        let mut k = [0u8; KEY_LEN];
        random(&mut k)?;
        Ok(Key(k))
    }

    /// A key from known bytes, for tests.
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Key {
        Key(bytes)
    }

    /// Writes the key as the one line the phone server reads from its standard input (64 hex digits and a
    /// newline), then wipes the copy it made.
    pub fn send(&self, to: &mut impl Write) -> io::Result<()> {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut line = [0u8; KEY_LEN * 2 + 1];
        for (i, b) in self.0.iter().enumerate() {
            line[2 * i] = HEX[(b >> 4) as usize];
            line[2 * i + 1] = HEX[(b & 15) as usize];
        }
        line[KEY_LEN * 2] = b'\n';
        let sent = to.write_all(&line).and_then(|_| to.flush());
        wipe(&mut line);
        sent
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Key(hidden)")
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        wipe(&mut self.0);
    }
}

/// Overwrites key material with zeros in a way the optimiser may not drop.
fn wipe(bytes: &mut [u8]) {
    for b in bytes.iter_mut() {
        // SAFETY: `b` is a valid, aligned, exclusive reference to one byte.
        unsafe { std::ptr::write_volatile(b, 0) };
    }
}

fn random(out: &mut [u8]) -> io::Result<()> {
    getrandom::fill(out).map_err(|e| io::Error::other(format!("no random numbers from the system: {e}")))
}

/// Which of the phone's sockets a proof is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    /// `tpl_mic`: the microphone stream.
    Mic,
    /// `tpl_ctl`: battery, media and wallpaper out; play, pause and volume in.
    Ctl,
}

impl Channel {
    fn label(self) -> &'static str {
        match self {
            Channel::Mic => "mic",
            Channel::Ctl => "ctl",
        }
    }
}

#[derive(Clone, Copy)]
enum Role {
    Pc,
    Phone,
}

/// The HMAC over `"sidemic/1 <role> <channel>" || Ns || Nc`, ready to finish or to verify.
fn mac(key: &Key, role: Role, channel: Channel, ns: &[u8; NONCE_LEN], nc: &[u8; NONCE_LEN]) -> Hmac<Sha256> {
    let mut m = Hmac::<Sha256>::new_from_slice(&key.0).expect("HMAC takes a key of any length");
    m.update(b"sidemic/1 ");
    m.update(match role {
        Role::Pc => b"pc ".as_slice(),
        Role::Phone => b"phone ".as_slice(),
    });
    m.update(channel.label().as_bytes());
    m.update(ns);
    m.update(nc);
    m
}

/// The PC's half of the handshake, on a connection that has just been opened. `Ok` means the other end proved it
/// holds `key` and the stream starts with the next byte; on any error nothing more must be read from `stream`.
pub fn prove(stream: &mut (impl Read + Write), key: &Key, channel: Channel) -> io::Result<()> {
    let mut nc = [0u8; NONCE_LEN];
    random(&mut nc)?;
    prove_with(stream, key, channel, &nc)
}

fn prove_with(stream: &mut (impl Read + Write), key: &Key, channel: Channel, nc: &[u8; NONCE_LEN]) -> io::Result<()> {
    let mut ns = [0u8; NONCE_LEN];
    stream.read_exact(&mut ns).map_err(|e| io::Error::new(e.kind(), format!("no challenge from the phone server ({e})")))?;
    let mut reply = [0u8; NONCE_LEN + MAC_LEN];
    reply[..NONCE_LEN].copy_from_slice(nc);
    reply[NONCE_LEN..].copy_from_slice(&mac(key, Role::Pc, channel, &ns, nc).finalize().into_bytes());
    stream.write_all(&reply)?;
    stream.flush()?;
    let mut ms = [0u8; MAC_LEN];
    // The phone closes without a word when the proof is wrong, so the usual reason for nothing here is a refusal.
    stream.read_exact(&mut ms).map_err(|e| {
        io::Error::new(io::ErrorKind::PermissionDenied, format!("the phone server refused this session's key, or the link dropped ({e})"))
    })?;
    mac(key, Role::Phone, channel, &ns, nc)
        .verify_slice(&ms)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "the other end isn't this session's phone server"))
}

/// [`prove`] on a TCP connection, with `timeout` on each read and write while it runs. The timeouts are cleared
/// again on success, so the stream behaves as it did before the handshake existed.
pub fn prove_tcp(sock: &mut TcpStream, key: &Key, channel: Channel, timeout: Duration) -> io::Result<()> {
    sock.set_read_timeout(Some(timeout))?;
    sock.set_write_timeout(Some(timeout))?;
    prove(sock, key, channel)?;
    sock.set_read_timeout(None)?;
    sock.set_write_timeout(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One end of a connection whose other end is scripted: reads come from `incoming`, writes land in `sent`.
    struct Scripted {
        incoming: io::Cursor<Vec<u8>>,
        sent: Vec<u8>,
    }

    impl Scripted {
        fn new(incoming: Vec<u8>) -> Scripted {
            Scripted { incoming: io::Cursor::new(incoming), sent: Vec::new() }
        }
    }

    impl Read for Scripted {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.incoming.read(buf)
        }
    }

    impl Write for Scripted {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.sent.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn fixed_key() -> Key {
        let mut k = [0u8; KEY_LEN];
        for (i, b) in k.iter_mut().enumerate() {
            *b = i as u8;
        }
        Key::from_bytes(k)
    }

    const NS: [u8; NONCE_LEN] = [0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xab, 0xac, 0xad, 0xae, 0xaf];
    const NC: [u8; NONCE_LEN] = [0xb0, 0xb1, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xbb, 0xbc, 0xbd, 0xbe, 0xbf];

    // The same four values are in phone-server/test/.../GateTest.java. They were worked out with a third
    // implementation (Python's hmac module), so a mistake shared by this crate and Auth.java would still show.
    const PC_MIC: &str = "a5fa245ae4a50642c62841eae9c8662d79e0fe345173d3d99ee2a97da223a224";
    const PC_CTL: &str = "e3ea7731e2ab5bf40ffe4776e51ee8f0dcb91ebbc2b73281e99886bb72589068";
    const PHONE_MIC: &str = "ab16aa6088a3c9c18d8e269c1a4b5049d6e7215a593bc947c912e07c0a29a07b";
    const PHONE_CTL: &str = "4be009072acc3544e0d64f0a8b2cf003c5a2a644b3f1cbeccf6b0da2fd673702";

    fn tag(role: Role, channel: Channel) -> Vec<u8> {
        mac(&fixed_key(), role, channel, &NS, &NC).finalize().into_bytes().to_vec()
    }

    #[test]
    fn the_proofs_match_the_vectors_shared_with_the_phone() {
        assert_eq!(hex(&tag(Role::Pc, Channel::Mic)), PC_MIC);
        assert_eq!(hex(&tag(Role::Pc, Channel::Ctl)), PC_CTL);
        assert_eq!(hex(&tag(Role::Phone, Channel::Mic)), PHONE_MIC);
        assert_eq!(hex(&tag(Role::Phone, Channel::Ctl)), PHONE_CTL);
    }

    #[test]
    fn hmac_sha256_matches_rfc_4231() {
        // Test case 2 of RFC 4231: key "Jefe", data "what do ya want for nothing?".
        let mut m = Hmac::<Sha256>::new_from_slice(b"Jefe").unwrap();
        m.update(b"what do ya want for nothing?");
        assert_eq!(hex(&m.finalize().into_bytes()), "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
    }

    #[test]
    fn a_phone_that_proves_the_key_is_accepted_and_the_key_is_never_sent() {
        let mut incoming = NS.to_vec();
        incoming.extend(tag(Role::Phone, Channel::Mic));
        incoming.extend(b"audio");
        let mut s = Scripted::new(incoming);
        prove_with(&mut s, &fixed_key(), Channel::Mic, &NC).unwrap();
        // Exactly Nc and the PC's proof went out...
        let mut want = NC.to_vec();
        want.extend(tag(Role::Pc, Channel::Mic));
        assert_eq!(s.sent, want);
        // ...and no run of the key's bytes is in it.
        assert!(!s.sent.windows(8).any(|w| fixed_key().0.windows(8).any(|k| k == w)));
        // The stream starts exactly after the phone's proof.
        let mut rest = Vec::new();
        s.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"audio");
    }

    #[test]
    fn a_fake_phone_cannot_start_a_stream() {
        // It sends a challenge and then audio straight away, as the server did before 1.0.1.
        let mut incoming = NS.to_vec();
        incoming.extend([0x11u8; 64]);
        let e = prove_with(&mut Scripted::new(incoming), &fixed_key(), Channel::Mic, &NC).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        // The right proof for the other socket, or for the PC's role (its own proof played back), is no better.
        for wrong in [tag(Role::Phone, Channel::Ctl), tag(Role::Pc, Channel::Mic)] {
            let mut incoming = NS.to_vec();
            incoming.extend(wrong);
            let e = prove_with(&mut Scripted::new(incoming), &fixed_key(), Channel::Mic, &NC).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        }
        // One flipped bit in the right proof.
        let mut nearly = tag(Role::Phone, Channel::Mic);
        nearly[MAC_LEN - 1] ^= 1;
        let mut incoming = NS.to_vec();
        incoming.extend(nearly);
        assert!(prove_with(&mut Scripted::new(incoming), &fixed_key(), Channel::Mic, &NC).is_err());
    }

    #[test]
    fn a_phone_that_closes_instead_of_proving_is_a_refusal() {
        // Nothing at all: the server isn't up yet.
        let e = prove_with(&mut Scripted::new(Vec::new()), &fixed_key(), Channel::Mic, &NC).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
        // A challenge, then closed: what the phone does to a wrong key.
        let e = prove_with(&mut Scripted::new(NS.to_vec()), &fixed_key(), Channel::Mic, &NC).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        // Half a proof, then closed.
        let mut incoming = NS.to_vec();
        incoming.extend(&tag(Role::Phone, Channel::Mic)[..MAC_LEN / 2]);
        let e = prove_with(&mut Scripted::new(incoming), &fixed_key(), Channel::Mic, &NC).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn every_key_and_nonce_is_fresh() {
        let (a, b) = (Key::generate().unwrap(), Key::generate().unwrap());
        assert_ne!(a.0, b.0);
        assert_ne!(a.0, [0u8; KEY_LEN]);
        // Two handshakes with one key against one challenge send different nonces, so different proofs.
        let run = || {
            let mut s = Scripted::new(NS.to_vec());
            let _ = prove(&mut s, &a, Channel::Mic);
            s.sent
        };
        let (first, second) = (run(), run());
        assert_eq!(first.len(), NONCE_LEN + MAC_LEN);
        assert_ne!(first[..NONCE_LEN], second[..NONCE_LEN]);
        assert_ne!(first[NONCE_LEN..], second[NONCE_LEN..]);
    }

    #[test]
    fn the_key_goes_out_as_one_hex_line_and_never_into_a_log() {
        let mut line = Vec::new();
        fixed_key().send(&mut line).unwrap();
        assert_eq!(line, b"000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\n");
        // Formatting a key (the only way one could reach a log line) shows none of it.
        let shown = format!("{:?} {:#?}", fixed_key(), Some(fixed_key()));
        assert_eq!(shown.matches("Key(hidden)").count(), 2);
        assert!(!shown.contains("0001") && !shown.contains("[0, 1"));
    }

    #[test]
    fn a_dropped_key_is_wiped() {
        // Run the key's own destructor where it stands, then look at the bytes it leaves behind.
        let mut key = std::mem::ManuallyDrop::new(Key::from_bytes([0x5a; KEY_LEN]));
        let bytes: *const [u8; KEY_LEN] = &key.0;
        // SAFETY: the key is dropped exactly once, here. Its only field is a plain byte array, which stays
        // readable storage on this stack frame after the destructor has run.
        let left = unsafe {
            std::mem::ManuallyDrop::drop(&mut key);
            std::ptr::read_volatile(bytes)
        };
        assert_eq!(left, [0u8; KEY_LEN]);
        let mut line = [0x5au8; 65];
        wipe(&mut line);
        assert_eq!(line, [0u8; 65]);
    }
}
