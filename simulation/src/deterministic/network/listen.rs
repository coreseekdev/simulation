use super::{FaultyTcpStream, SocketHalf};
use crate::TcpStream;
use async_trait::async_trait;
use futures::{channel::mpsc, Poll, Stream, StreamExt};
use std::{fmt, io, net, pin::Pin, task::Context};
use tracing::trace;

#[derive(Debug)]
/// ListenerState represents both the bound and unbound state of a Listener.
/// This allows supporting late binding of Listeners to sockets.
pub(crate) enum ListenerState {
    Unbound {
        tx: mpsc::Sender<FaultyTcpStream<SocketHalf>>,
        rx: mpsc::Receiver<FaultyTcpStream<SocketHalf>>,
    },
    Bound {
        tx: mpsc::Sender<FaultyTcpStream<SocketHalf>>,
    },
}

pub struct Listener {
    local_addr: net::SocketAddr,
    incoming: mpsc::Receiver<FaultyTcpStream<SocketHalf>>,
}

impl fmt::Debug for Listener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let local_addr = self.local_addr;
        write!(f, "Listener {{ local_addr: {} }}", local_addr)
    }
}

impl Listener {
    pub fn new(
        local_addr: net::SocketAddr,
        incoming: mpsc::Receiver<FaultyTcpStream<SocketHalf>>,
    ) -> Self {
        Self {
            local_addr,
            incoming,
        }
    }
}

impl Listener {
    // inner function for now, remove when tracing support async_trait.
    #[tracing_attributes::instrument]
    async fn accept(
        &mut self,
    ) -> Result<(FaultyTcpStream<SocketHalf>, net::SocketAddr), io::Error> {
        if let Some(next) = self.incoming.next().await {
            let addr = next.peer_addr()?;
            trace!("accepted new connection from {}", addr);
            Ok((next, addr))
        } else {
            trace!("listener no longer connected");
            Err(io::ErrorKind::NotConnected.into())
        }
    }
}

struct ListenerStream {
    incoming: mpsc::Receiver<FaultyTcpStream<SocketHalf>>,
}

impl Stream for ListenerStream {
    type Item = Result<FaultyTcpStream<SocketHalf>, io::Error>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match futures::ready!(self.incoming.poll_next_unpin(cx)) {
            Some(item) => Poll::Ready(Some(Ok(item))),
            None => Poll::Ready(None),
        }
    }
}

#[async_trait]
impl crate::TcpListener for Listener {
    type Stream = FaultyTcpStream<SocketHalf>;
    async fn accept(&mut self) -> Result<(Self::Stream, net::SocketAddr), io::Error> {
        Listener::accept(self).await
    }
    fn local_addr(&self) -> Result<net::SocketAddr, io::Error> {
        Ok(self.local_addr)
    }
    fn ttl(&self) -> io::Result<u32> {
        Ok(0)
    }
    fn set_ttl(&self, _: u32) -> io::Result<()> {
        Ok(())
    }
    fn into_stream(self) -> Pin<Box<dyn Stream<Item = Result<Self::Stream, io::Error>> + Send>> {
        let Listener { incoming, .. } = self;
        Box::pin(ListenerStream { incoming })
    }
}
