// Compiled by the lifecycle integration test into a real child process. It implements
// the local HTTP listener contract, not Hysteria's wire protocol.
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::Duration;

fn main() {
    let path = PathBuf::from(std::env::args().last().unwrap());
    let config = std::fs::read_to_string(&path).unwrap();
    let addr = config
        .split("\"listen\":\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let starts = path.with_extension("starts");
    let count = std::fs::read_to_string(&starts)
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0)
        + 1;
    std::fs::write(starts, count.to_string()).unwrap();
    if count > 1 {
        let delay = std::fs::read_to_string(path.with_extension("delay"))
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        std::thread::sleep(Duration::from_millis(delay));
    }
    let listener = TcpListener::bind(addr).unwrap();
    std::fs::write(path.with_extension("ready"), count.to_string()).unwrap();
    for socket in listener.incoming() {
        let mut socket = socket.unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut buf = [0; 8192];
        let Ok(length) = socket.read(&mut buf) else {
            continue;
        };
        if length == 0 {
            continue;
        }
        let request = String::from_utf8_lossy(&buf[..length]);
        let _ = socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
        if request.contains("/crash ") {
            return;
        }
    }
}
