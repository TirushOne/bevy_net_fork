use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::io;
use std::io::{ErrorKind, IoSliceMut};
use std::net::{SocketAddr, UdpSocket};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;
use quinn::udp::{RecvMeta, Transmit, UdpSocketState, UdpSockRef};

use static_init::dynamic;
use bevy_tasks::IoTaskPool;

pub use quinn::*;

/// A QUIC endpoint.
///
/// An endpoint corresponds to a single UDP socket, may host many connections, and may act as both
/// client and server for different connections.
///
/// May be cloned to obtain another handle to the same endpoint.
///
/// This type is a conviniant wrapper around [Endpoint].
#[derive(Debug, Clone)]
pub struct EndPoint(Endpoint);

// A couple of endpoint methods aren't reimplemented due to the relevant types
// in quinn not being reexported. A pr with a fix (https://github.com/quinn-rs/quinn/pull/1920#event-13538285399)
// has been merged so whenever the next update for quinn comes out we can implement the relevant methods.
impl EndPoint {

    /// Construct an endpoint with arbitrary configuration and socket
    pub fn new(
        config: EndpointConfig,
        server_config: Option<ServerConfig>,
        socket: UdpSocket
    ) -> io::Result<Self> {
        Ok(Self(Endpoint::new(config, server_config, socket, RUNTIME.clone())?))
    }

    /// Helper to construct an endpoint for use with both incoming and outgoing connections
    ///
    /// Platform defaults for dual-stack sockets vary. For example, any socket bound to a wildcard
    /// IPv6 address on Windows will not by default be able to communicate with IPv4
    /// addresses. Portable applications should bind an address that matches the family they wish to
    /// communicate within.
    pub fn server(config: ServerConfig, addr: SocketAddr) -> io::Result<Self> {
        Ok(
            Self::new(
                EndpointConfig::default(),
                Some(config),
                UdpSocket::bind(addr)?
            )?
        )
    }

    /// Helper to construct an endpoint for use with outgoing connections only
    ///
    /// Note that `addr` is the *local* address to bind to, which should usually be a wildcard
    /// address like `0.0.0.0:0` or `[::]:0`, which allow communication with any reachable IPv4 or
    /// IPv6 address respectively from an OS-assigned port.
    ///
    /// Platform defaults for dual-stack sockets vary. For example, any socket bound to a wildcard
    /// IPv6 address on Windows will not by default be able to communicate with IPv4
    /// addresses. Portable applications should bind an address that matches the family they wish to
    /// communicate within.
    pub fn client(addr: SocketAddr) -> io::Result<Self> {
        Ok(
            Self::new(
                EndpointConfig::default(),
                None,
                UdpSocket::bind(addr)?
            )?
        )
    }

    /// Get the next incoming connection attempt from a client
    ///
    /// Yields [`Incoming`](quinn::Incoming)s, or `None` if the endpoint is [`close`](Self::close)d. [`Incoming`]
    /// can be `await`ed to obtain the final [`Connection`](quinn::Connection), or used to e.g.
    /// filter connection attempts or force address validation, or converted into an intermediate
    /// `Connecting` future which can be used to e.g. send 0.5-RTT data.

    pub fn accept(&self) -> Accept<'_> {
        self.0.accept()
    }

    /// Set the client configuration used by [`connect`](Self::connect)
    pub fn set_default_client_config(&mut self, config: ClientConfig) {
        self.0.set_default_client_config(config);
    }

    /// Connect to a remote endpoint
    ///
    /// `server_name` must be covered by the certificate presented by the server. This prevents a
    /// connection from being intercepted by an attacker with a valid certificate for some other
    /// server.
    ///
    /// May fail immediately due to configuration errors, or in the future if the connection could
    /// not be established.
    pub fn connect(
        &self,
        addr: SocketAddr,
        server_name: &str
    ) -> Result<Connecting, ConnectError> {
        self.0.connect(addr, server_name)
    }


    /// Connect to a remote endpoint using a custom configuration.
    ///
    /// See [`connect()`] for details.
    ///
    /// [`connect()`]: Self::connect
    pub fn connect_with(
        &self,
        config: ClientConfig,
        addr: SocketAddr,
        server_name: &str
    ) -> Result<Connecting, ConnectError> {
        self.0.connect_with(config, addr, server_name)
    }

    /// Switch to a new UDP socket
    ///
    /// Allows the endpoint's address to be updated live, affecting all active connections. Incoming
    /// connections and connections to servers unreachable from the new address will be lost.
    ///
    /// On error, the old UDP socket is retained.
    pub fn rebind(&self, socket: UdpSocket) -> io::Result<()> {
        self.0.rebind(socket)
    }

    /// Replace the server configuration, affecting new incoming connections only
    ///
    /// Useful for e.g. refreshing TLS certificates without disrupting existing connections.
    pub fn set_server_config(&self, server_config: Option<ServerConfig>) {
        self.0.set_server_config(server_config)
    }

    /// Get the local `SocketAddr` the underlying socket is bound to
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.0.local_addr()
    }

    /// Get the number of connections that are currently open
    pub fn open_connections(&self) -> usize {
        self.0.open_connections()
    }

    /// Close all of this endpoint's connections immediately and cease accepting new connections.
    ///
    /// See [`Connection::close()`](quinn::Connection) for details.
    ///
    /// [`Connection::close()`](quinn::Connection)
    pub fn close(&self, error_code: VarInt, reason: &[u8]) {
        self.0.close(error_code, reason)
    }

    /// Wait for all connections on the endpoint to be cleanly shut down
    ///
    /// Waiting for this condition before exiting ensures that a good-faith effort is made to notify
    /// peers of recent connection closes, whereas exiting immediately could force them to wait out
    /// the idle timeout period.
    ///
    /// Does not proactively close existing connections or cause incoming connections to be
    /// rejected. Consider calling [`close()`] if that is desired.
    ///
    /// [`close()`]: Self::close
    pub async fn wait_idle(&self) {
        self.0.wait_idle().await
    }
}

#[derive(Debug)]
struct BevyQuinnRuntime;

#[dynamic]
static RUNTIME: Arc<BevyQuinnRuntime> = Arc::new(BevyQuinnRuntime);

impl Runtime for BevyQuinnRuntime {
    fn new_timer(&self, i: Instant) -> Pin<Box<dyn AsyncTimer>> {
        Box::pin(IoTimer::new(i))
    }

    fn spawn(&self, future: Pin<Box<dyn Future<Output=()> + Send>>) {
        IoTaskPool::get().spawn(future).detach();
    }

    fn wrap_udp_socket(&self, t: UdpSocket) -> io::Result<Arc<dyn AsyncUdpSocket>> {
        Ok(Arc::new(QuinnUdp::new(t)?))
    }
}

struct QuinnUdp {
    state: UdpSocketState,
    socket: UdpSocket
}

impl Debug for QuinnUdp {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        self.state.fmt(f)
    }
}

impl QuinnUdp {
    fn new(socket: UdpSocket) -> Result<QuinnUdp, io::Error> {
        Ok(Self {
            state: UdpSocketState::new(UdpSockRef::from(&socket))?,
            socket: socket
        })
    }
}

#[derive(Debug)]
struct QuinnPoller(bool);

impl UdpPoller for QuinnPoller {
    //todo: create a more efficient implementation
    fn poll_writable(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        if self.0 {
            return Poll::Ready(Ok(()))
        }

        self.0 = true;

        let waker = cx.waker().clone();

        IoTaskPool::get().spawn(async move {
            waker.wake()
        }).detach();
        Poll::Pending
    }
}

impl AsyncUdpSocket for QuinnUdp {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(QuinnPoller(false))
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        self.state.send(UdpSockRef::from(&self.socket), transmit)
    }

    fn poll_recv(&self, cx: &mut Context, bufs: &mut [IoSliceMut<'_>], meta: &mut [RecvMeta]) -> Poll<io::Result<usize>> {
        match self.state.recv(UdpSockRef::from(&self.socket), bufs, meta) {
            Ok(n) => {
                Poll::Ready(Ok(n))
            }
            Err(error) => {
                match error.kind() {
                    ErrorKind::WouldBlock => {
                        let waker = cx.waker().clone();

                        IoTaskPool::get().spawn(async move {
                            waker.wake()
                        }).detach();

                        Poll::Pending
                    },
                    _ => {
                        Poll::Ready(Err(error))
                    }
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

#[derive(Debug, Copy, Clone)]
pub struct IoTimer {
    expiry: Instant,
}

impl IoTimer {
    pub fn new(expiry: Instant) -> Self {
        Self {
            expiry
        }
    }

    pub fn reset(&mut self, new_expiry: Instant) -> &mut Self {
        self.expiry = new_expiry;
        self
    }

    pub fn expires(&self) -> Instant {
        self.expiry
    }
}

impl AsyncTimer for IoTimer {
    fn reset(mut self: Pin<&mut Self>, i: Instant) {
        self.expiry = i;
    }

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<()> {
        <Self as Future>::poll(self, cx)
    }
}

impl Future for IoTimer {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let now = Instant::now();

        if now >= self.expiry {
            return Poll::Ready(())
        }
        let waker = cx.waker().clone();
        IoTaskPool::get().spawn(async move {
            waker.wake()
        }).detach();
        Poll::Pending
    }
}
