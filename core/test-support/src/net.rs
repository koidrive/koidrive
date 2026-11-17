// SPDX-License-Identifier: Apache-2.0

use std::io::Read;
use std::net::{SocketAddr, TcpListener};
use std::sync::OnceLock;
use std::time::Duration;

use socket2::Socket;

pub fn reset_tcp_address() -> SocketAddr {
    static ADDRESS: OnceLock<SocketAddr> = OnceLock::new();
    *ADDRESS.get_or_init(|| {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::Builder::new()
            .name("test-reset-server".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    let mut stream = stream.unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let _ = stream.read(&mut [0]);
                    let socket = Socket::from(stream);
                    socket.set_linger(Some(Duration::ZERO)).unwrap();
                }
            })
            .unwrap();
        address
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Write};
    use std::net::TcpStream;

    #[test]
    fn reset_tcp_ports_fail_connections_without_releasing_their_addresses() {
        let threads: Vec<_> = (0..32)
            .map(|_| {
                std::thread::spawn(|| {
                    let address = reset_tcp_address();
                    assert_eq!(
                        TcpListener::bind(address).unwrap_err().kind(),
                        io::ErrorKind::AddrInUse
                    );
                    let mut stream =
                        TcpStream::connect_timeout(&address, Duration::from_secs(5)).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    stream.write_all(b"x").unwrap();
                    assert_eq!(
                        stream.read(&mut [0]).unwrap_err().kind(),
                        io::ErrorKind::ConnectionReset
                    );
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
    }
}
