use super::{SharedState, SharedPeer, UtunPacket, trace_packet};
use consts::{REKEY_AFTER_TIME, KEEPALIVE_TIMEOUT};
use protocol::Session;

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use base64;
use byteorder::{ByteOrder, BigEndian, LittleEndian};
use futures::{self, Async, Future, Stream, Sink, Poll, future, unsync, sync, stream};
use pnet::packet::ipv4::Ipv4Packet;
use pnet::packet::ipv6::Ipv6Packet;
use pnet::packet::ethernet::{EtherTypes, EthernetPacket};
use socket2::{Socket, Domain, Type, SockAddr, Protocol};
use snow::{self, NoiseBuilder};
use tokio_core::net::{UdpSocket, UdpCodec, UdpFramed};
use tokio_core::reactor::Handle;
use tokio_io::codec::Framed;
use tokio_timer::{Interval, Timer};
use treebitmap::{IpLookupTable, IpLookupTableOps};


pub type PeerServerMessage = (SocketAddr, Vec<u8>);
struct VecUdpCodec;
impl UdpCodec for VecUdpCodec {
    type In = PeerServerMessage;
    type Out = PeerServerMessage;

    fn decode(&mut self, src: &SocketAddr, buf: &[u8]) -> io::Result<Self::In> {
        let unmapped_ip = match src.ip() {
            IpAddr::V6(v6addr) => {
                if let Some(v4addr) = v6addr.to_ipv4() {
                    IpAddr::V4(v4addr)
                } else {
                    IpAddr::V6(v6addr)
                }
            }
            v4addr => v4addr
        };
        Ok((SocketAddr::new(unmapped_ip, src.port()), buf.to_vec()))
    }

    fn encode(&mut self, msg: Self::Out, buf: &mut Vec<u8>) -> SocketAddr {
        let (mut addr, mut data) = msg;
        buf.append(&mut data);
        let mapped_ip = match addr.ip() {
            IpAddr::V4(v4addr) => IpAddr::V6(v4addr.to_ipv6_mapped()),
            v6addr => v6addr.clone()
        };
        addr.set_ip(mapped_ip);
        addr
    }
}

#[derive(Debug)]
pub enum TimerMessage {
    KeepAlive(SharedPeer, u32),
    Rekey(SharedPeer, u32),
}

pub struct PeerServer {
    handle: Handle,
    shared_state: SharedState,
    timer: Timer,
    udp_stream: stream::SplitStream<UdpFramed<VecUdpCodec>>,
    outgoing_tx: unsync::mpsc::Sender<UtunPacket>,
    outgoing_rx: futures::stream::Peekable<unsync::mpsc::Receiver<UtunPacket>>,
    timer_tx: unsync::mpsc::Sender<TimerMessage>,
    timer_rx: unsync::mpsc::Receiver<TimerMessage>,
    udp_tx: unsync::mpsc::Sender<(SocketAddr, Vec<u8>)>,
    tunnel_tx: unsync::mpsc::Sender<UtunPacket>,
}

impl PeerServer {
    pub fn bind(handle: Handle, shared_state: SharedState, tunnel_tx: unsync::mpsc::Sender<UtunPacket>) -> Self {
        let socket = Socket::new(Domain::ipv6(), Type::dgram(), Some(Protocol::udp())).unwrap();
        socket.set_only_v6(false).unwrap();
        socket.set_nonblocking(true).unwrap();
        socket.bind(&SocketAddr::from((Ipv6Addr::unspecified(), 0)).into()).unwrap();
        let socket = UdpSocket::from_socket(socket.into_udp_socket(), &handle.clone()).unwrap();
        let (udp_sink, udp_stream) = socket.framed(VecUdpCodec{}).split();
        let (timer_tx, timer_rx) = unsync::mpsc::channel::<TimerMessage>(1024);
        let (udp_tx, udp_rx) = unsync::mpsc::channel::<(SocketAddr, Vec<u8>)>(1024);
        let (outgoing_tx, outgoing_rx) = unsync::mpsc::channel::<UtunPacket>(1024);
        let outgoing_rx = outgoing_rx.peekable();
        let timer = Timer::default();

        let udp_write_passthrough = udp_sink.sink_map_err(|_| ()).send_all(
            udp_rx.map(|(addr, packet)| {
                trace!("sending UDP packet to {:?}", &addr);
                (addr, packet)
            }).map_err(|_| ()))
            .then(|_| Ok(()));
        handle.spawn(udp_write_passthrough);

        PeerServer {
            handle, shared_state, timer, udp_stream, udp_tx, tunnel_tx, timer_tx, timer_rx, outgoing_tx, outgoing_rx
        }
    }

    pub fn tx(&self) -> unsync::mpsc::Sender<UtunPacket> {
        self.outgoing_tx.clone()
    }

    pub fn udp_tx(&self) -> unsync::mpsc::Sender<(SocketAddr, Vec<u8>)> {
        self.udp_tx.clone()
    }

    // TODO: create a transport packet (type 0x4) queue until a handshake has been completed
    fn handle_incoming_packet(&mut self, addr: SocketAddr, packet: Vec<u8>) {
        debug!("got a UDP packet from {:?} of length {}, packet type {}", &addr, packet.len(), packet[0]);
        let mut state = self.shared_state.borrow_mut();
        match packet[0] {
            1 => {
                let their_index = LittleEndian::read_u32(&packet[4..]);

                let mut noise = NoiseBuilder::new("Noise_IKpsk2_25519_ChaChaPoly_BLAKE2s".parse().unwrap())
                    .local_private_key(&state.interface_info.private_key.expect("no private key!"))
                    .prologue("WireGuard v1 zx2c4 Jason@zx2c4.com".as_bytes())
                    .build_responder().unwrap();

                let mut timestamp = [0u8; 116];
                if let Err(_) = noise.read_message(&packet[8..116], &mut timestamp) {
                    warn!("failed to parse incoming handshake");
                    return;
                }

                // TODO: hacked up API until it's officially supported in snow.
                let peer_ref = {
                    let their_pubkey = noise.get_remote_static().expect("should have remote static key");

                    info!("their_pubkey: {}", base64::encode(&their_pubkey[..]));
                    let peer_ref = state.pubkey_map.get(&their_pubkey[..]);
                    if peer_ref.is_none() {
                        warn!("unknown public key received");
                        return;
                    }
                    peer_ref.unwrap().clone()
                };

                let mut peer = peer_ref.borrow_mut();

                // TODO: hacked up API until it's officially supported in snow.
                match noise {
                    snow::Session::Handshake(ref mut handshake_state) => {
                        handshake_state.set_psk(2, &peer.info.psk.unwrap_or_else(|| [0u8; 32]));
                    },
                    _ => unreachable!()
                }

                peer.set_next_session(Session::with_their_index(noise, their_index));
                let _ = state.index_map.insert(peer.our_next_index().unwrap(), peer_ref.clone());

                let response_packet = peer.get_response_packet();

                self.handle.spawn(self.udp_tx.clone().send((addr.clone(), response_packet)).then(|_| Ok(())));
                let dead_session = peer.ratchet_session().unwrap();
                if let Some(session) = dead_session {
                    let _ = state.index_map.remove(&session.our_index);
                }
                info!("sent handshake response, ratcheted session.");
            },
            2 => {
                let their_index = LittleEndian::read_u32(&packet[4..]);
                let our_index = LittleEndian::read_u32(&packet[8..]);
                let peer_ref = state.index_map.get(&our_index).unwrap().clone();
                let mut peer = peer_ref.borrow_mut();
                peer.sessions.next.as_mut().unwrap().their_index = their_index;
                let payload_len = peer.next_noise().expect("pending noise session")
                    .read_message(&packet[12..60], &mut []).unwrap();
                assert!(payload_len == 0);
                peer.ratchet_session().unwrap();
                info!("got handshake response, ratcheted session.");

                // TODO neither of these timers are to spec, but are simple functional placeholders
                let rekey_timer = self.timer.sleep(Duration::from_secs(REKEY_AFTER_TIME));
                let rekey_future = rekey_timer.map_err(|_|()).and_then({
                    let timer_tx = self.timer_tx.clone();
                    let peer_ref = peer_ref.clone();
                    move |_| {
                        timer_tx.clone().send(TimerMessage::Rekey(peer_ref, our_index))
                            .then(|_| Ok(()))
                    }
                }).then(|_| Ok(()));
                self.handle.spawn(rekey_future);

                let keepalive_interval = self.timer.interval(Duration::from_secs(KEEPALIVE_TIMEOUT));
                let keepalive_future = keepalive_interval.map_err(|_|()).for_each({
                    let timer_tx = self.timer_tx.clone();
                    let peer_ref = peer_ref.clone();
                    move |_| -> Box<Future<Item = _, Error = _>> {
                        if peer_ref.borrow().our_current_index().unwrap() != our_index {
                            debug!("cancelling old keepalive_timer");
                            Box::new(future::err(()))
                        } else {
                            Box::new(timer_tx.clone().send(TimerMessage::KeepAlive(peer_ref.clone(), our_index))
                                .then(|_| Ok(())))
                        }
                    }
                });
                self.handle.spawn(keepalive_future);
            },
            3 => {
                warn!("cookie messages not yet implemented.");
            }
            4 => {
                let our_index_received = LittleEndian::read_u32(&packet[4..]);
                let nonce = LittleEndian::read_u64(&packet[8..]);

                let lookup = state.index_map.get(&our_index_received);
                if let Some(ref peer) = lookup {
                    let mut peer = peer.borrow_mut();

                    let res = peer.decrypt_transport_packet(our_index_received, nonce, &packet[16..]);

                    if let Ok(raw_packet) = res {
                        trace_packet("received TRANSPORT: ", &raw_packet);
                        let utun_packet = match (raw_packet[0] & 0xf0) >> 4 {
                            4 => UtunPacket::Inet4(raw_packet),
                            6 => UtunPacket::Inet6(raw_packet),
                            _ => unimplemented!()
                        };
                        self.handle.spawn(self.tunnel_tx.clone().send(utun_packet)
                            .then(|_| Ok(())));
                    } else {
                        warn!("dropped incoming tranport packet that neither the current nor past session could decrypt");
                    }
                }
            },
            _ => unimplemented!()
        }
    }

    fn handle_timer(&mut self, message: TimerMessage) {
        let mut state = self.shared_state.borrow_mut();
        match message {
            TimerMessage::Rekey(peer_ref, _our_index) => {
                let mut peer = peer_ref.borrow_mut();
                let noise = NoiseBuilder::new("Noise_IKpsk2_25519_ChaChaPoly_BLAKE2s".parse().unwrap())
                    .local_private_key(&state.interface_info.private_key.expect("no private key!"))
                    .remote_public_key(&peer.info.pub_key)
                    .prologue("WireGuard v1 zx2c4 Jason@zx2c4.com".as_bytes())
                    .psk(2, &peer.info.psk.unwrap_or_else(|| [0u8; 32]))
                    .build_initiator().unwrap();
                peer.set_next_session(noise.into());

                let _ = state.index_map.insert(peer.our_next_index().unwrap(), peer_ref.clone());

                let init_packet = peer.get_handshake_packet();
                let endpoint = peer.info.endpoint.unwrap().clone();

                self.handle.spawn(self.udp_tx.clone().send((endpoint, init_packet)).then(|_| Ok(())));
                info!("sent rekey");
            },
            TimerMessage::KeepAlive(peer_ref, _our_index) => {
                let mut peer = peer_ref.borrow_mut();
                let mut packet = vec![0u8; 1500];
                packet[0] = 4;
                let their_index = peer.their_current_index().expect("no current index for them");
                let endpoint = peer.info.endpoint.unwrap();
                peer.tx_bytes += packet.len() as u64;
                let noise = peer.current_noise().expect("current noise session");
                LittleEndian::write_u32(&mut packet[4..], their_index);
                LittleEndian::write_u64(&mut packet[8..], noise.sending_nonce().unwrap());
                let len = noise.write_message(&[], &mut packet[16..]).expect("failed to encrypt outgoing keepalive");
                packet.truncate(len + 16);
                self.handle.spawn(self.udp_tx.clone().send((endpoint, packet)).then(|_| Ok(())));
                debug!("sent keepalive");
            }
        }
    }

    // Just this way to avoid a double-mutable-borrow while peeking.
    fn peek_and_handle(&mut self) -> Result<bool,()> {
        let routed = {
            let packet = match self.outgoing_rx.peek() {
                Ok(Async::Ready(Some(packet))) => packet,
                Ok(Async::NotReady) => return Ok(false),
                Ok(Async::Ready(None)) | Err(_) => return Err(()),
            };

            trace_packet("received UTUN packet: ", packet.payload());
            let state = self.shared_state.borrow();
            let mut out_packet = vec![0u8; 1500];
            let peer = match packet {
                &UtunPacket::Inet4(ref packet) => {
                    let destination = Ipv4Packet::new(&packet).unwrap().get_destination();
                    state.ip4_map.longest_match(destination).map(|(_, _, peer)| peer)
                },
                &UtunPacket::Inet6(ref packet) => {
                    let destination = Ipv6Packet::new(&packet).unwrap().get_destination();
                    state.ip6_map.longest_match(destination).map(|(_, _, peer)| peer)
                }
            };

            if let Some(peer) = peer {
                let mut peer = peer.borrow_mut();
                out_packet[0] = 4;
                if let Some(their_index) = peer.their_current_index() {
                    let endpoint = peer.info.endpoint.unwrap();
                    peer.tx_bytes += packet.payload().len() as u64;
                    let noise = peer.current_noise().expect("current noise session");
                    LittleEndian::write_u32(&mut out_packet[4..], their_index);
                    LittleEndian::write_u64(&mut out_packet[8..], noise.sending_nonce().unwrap());
                    let len = noise.write_message(&packet.payload(), &mut out_packet[16..]).expect("failed to encrypt outgoing UDP packet");
                    out_packet.truncate(16 + len);
                    self.handle.spawn(self.udp_tx.clone().send((endpoint, out_packet)).then(|_| Ok(())));
                    true
                } else {
                    debug!("got outgoing packet with no current session");
                    false
                }
            } else {
                // TODO return another error and generate ICMP "no route" packet
                warn!("got packet with no available outgoing route");
                false
            }
        };
        if routed {
            let _ = self.outgoing_rx.poll();
        }
        return Ok(routed)
    }
}

impl Future for PeerServer {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        // Handle pending state-changing timers
        loop {
            match self.timer_rx.poll() {
                Ok(Async::Ready(Some(message))) => self.handle_timer(message),
                Ok(Async::NotReady) => break,
                Ok(Async::Ready(None)) | Err(_) => return Err(()),
            }
        }

        // Handle UDP packets from the outside world
        loop {
            match self.udp_stream.poll() {
                Ok(Async::Ready(Some((addr, packet)))) => self.handle_incoming_packet(addr, packet),
                Ok(Async::NotReady) => break,
                Ok(Async::Ready(None)) | Err(_) => return Err(()),
            }
        }

        // Handle packets coming from the local tunnel
        loop {
            match self.peek_and_handle() {
                Ok(false) => break,
                Err(_) => return Err(()),
                _ => {}
            }
        }

        Ok(Async::NotReady)
    }
}
