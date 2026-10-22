//! Reviewed ordinary discovery boundary. Never called by protected sources.
use crate::{
    Result,
    net::{DeadlineStream, HttpClient},
};
use std::{
    net::{Shutdown, TcpStream, ToSocketAddrs},
    time::Instant,
};
pub(crate) fn http() -> HttpClient {
    HttpClient::new()
}
pub(crate) struct Irc {
    socket: TcpStream,
}
impl Irc {
    pub(crate) fn shutdown(&self) {
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}
pub(crate) fn irc(host: &str, port: u16, deadline: Instant) -> Result<(DeadlineStream, Irc)> {
    for address in (host, port)
        .to_socket_addrs()
        .map_err(|_| "IRC: hostname lookup failed")?
        .take(8)
    {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or("IRC: connection deadline exceeded")?;
        if let Ok(socket) = TcpStream::connect_timeout(&address, remaining) {
            socket
                .set_nodelay(true)
                .map_err(|_| "IRC: socket configuration failed")?;
            let handle = Irc {
                socket: socket
                    .try_clone()
                    .map_err(|_| "IRC: shutdown handle unavailable")?,
            };
            return Ok((DeadlineStream::new(socket, deadline), handle));
        }
    }
    Err("IRC: connection failed".into())
}
