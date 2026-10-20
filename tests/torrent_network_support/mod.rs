//! Synthetic revocation after a delayed successful read, for caller fences.
#![allow(dead_code)]
use mynou::{
    Result,
    net::{HttpClient, HttpConnector, NetworkDatagram, NetworkStream},
    torrent::transport::{Direct, Network, NetworkListener, Publication},
};
use std::{
    io::{self, Read, Write},
    net::{SocketAddr, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Default)]
pub struct Revocable {
    pub revoked: AtomicBool,
    pub generation: AtomicU64,
    armed: AtomicBool,
    entered: AtomicBool,
    released: AtomicBool,
}
impl Revocable {
    pub fn arm(&self) {
        self.armed.store(true, Ordering::Release);
    }
    pub fn wait(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.entered.load(Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "metadata read never reached gate"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }
    pub fn revoke_and_release(&self) {
        self.revoked.store(true, Ordering::Release);
        self.released.store(true, Ordering::Release);
    }
}
struct Stream {
    inner: Box<dyn NetworkStream>,
    gate: Arc<Revocable>,
}
impl Read for Stream {
    fn read(&mut self, data: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(data)?;
        if n > 0 && self.gate.armed.swap(false, Ordering::AcqRel) {
            self.gate.entered.store(true, Ordering::Release);
            let deadline = Instant::now() + Duration::from_secs(10);
            while !self.gate.released.load(Ordering::Acquire) {
                assert!(Instant::now() < deadline, "metadata gate was not released");
                thread::sleep(Duration::from_millis(2));
            }
        }
        Ok(n)
    }
}
impl Write for Stream {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.inner.write(data)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
impl NetworkStream for Stream {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.inner.set_read_timeout(timeout)
    }
    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.inner.set_write_timeout(timeout)
    }
}
struct Handle(Arc<Revocable>);
impl HttpConnector for Handle {
    fn connect(&self, host: &str, port: u16, deadline: Instant) -> Result<Box<dyn NetworkStream>> {
        let addresses = Direct.resolve(&format!("{host}:{port}"), deadline)?;
        let stream = TcpStream::connect_timeout(
            &addresses[0],
            deadline.saturating_duration_since(Instant::now()),
        )
        .map_err(|_| "fixture connect")?;
        Ok(Box::new(Stream {
            inner: Box::new(stream),
            gate: self.0.clone(),
        }))
    }
}
impl Network for Handle {
    fn generation(&self) -> Result<Option<String>> {
        self.publication()?;
        Ok(Some(self.0.generation.load(Ordering::Acquire).to_string()))
    }

    fn publication(&self) -> Result<Box<dyn Publication + '_>> {
        if self.0.revoked.load(Ordering::Acquire) {
            return Err("Synthetic torrent generation revoked".into());
        }
        Ok(Box::new(()))
    }
    fn connect(&self, address: SocketAddr, timeout: Duration) -> Result<Box<dyn NetworkStream>> {
        Ok(Box::new(Stream {
            inner: Direct.connect(address, timeout)?,
            gate: self.0.clone(),
        }))
    }
    fn resolve(&self, authority: &str, deadline: Instant) -> Result<Vec<SocketAddr>> {
        Direct.resolve(authority, deadline)
    }
    fn datagram(&self, ipv6: bool) -> Result<Box<dyn NetworkDatagram>> {
        Direct.datagram(ipv6)
    }
    fn listen(&self, address: SocketAddr) -> Result<Box<dyn NetworkListener>> {
        Direct.listen(address)
    }
    fn http(&self) -> HttpClient {
        HttpClient::new().with_connector(Arc::new(Handle(self.0.clone())))
    }
}
pub fn network(gate: &Arc<Revocable>) -> Arc<dyn Network> {
    Arc::new(Handle(gate.clone()))
}
