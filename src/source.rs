use std::collections::HashMap;
use std::fmt::{self, Debug};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ringbuf::{RingBuffer, Producer, Consumer};

use crate::engine::Sample;

#[derive(Clone)]
pub struct Registry {
    inner: Arc<Mutex<RegistryInner>>,
}

struct RegistryInner {
    channels: HashMap<String, Source>,
}

impl Registry {
    pub fn new() -> Self {
        let inner = RegistryInner {
            channels: HashMap::new(),
        };

        Registry { inner: Arc::new(Mutex::new(inner)) }
    }
}

pub struct Source {
    shared: Arc<SourceShared>,
    tx: Option<Producer<Sample>>,
}

#[derive(Debug)]
pub struct SourceShared {
    channel_name: String,
    recv_online: AtomicBool,
}

#[derive(Debug)]
pub enum ListenError {
    AlreadyInUse,
}

#[derive(Debug)]
pub enum ConnectError {
    NoMountpoint,
    AlreadyConnected,
}

impl Registry {
    pub fn listen(&self, channel_name: &str) -> Result<SourceRecv, ListenError> {
        let mut registry = self.inner.lock()
            .expect("registry lock");

        if registry.channels.contains_key(channel_name) {
            return Err(ListenError::AlreadyInUse);
        }

        let (tx, rx) = RingBuffer::<Sample>::new(65536).split();

        let shared = Arc::new(SourceShared {
            channel_name: channel_name.to_owned(),
            recv_online: AtomicBool::new(true),
        });

        let recv = SourceRecv {
            registry: self.clone(),
            shared: shared.clone(),
            rx,
        };

        let source = Source {
            shared: shared.clone(),
            tx: Some(tx),
        };

        registry.channels.insert(channel_name.to_owned(), source);

        Ok(recv)
    }

    pub fn connect(&self, channel_name: &str) -> Result<SourceSend, ConnectError> {
        let mut registry = self.inner.lock()
            .expect("registry lock");

        let source = match registry.channels.get_mut(channel_name) {
            None => { return Err(ConnectError::NoMountpoint); }
            Some(source) => source,
        };

        let tx = match source.tx.take() {
            None => { return Err(ConnectError::AlreadyConnected); }
            Some(tx) => tx,
        };

        Ok(SourceSend {
            registry: self.clone(),
            shared: source.shared.clone(),
            tx: Some(tx),
        })
    }
}


pub struct SourceSend {
    registry: Registry,
    shared: Arc<SourceShared>,
    // this is, regrettably, an Option because we need to take the producer
    // and put it back in the mountpoints table on drop:
    tx: Option<Producer<Sample>>,
}

impl SourceSend {
    pub fn connected(&self) -> bool {
        self.shared.recv_online.load(Ordering::Relaxed)
    }

    pub fn write(&mut self, data: &[Sample]) -> Result<usize, ()> {
        if self.connected() {
            if let Some(tx) = &mut self.tx {
                Ok(tx.push_slice(data))
            } else {
                // tx is always Some for a valid (non-dropped) SourceSend
                unreachable!()
            }
        } else {
            Err(())
        }
    }
}

impl Debug for SourceSend {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "SourceSend {{ shared: {:?}, .. }}", self.shared)
    }
}

impl Drop for SourceSend {
    fn drop(&mut self) {
        let mut registry = self.registry.inner.lock()
            .expect("registry lock");

        match registry.channels.get_mut(&self.shared.channel_name) {
            None => {
                // receiver has disconnected, there is nothing to do
            }
            Some(channel) => {
                channel.tx = self.tx.take();
            }
        }
    }
}

pub struct SourceRecv {
    registry: Registry,
    shared: Arc<SourceShared>,
    rx: Consumer<Sample>,
}

impl Debug for SourceRecv {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "SourceRecv {{ shared: {:?}, .. }}", self.shared)
    }
}

impl SourceRecv {
    pub fn channel_name(&self) -> &str {
        &self.shared.channel_name
    }

    pub fn read(&mut self, data: &mut [Sample]) -> usize {
        self.rx.pop_slice(data)
    }
}

impl Drop for SourceRecv {
    fn drop(&mut self) {
        self.registry.inner.lock()
            .expect("registry lock")
            .channels
            .remove(&self.shared.channel_name);

        self.shared.recv_online.store(false, Ordering::Relaxed);
    }
}