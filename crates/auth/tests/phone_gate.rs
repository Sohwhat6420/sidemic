//! The PC's half of the handshake (this crate) against the phone's half (`Auth.java` and `Gate.java`, the classes
//! the phone server runs), over a real loopback TCP socket: what adb's forward of `tpl_mic` looks like to a
//! program on the PC. `GateTest serve` reads the session key from its standard input exactly as the phone server
//! does, and sends a marker where the phone would send mic audio.
//!
//! Needs a JDK (`javac` and `java`, from `JAVA_HOME` or the PATH); no Android SDK and no phone.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use tpl_auth::{prove, prove_tcp, Channel, Key, MAC_LEN, NONCE_LEN};

/// What `GateTest` sends a proven connection, in place of mic audio.
const STREAM: &[u8] = b"sidemic-test-stream\n";
/// How long the test's handshakes wait on each read and write.
const PATIENCE: Duration = Duration::from_secs(20);

fn tool(name: &str) -> PathBuf {
    let exe = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    std::env::var_os("JAVA_HOME").map(|home| Path::new(&home).join("bin").join(&exe)).filter(|p| p.is_file()).unwrap_or_else(|| exe.into())
}

/// The phone's handshake classes and `GateTest`, compiled once per test run with plain `javac`.
fn classes() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let phone = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../phone-server");
        let server = phone.join("src/com/truephonelink/server");
        let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join("phone-gate-classes");
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&out).unwrap();
        let done = Command::new(tool("javac"))
            .args(["-encoding", "UTF-8", "-Xlint:-options", "-d"])
            .arg(&out)
            .arg(server.join("Auth.java"))
            .arg(server.join("Gate.java"))
            .arg(server.join("Log.java"))
            .arg(phone.join("test/com/truephonelink/server/GateTest.java"))
            .output()
            .expect("these tests need a JDK: javac wasn't found in JAVA_HOME or on the PATH");
        assert!(done.status.success(), "javac failed:\n{}", String::from_utf8_lossy(&done.stderr));
        out
    })
}

fn gate_test() -> Command {
    let mut c = Command::new(tool("java"));
    c.arg("-cp").arg(classes()).arg("com.truephonelink.server.GateTest");
    c
}

/// The key as the hex line the desktop writes to the phone server's standard input (without the newline).
fn hex_of(key: &Key) -> String {
    let mut line = Vec::new();
    key.send(&mut line).unwrap();
    String::from_utf8(line).unwrap().trim().to_owned()
}

/// A running `GateTest serve`: the phone's door on a loopback port.
struct Phone {
    child: Child,
    port: u16,
    /// Everything it has printed, on standard output and standard error: the desktop app writes both to its log.
    log: Arc<Mutex<String>>,
}

impl Phone {
    /// Starts it and hands it `key` on standard input, the way the desktop starts the real phone server.
    fn start(key: &Key, channel: &str, deadline_ms: u32) -> Phone {
        let mut child = gate_test()
            .args(["serve", channel, &deadline_ms.to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("java wasn't found in JAVA_HOME or on the PATH");
        // The pipe stays open in `child`, as it does in the desktop app.
        key.send(child.stdin.as_mut().unwrap()).unwrap();
        let log = Arc::new(Mutex::new(String::new()));
        let (tx, rx) = mpsc::channel();
        let (out, kept) = (child.stdout.take().unwrap(), log.clone());
        thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if let Some(port) = line.strip_prefix("port ") {
                    let _ = tx.send(port.trim().parse::<u16>().unwrap());
                }
                let mut log = kept.lock().unwrap();
                log.push_str(&line);
                log.push('\n');
            }
        });
        let (err, kept) = (child.stderr.take().unwrap(), log.clone());
        thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let mut log = kept.lock().unwrap();
                log.push_str(&line);
                log.push('\n');
            }
        });
        let port = rx.recv_timeout(Duration::from_secs(60)).expect("GateTest didn't start listening");
        Phone { child, port, log }
    }

    fn connect(&self) -> TcpStream {
        let s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(PATIENCE)).unwrap();
        s
    }

    fn log(&self) -> String {
        self.log.lock().unwrap().clone()
    }

    /// Waits until the log contains `text`.
    fn logged(&self, text: &str) -> bool {
        let until = Instant::now() + Duration::from_secs(20);
        while Instant::now() < until {
            if self.log().contains(text) {
                return true;
            }
            thread::sleep(Duration::from_millis(50));
        }
        false
    }
}

impl Drop for Phone {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Reads until the other end closes (a reset counts: Windows reports one when the closer had unread data).
/// Returns what arrived and how long the close took; panics if the connection is still open after `PATIENCE`.
fn until_closed(s: &mut TcpStream) -> (Vec<u8>, Duration) {
    let started = Instant::now();
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                panic!("the connection was left open ({} bytes arrived)", got.len())
            }
            Err(_) => break,
        }
    }
    (got, started.elapsed())
}

fn has_stream(bytes: &[u8]) -> bool {
    bytes.windows(STREAM.len()).any(|w| w == STREAM)
}

#[test]
fn the_phone_code_passes_its_own_checks() {
    let done = gate_test().arg("check").output().unwrap();
    assert!(done.status.success(), "{}{}", String::from_utf8_lossy(&done.stdout), String::from_utf8_lossy(&done.stderr));
}

#[test]
fn a_connection_that_proves_the_key_gets_the_stream() {
    let key = Key::generate().unwrap();
    let phone = Phone::start(&key, "mic", 3000);
    let mut s = phone.connect();
    prove_tcp(&mut s, &key, Channel::Mic, PATIENCE).unwrap();
    // The handshake's timeouts are gone again, so the stream that follows is read as it was before 1.0.1.
    assert_eq!((s.read_timeout().unwrap(), s.write_timeout().unwrap()), (None, None));
    let mut stream = [0u8; STREAM.len()];
    s.read_exact(&mut stream).unwrap();
    assert_eq!(stream, STREAM);
    // The control socket is the same door with its own label.
    let phone = Phone::start(&key, "ctl", 3000);
    let mut s = phone.connect();
    prove_tcp(&mut s, &key, Channel::Ctl, PATIENCE).unwrap();
    s.read_exact(&mut stream).unwrap();
    assert_eq!(stream, STREAM);
}

#[test]
fn a_local_connection_that_proves_nothing_gets_no_stream_and_is_closed() {
    let key = Key::generate().unwrap();
    let phone = Phone::start(&key, "mic", 1500);
    // What any program on the PC could do before 1.0.1: connect and listen.
    let mut s = phone.connect();
    let (got, took) = until_closed(&mut s);
    assert_eq!(got.len(), NONCE_LEN, "only the challenge may arrive");
    assert!(!has_stream(&got));
    assert!(took >= Duration::from_millis(1000) && took < Duration::from_secs(15), "closed after {took:?}, not at the 1.5 s deadline");
    assert!(phone.logged("mic: refused 1 connection that didn't prove this session's key"), "{}", phone.log());
}

#[test]
fn a_connection_that_sends_something_else_gets_no_stream() {
    let key = Key::generate().unwrap();
    let phone = Phone::start(&key, "mic", 1500);
    let another_key = hex_of(&Key::generate().unwrap());
    let guesses: [&[u8]; 5] = [
        b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        &[0u8; NONCE_LEN + MAC_LEN],
        &[0xffu8; NONCE_LEN + MAC_LEN],
        &[0x41u8; 4096],
        another_key.as_bytes(),
    ];
    for guess in guesses {
        let mut s = phone.connect();
        s.write_all(guess).unwrap();
        let (got, _) = until_closed(&mut s);
        assert!(got.len() <= NONCE_LEN && !has_stream(&got), "{} bytes came back for a {}-byte guess", got.len(), guess.len());
    }
}

#[test]
fn a_wrong_key_is_refused() {
    let key = Key::generate().unwrap();
    let phone = Phone::start(&key, "mic", 3000);
    let mut s = phone.connect();
    let wrong = Key::generate().unwrap();
    let e = prove_tcp(&mut s, &wrong, Channel::Mic, PATIENCE).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::PermissionDenied, "{e}");
    let (got, _) = until_closed(&mut s);
    assert!(got.is_empty(), "{} bytes followed a refusal", got.len());
    // A key that differs in one bit is as wrong as any other.
    let mut line = hex_of(&key).into_bytes();
    line[63] = if line[63] == b'0' { b'1' } else { b'0' };
    let mut near = [0u8; 32];
    for (i, b) in near.iter_mut().enumerate() {
        *b = u8::from_str_radix(std::str::from_utf8(&line[2 * i..2 * i + 2]).unwrap(), 16).unwrap();
    }
    let mut s = phone.connect();
    assert!(prove_tcp(&mut s, &Key::from_bytes(near), Channel::Mic, PATIENCE).is_err());
    // The door still opens for the right key afterwards.
    let mut s = phone.connect();
    prove_tcp(&mut s, &key, Channel::Mic, PATIENCE).unwrap();
}

#[test]
fn the_proof_for_the_other_socket_is_refused() {
    let key = Key::generate().unwrap();
    let phone = Phone::start(&key, "mic", 3000);
    let mut s = phone.connect();
    assert!(prove_tcp(&mut s, &key, Channel::Ctl, PATIENCE).is_err());
    let (got, _) = until_closed(&mut s);
    assert!(got.is_empty());
}

/// A connection that keeps a copy of everything written to it: an eavesdropper's view of the PC's side.
struct Recorded {
    inner: TcpStream,
    sent: Vec<u8>,
}

impl Read for Recorded {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Write for Recorded {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.sent.extend_from_slice(&buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[test]
fn a_recorded_proof_is_refused_on_a_new_connection() {
    let key = Key::generate().unwrap();
    let phone = Phone::start(&key, "mic", 3000);
    let mut first = Recorded { inner: phone.connect(), sent: Vec::new() };
    prove(&mut first, &key, Channel::Mic).unwrap();
    assert_eq!(first.sent.len(), NONCE_LEN + MAC_LEN);
    drop(first.inner);
    // Played back word for word to a new connection, it answers the wrong challenge.
    let mut s = phone.connect();
    let mut challenge = [0u8; NONCE_LEN];
    s.read_exact(&mut challenge).unwrap();
    s.write_all(&first.sent).unwrap();
    let (got, _) = until_closed(&mut s);
    assert!(got.is_empty(), "{} bytes followed a replayed proof", got.len());
}

#[test]
fn the_deadline_is_only_for_proving_the_key() {
    let key = Key::generate().unwrap();
    let phone = Phone::start(&key, "mic", 1000);
    let mut s = phone.connect();
    prove_tcp(&mut s, &key, Channel::Mic, PATIENCE).unwrap();
    let mut stream = [0u8; STREAM.len()];
    s.read_exact(&mut stream).unwrap();
    // Well past the 1 s deadline, a proven connection is still open: nothing more to read, but not closed.
    thread::sleep(Duration::from_millis(2500));
    s.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
    let e = s.read(&mut stream).expect_err("a proven connection was closed at the handshake deadline");
    assert!(matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut), "{e}");
}

#[test]
fn silent_connections_that_came_first_do_not_keep_the_laptop_out() {
    let key = Key::generate().unwrap();
    // A long deadline, so that nothing here is closed by it.
    let phone = Phone::start(&key, "mic", 60_000);
    let mut idle: Vec<TcpStream> = (0..20).map(|_| phone.connect()).collect();
    // The laptop still gets in at once...
    let started = Instant::now();
    let mut s = phone.connect();
    prove_tcp(&mut s, &key, Channel::Mic, PATIENCE).unwrap();
    let mut stream = [0u8; STREAM.len()];
    s.read_exact(&mut stream).unwrap();
    assert!(started.elapsed() < Duration::from_secs(15));
    // ...and the oldest silent connections were closed to make room, long before the deadline.
    let (got, took) = until_closed(&mut idle[0]);
    assert!(got.len() <= NONCE_LEN && took < Duration::from_secs(15));
}

#[test]
fn proven_connections_are_served_one_at_a_time() {
    let key = Key::generate().unwrap();
    let phone = Phone::start(&key, "mic", 3000);
    let mut first = phone.connect();
    prove_tcp(&mut first, &key, Channel::Mic, PATIENCE).unwrap();
    let mut stream = [0u8; STREAM.len()];
    first.read_exact(&mut stream).unwrap();
    // A second proven connection waits its turn (one mic, one listener), as it did before the handshake existed.
    let mut second = phone.connect();
    prove_tcp(&mut second, &key, Channel::Mic, PATIENCE).unwrap();
    second.set_read_timeout(Some(Duration::from_millis(700))).unwrap();
    let e = second.read(&mut stream).unwrap_err();
    assert!(matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut), "{e}");
    drop(first);
    second.set_read_timeout(Some(PATIENCE)).unwrap();
    second.read_exact(&mut stream).unwrap();
    assert_eq!(stream, STREAM);
}

#[test]
fn nothing_the_phone_logs_contains_the_key() {
    let key = Key::generate().unwrap();
    let phone = Phone::start(&key, "mic", 1000);
    let mut s = phone.connect();
    prove_tcp(&mut s, &key, Channel::Mic, PATIENCE).unwrap();
    let mut s = phone.connect();
    let _ = prove_tcp(&mut s, &Key::generate().unwrap(), Channel::Mic, PATIENCE);
    let _ = until_closed(&mut phone.connect());
    assert!(phone.logged("refused"), "{}", phone.log());
    let (log, hex) = (phone.log().to_lowercase(), hex_of(&key));
    for part in [&hex[..], &hex[..16], &hex[48..]] {
        assert!(!log.contains(part), "the phone's log repeats key material");
    }
}

#[test]
fn a_phone_server_started_without_a_key_serves_nobody() {
    for input in ["", "\n", "not a key\n", "0123456789abcdef\n"] {
        let mut child = gate_test()
            .args(["serve", "mic", "1000"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap(); // and close it
        let done = child.wait_with_output().unwrap();
        assert!(!done.status.success(), "it ran with {input:?} for a key");
        let said = format!("{}{}", String::from_utf8_lossy(&done.stdout), String::from_utf8_lossy(&done.stderr));
        assert!(!said.contains("port "), "it listened with {input:?} for a key");
        assert!(said.contains("no session key on standard input"), "{said}");
        assert!(input.trim().len() < 8 || !said.contains(input.trim()), "the error repeats what it read");
    }
}
