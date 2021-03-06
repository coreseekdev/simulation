//! Determinstic scheduling, IO and fault injection for Tokio
//!
//! The goal of this crate is to provide FoundationDB style simulation
//! testing for all.
//!
//! There are 3 layers on which the `DeterministicRuntime` is built.
//!
//! - `DeterministicRandom` allows for accessing a deterministic source of randomness.
//! - `DeterministicTime` provides a deterministic time source.
//! - `DeterministicNetwork` provides a process wide networking in memory networking implementation.
//!
//! `DeterministicRuntime` uses these to support deterministic task scheduling and fault injection.
use crate::Error;
use async_trait::async_trait;
use futures::Future;
use std::{
    io, net,
    time::{Duration, Instant},
};

mod network;
mod random;
mod time;
pub(crate) use network::{DeterministicNetwork, DeterministicNetworkHandle};
pub use network::{Listener, Socket};
pub(crate) use random::{DeterministicRandom, DeterministicRandomHandle};
pub(crate) use time::{DeterministicTime, DeterministicTimeHandle};
use tokio_net::driver;

#[derive(Debug, Clone)]
pub struct DeterministicRuntimeHandle {
    time_handle: time::DeterministicTimeHandle,
    network_handle: DeterministicNetworkHandle,
    executor_handle: tokio_executor::current_thread::Handle,
    random_handle: DeterministicRandomHandle,
}

impl DeterministicRuntimeHandle {
    pub fn now(&self) -> Instant {
        self.time_handle.now()
    }
    pub fn time_handle(&self) -> time::DeterministicTimeHandle {
        self.time_handle.clone()
    }
    pub fn random_handle(&self) -> DeterministicRandomHandle {
        self.random_handle.clone()
    }
}

#[async_trait]
impl crate::Environment for DeterministicRuntimeHandle {
    type TcpStream = network::Socket;
    type TcpListener = network::Listener;
    fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.executor_handle.spawn(future).expect("failed to spawn");
    }
    fn now(&self) -> Instant {
        self.time_handle.now()
    }
    fn delay(&self, deadline: Instant) -> tokio_timer::Delay {
        self.time_handle.delay(deadline)
    }
    fn timeout<T>(&self, value: T, timeout: Duration) -> tokio_timer::Timeout<T> {
        self.time_handle.timeout(value, timeout)
    }
    async fn bind<A>(&self, addr: A) -> io::Result<Self::TcpListener>
    where
        A: Into<net::SocketAddr> + Send + Sync,
    {
        self.network_handle.bind(addr.into()).await
    }
    async fn connect<A>(&self, addr: A) -> io::Result<Self::TcpStream>
    where
        A: Into<net::SocketAddr> + Send + Sync,
    {
        self.network_handle.connect(addr.into()).await
    }
}

type Executor = tokio_executor::current_thread::CurrentThread<DeterministicTime<driver::Reactor>>;

pub struct DeterministicRuntime {
    executor: Executor,
    time_handle: DeterministicTimeHandle,
    network: DeterministicNetwork,
    random: DeterministicRandom,
}

impl DeterministicRuntime {
    pub fn new() -> Result<Self, Error> {
        DeterministicRuntime::new_with_seed(0)
    }
    pub fn new_with_seed(seed: u64) -> Result<Self, Error> {
        let reactor = driver::Reactor::new().map_err(|source| Error::RuntimeBuild { source })?;

        let time = DeterministicTime::new_with_park(reactor);
        let time_handle = time.handle();
        let network = DeterministicNetwork::new(time_handle.clone());
        let executor = tokio_executor::current_thread::CurrentThread::new_with_park(time);
        let random = DeterministicRandom::new_with_seed(seed);
        Ok(DeterministicRuntime {
            executor,
            time_handle,
            network,
            random,
        })
    }

    pub fn handle(&self, addr: net::IpAddr) -> DeterministicRuntimeHandle {
        DeterministicRuntimeHandle {
            time_handle: self.time_handle.clone(),
            network_handle: self.network.scoped(addr),
            executor_handle: self.executor.handle(),
            random_handle: self.random.handle(),
        }
    }

    pub fn latency_fault(&self) -> network::fault::LatencyFaultInjector {
        let network_inner = self.network.clone_inner();
        network::fault::LatencyFaultInjector::new(
            network_inner,
            self.random.handle(),
            self.time_handle.clone(),
        )
    }

    pub fn localhost_handle(&self) -> DeterministicRuntimeHandle {
        self.handle(net::IpAddr::V4(net::Ipv4Addr::LOCALHOST))
    }

    pub fn spawn<F>(&mut self, future: F) -> &mut Self
    where
        F: Future<Output = ()> + 'static,
    {
        self.executor.spawn(future);
        self
    }

    pub fn run(&mut self) -> Result<(), Error> {
        self.enter(|executor| executor.run())
            .map_err(|source| Error::CurrentThreadRun { source })
    }

    pub fn block_on<F>(&mut self, f: F) -> F::Output
    where
        F: Future,
    {
        self.enter(|executor| executor.block_on(f))
    }

    fn enter<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut Executor) -> R,
    {
        let DeterministicRuntime {
            ref mut time_handle,
            ref mut executor,
            ..
        } = *self;
        // Setup mock clock globals
        let clock = tokio_timer::clock::Clock::new_with_now(time_handle.clone_now());
        let timer_handle = time_handle.clone_timer_handle();
        let _guard = tokio_timer::timer::set_default(&timer_handle);
        tokio_timer::clock::with_default(&clock, || {
            let mut default_executor = tokio_executor::current_thread::TaskExecutor::current();
            tokio_executor::with_default(&mut default_executor, || f(executor))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Environment;

    #[test]
    /// Test that delays accurately advance the clock.
    fn delays() {
        let mut runtime = DeterministicRuntime::new().unwrap();
        let handle = runtime.localhost_handle();
        runtime.block_on(async {
            let start_time = handle.now();
            handle.delay_from(Duration::from_secs(30)).await;
            let end_time = handle.now();
            assert!(end_time > start_time);
            assert_eq!(end_time - Duration::from_secs(30), start_time)
        });
    }

    #[test]
    /// Test that waiting on delays across spawned tasks results in the clock
    /// being advanced in accordance with the length of the delay.
    fn ordering() {
        let mut runtime = DeterministicRuntime::new().unwrap();
        let handle = runtime.localhost_handle();
        runtime.block_on(async {
            let delay1 = handle.delay_from(Duration::from_secs(10));
            let delay2 = handle.delay_from(Duration::from_secs(30));

            let handle1 = handle.clone();
            let completed_at1 = crate::spawn_with_result(&handle1.clone(), async move {
                delay1.await;
                handle1.now()
            })
            .await;

            let handle2 = handle.clone();
            let completed_at2 = crate::spawn_with_result(&handle2.clone(), async move {
                delay2.await;
                handle2.now()
            })
            .await;
            assert!(completed_at1 < completed_at2)
        });
    }

    #[test]
    /// Test that the Tokio global timer and clock are both set correctly.
    fn globals() {
        let mut runtime = DeterministicRuntime::new().unwrap();
        let handle = runtime.localhost_handle();
        runtime.block_on(async {
            let start_time = tokio_timer::clock::now();
            assert_eq!(
                handle.now(),
                tokio_timer::clock::now(),
                "expected start time to be equal"
            );
            let delay_duration = Duration::from_secs(1);
            let delay = tokio::timer::delay_for(delay_duration);
            delay.await;
            assert_eq!(
                start_time + delay_duration,
                tokio_timer::clock::now(),
                "expected elapsed time to be equal"
            );
        });
    }
}
