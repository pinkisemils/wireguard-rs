use super::router;
use super::timers::{Events, Timers};
use super::HandshakeJob;

use super::tun::Tun;
use super::udp::UDP;
use super::wireguard::WireguardInner;

use std::fmt;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use spin::{Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crossbeam_channel::Sender;
use x25519_dalek::PublicKey;

pub struct Peer<T: Tun, B: UDP> {
    pub router: Arc<router::PeerHandle<B::Endpoint, Events<T, B>, T::Writer, B::Writer>>,
    pub state: Arc<PeerInner<T, B>>,
}

pub struct PeerInner<T: Tun, B: UDP> {
    // internal id (for logging)
    pub id: u64,

    // wireguard device state
    pub wg: Arc<WireguardInner<T, B>>,

    // handshake state
    pub walltime_last_handshake: Mutex<Option<SystemTime>>,
    pub last_handshake_sent: Mutex<Instant>, // instant for last handshake
    pub handshake_queued: AtomicBool,        // is a handshake job currently queued for the peer?
    pub queue: Mutex<Sender<HandshakeJob<B::Endpoint>>>, // handshake queue

    // stats and configuration
    pub pk: PublicKey,       // public key, DISCUSS: avoid this. TODO: remove
    pub rx_bytes: AtomicU64, // received bytes
    pub tx_bytes: AtomicU64, // transmitted bytes

    // timer model
    pub timers: RwLock<Timers>,
}

impl<T: Tun, B: UDP> Clone for Peer<T, B> {
    fn clone(&self) -> Peer<T, B> {
        Peer {
            router: self.router.clone(),
            state: self.state.clone(),
        }
    }
}

impl<T: Tun, B: UDP> PeerInner<T, B> {
    #[inline(always)]
    pub fn timers(&self) -> RwLockReadGuard<Timers> {
        self.timers.read()
    }

    #[inline(always)]
    pub fn timers_mut(&self) -> RwLockWriteGuard<Timers> {
        self.timers.write()
    }
}

impl<T: Tun, B: UDP> fmt::Display for Peer<T, B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "peer(id = {})", self.id)
    }
}

impl<T: Tun, B: UDP> Deref for Peer<T, B> {
    type Target = PeerInner<T, B>;
    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

impl<T: Tun, B: UDP> Peer<T, B> {
    /// Bring the peer down. Causing:
    ///
    /// - Timers to be stopped and disabled.
    /// - All keystate to be zeroed
    pub fn down(&self) {
        self.stop_timers();
        self.router.down();
    }

    /// Bring the peer up.
    pub fn up(&self) {
        self.router.up();
        self.start_timers();
    }
}