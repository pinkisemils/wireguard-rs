use super::{SharedState, UtunPacket};
use consts::{REKEY_TIMEOUT, REKEY_AFTER_TIME, REJECT_AFTER_TIME, REKEY_ATTEMPT_TIME, KEEPALIVE_TIMEOUT, MAX_CONTENT_SIZE, TIMER_TICK_DURATION};
use cookie;
use interface::SharedPeer;
use peer::{Peer, SessionType};
use timer::{Timer, TimerMessage};

use std::io;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

use byteorder::{ByteOrder, LittleEndian};
use failure::{Error, err_msg};
use futures::{Async, Future, Stream, Sink, Poll, unsync::mpsc, stream};
use socket2::{Socket, Domain, Type, Protocol};
use tokio_core::net::{UdpSocket, UdpCodec, UdpFramed};
use tokio_core::reactor::Handle;


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
            v6addr => v6addr
        };
        addr.set_ip(mapped_ip);
        addr
    }
}

pub struct PeerServer {
    handle       : Handle,
    shared_state : SharedState,
    udp_stream   : stream::SplitStream<UdpFramed<VecUdpCodec>>,
    timer        : Timer,
    outgoing_tx  : mpsc::Sender<UtunPacket>,
    outgoing_rx  : mpsc::Receiver<UtunPacket>,
    udp_tx       : mpsc::Sender<(SocketAddr, Vec<u8>)>,
    tunnel_tx    : mpsc::Sender<Vec<u8>>,
}

impl PeerServer {
    pub fn bind(handle: Handle, shared_state: SharedState, tunnel_tx: mpsc::Sender<Vec<u8>>) -> Result<Self, Error> {
        let timer  = Timer::default();
        let port   = shared_state.borrow().interface_info.listen_port.unwrap_or(0);
        let socket = Socket::new(Domain::ipv6(), Type::dgram(), Some(Protocol::udp()))?;
        socket.set_only_v6(false)?;
        socket.set_nonblocking(true)?;
        socket.bind(&SocketAddr::from((Ipv6Addr::unspecified(), port)).into())?;
        let socket = UdpSocket::from_socket(socket.into_udp_socket(), &handle.clone())?;
        let (udp_sink, udp_stream) = socket.framed(VecUdpCodec{}).split();
        let (udp_tx, udp_rx) = mpsc::channel::<(SocketAddr, Vec<u8>)>(1024);
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<UtunPacket>(1024);

        let udp_write_passthrough = udp_sink.sink_map_err(|_| ()).send_all(
            udp_rx.map(|(addr, packet)| {
                trace!("sending UDP packet to {:?}", &addr);
                (addr, packet)
            }).map_err(|_| ()))
            .then(|_| Ok(()));
        handle.spawn(udp_write_passthrough);

        Ok(PeerServer {
            handle, shared_state, timer, udp_stream, udp_tx, tunnel_tx, outgoing_tx, outgoing_rx
        })
    }

    pub fn tx(&self) -> mpsc::Sender<UtunPacket> {
        self.outgoing_tx.clone()
    }

    fn send_to_peer(&self, payload: PeerServerMessage) {
        self.handle.spawn(self.udp_tx.clone().send(payload).then(|_| Ok(())));
    }

    fn send_to_tunnel(&self, packet: Vec<u8>) {
        self.handle.spawn(self.tunnel_tx.clone().send(packet).then(|_| Ok(())));
    }

    fn handle_ingress_packet(&mut self, addr: SocketAddr, packet: &[u8]) -> Result<(), Error> {
        trace!("got a UDP packet from {:?} of length {}, packet type {}", &addr, packet.len(), packet[0]);
        match packet[0] {
            1 => self.handle_ingress_handshake_init(addr, packet),
            2 => self.handle_ingress_handshake_resp(addr, packet),
            3 => bail!("cookie messages not yet supported."),
            4 => self.handle_ingress_transport(addr, packet),
            _ => bail!("unknown wireguard message type")
        }
    }

    fn handle_ingress_handshake_init(&mut self, addr: SocketAddr, packet: &[u8]) -> Result<(), Error> {
        ensure!(packet.len() == 148, "handshake init packet length is incorrect");
        let mut state = self.shared_state.borrow_mut();
        {
            let pubkey = state.interface_info.pub_key.as_ref()
                .ok_or_else(|| err_msg("must have local interface key"))?;
            let (mac_in, mac_out) = packet.split_at(116);
            cookie::verify_mac1(pubkey, mac_in, &mac_out[..16])?;
        }

        debug!("got handshake initiation request (0x01)");

        let handshake = Peer::process_incoming_handshake(
            &state.interface_info.private_key.ok_or_else(|| err_msg("no private key!"))?,
            packet)?;

        let peer_ref = state.pubkey_map.get(handshake.their_pubkey())
            .ok_or_else(|| err_msg("unknown peer pubkey"))?.clone();

        let mut peer = peer_ref.borrow_mut();
        let (response, next_index) = peer.complete_incoming_handshake(addr, handshake)?;
        let _ = state.index_map.insert(next_index, peer_ref.clone());

        self.send_to_peer((addr, response));
        info!("sent handshake response, ratcheted session (index {}).", next_index);

        Ok(())
    }

    // TODO use the address to update endpoint if it changes i suppose
    fn handle_ingress_handshake_resp(&mut self, _addr: SocketAddr, packet: &[u8]) -> Result<(), Error> {
        ensure!(packet.len() == 92, "handshake resp packet length is incorrect");
        let mut state = self.shared_state.borrow_mut();
        {
            let pubkey = state.interface_info.pub_key.as_ref()
                .ok_or_else(|| err_msg("must have local interface key"))?;
            let (mac_in, mac_out) = packet.split_at(60);
            cookie::verify_mac1(pubkey, mac_in, &mac_out[..16])?;
        }
        debug!("got handshake response (0x02)");

        let our_index = LittleEndian::read_u32(&packet[8..]);
        let peer_ref  = state.index_map.get(&our_index)
            .ok_or_else(|| format_err!("unknown our_index ({})", our_index))?
            .clone();
        let mut peer = peer_ref.borrow_mut();
        let dead_index = peer.process_incoming_handshake_response(packet)?;
        if let Some(index) = dead_index {
            let _ = state.index_map.remove(&index);
        }
        if peer.ready_for_transport() {
            if !peer.outgoing_queue.is_empty() {
                debug!("sending {} queued egress packets", peer.outgoing_queue.len());
                while let Some(packet) = peer.outgoing_queue.pop_front() {
                    self.send_to_peer(peer.handle_outgoing_transport(packet.payload())?);
                }
            } else {
                self.send_to_peer(peer.handle_outgoing_transport(&[])?);
            }
        } else {
            error!("peer not ready for transport after processing handshake response. this shouldn't happen.");
        }
        info!("handshake response received, current session now {}", our_index);

        self.timer.spawn_delayed(&self.handle,
                                 *KEEPALIVE_TIMEOUT,
                                 TimerMessage::PassiveKeepAlive(peer_ref.clone(), our_index));

        self.timer.spawn_delayed(&self.handle,
                                 *REJECT_AFTER_TIME,
                                 TimerMessage::Reject(peer_ref.clone(), our_index));

        if let Some(persistent_keep_alive) = peer.info.keep_alive_interval {
            self.timer.spawn_delayed(&self.handle,
                                     Duration::from_secs(u64::from(persistent_keep_alive)),
                                     TimerMessage::PersistentKeepAlive(peer_ref.clone(), our_index));
        }
        Ok(())
    }

    fn handle_ingress_transport(&mut self, addr: SocketAddr, packet: &[u8]) -> Result<(), Error> {
        let mut state      = self.shared_state.borrow_mut();
        let     our_index  = LittleEndian::read_u32(&packet[4..]);
        let     peer_ref   = state.index_map.get(&our_index).ok_or_else(|| err_msg("unknown our_index"))?.clone();
        let     raw_packet = {
            let mut peer = peer_ref.borrow_mut();
            let (raw_packet, transition) = peer.handle_incoming_transport(addr, packet)?;

            if let Some(possible_dead_index) = transition {
                if let Some(index) = possible_dead_index {
                    let _ = state.index_map.remove(&index);
                }

                let outgoing: Vec<UtunPacket> = peer.outgoing_queue.drain(..).collect();

                for packet in outgoing {
                    match peer.handle_outgoing_transport(packet.payload()) {
                        Ok(message) => self.send_to_peer(message),
                        Err(e) => warn!("failed to encrypt packet: {}", e)
                    }
                }
            }
            raw_packet
        };

        if raw_packet.is_empty() {
            debug!("received keepalive.");
            return Ok(()) // short-circuit on keep-alives
        }

        state.router.validate_source(&raw_packet, &peer_ref)?;
        trace!("received transport packet");
        self.send_to_tunnel(raw_packet);
        Ok(())
    }

    fn send_handshake_init(&mut self, peer_ref: &SharedPeer) -> Result<u32, Error> {
        let mut state       = self.shared_state.borrow_mut();
        let mut peer        = peer_ref.borrow_mut();
        let     private_key = &state.interface_info.private_key.ok_or_else(|| err_msg("no private key!"))?;

        let (endpoint, init_packet, new_index, dead_index) = peer.initiate_new_session(private_key)?;

        let _ = state.index_map.insert(new_index, peer_ref.clone());
        if let Some(index) = dead_index {
            trace!("removing abandoned 'next' session ({}) from index map", index);
            let _ = state.index_map.remove(&index);
        }

        self.send_to_peer((endpoint, init_packet));
        peer.last_sent_init = Some(Instant::now());
        peer.last_tun_queue = peer.last_tun_queue.or_else(|| Some(Instant::now()));
        let when = *REKEY_TIMEOUT + *TIMER_TICK_DURATION * 2;
        self.timer.spawn_delayed(&self.handle,
                                 when,
                                 TimerMessage::Rekey(peer_ref.clone(), new_index));
        Ok(new_index)
    }

    fn handle_timer(&mut self, message: TimerMessage) -> Result<(), Error> {
        match message {
            TimerMessage::Rekey(peer_ref, our_index) => {
                {
                    let mut peer = peer_ref.borrow_mut();
                    let     now  = Instant::now();

                    match peer.find_session(our_index) {
                        Some((_, SessionType::Next)) => {
                            if let Some(sent_init) = peer.last_sent_init {
                                let since_last_init = now.duration_since(sent_init);
                                if since_last_init < *REKEY_TIMEOUT {
                                    let wait = *REKEY_TIMEOUT - since_last_init + *TIMER_TICK_DURATION * 2;
                                    self.timer.spawn_delayed(&self.handle,
                                                             wait,
                                                             TimerMessage::Rekey(peer_ref.clone(), our_index));
                                    bail!("too soon since last init sent, waiting {:?} ({})", wait, our_index);
                                }
                            }
                            if let Some(init_attempt_epoch) = peer.last_tun_queue {
                                let since_attempt_epoch = now.duration_since(init_attempt_epoch);
                                if since_attempt_epoch > *REKEY_ATTEMPT_TIME {
                                    peer.last_tun_queue = None;
                                    bail!("REKEY_ATTEMPT_TIME exceeded ({})", our_index);
                                }
                            }
                        },
                        Some((_, SessionType::Current)) => {
                            if let Some(last_handshake) = peer.last_handshake {
                                let since_last_handshake = now.duration_since(last_handshake);
                                if since_last_handshake <= *REKEY_AFTER_TIME {
                                    let wait = *REKEY_AFTER_TIME - since_last_handshake + *TIMER_TICK_DURATION * 2;
                                    self.timer.spawn_delayed(&self.handle,
                                                             wait,
                                                             TimerMessage::Rekey(peer_ref.clone(), our_index));
                                    bail!("recent last complete handshake - waiting {:?} ({})", wait, our_index);
                                }
                            }
                        },
                        _ => bail!("index is linked to a dead session, bailing.")
                    }
                }

                let new_index = self.send_handshake_init(&peer_ref)?;
                debug!("sent handshake init (Rekey timer) ({} -> {})", our_index, new_index);

            },
            TimerMessage::Reject(peer_ref, our_index) => {
                let mut peer  = peer_ref.borrow_mut();
                let mut state = self.shared_state.borrow_mut();

                debug!("rejection timeout for session {}, ejecting", our_index);

                match peer.find_session(our_index) {
                    Some((_, SessionType::Next))    => { peer.sessions.next = None; },
                    Some((_, SessionType::Current)) => { peer.sessions.current = None; },
                    Some((_, SessionType::Past))    => { peer.sessions.past = None; },
                    None                            => debug!("reject timeout for already-killed session")
                }
                let _ = state.index_map.remove(&our_index);
            },
            TimerMessage::PassiveKeepAlive(peer_ref, our_index) => {
                let mut peer = peer_ref.borrow_mut();
                {
                    let (session, session_type) = peer.find_session(our_index).ok_or_else(|| err_msg("missing session for timer"))?;
                    ensure!(session_type == SessionType::Current, "expired session for passive keepalive timer");

                    if let Some(last_sent) = session.last_sent {
                        let last_sent_packet = Instant::now().duration_since(last_sent);
                        if last_sent_packet < *KEEPALIVE_TIMEOUT {
                            self.timer.spawn_delayed(&self.handle,
                                                     *KEEPALIVE_TIMEOUT - last_sent_packet + *TIMER_TICK_DURATION,
                                                     TimerMessage::PassiveKeepAlive(peer_ref.clone(), our_index));
                            bail!("passive keepalive tick (waiting {:?})", *KEEPALIVE_TIMEOUT - last_sent_packet);
                        }
                    }
                }

                self.send_to_peer(peer.handle_outgoing_transport(&[])?);
                debug!("sent passive keepalive packet ({})", our_index);

                self.timer.spawn_delayed(&self.handle,
                                         *KEEPALIVE_TIMEOUT,
                                         TimerMessage::PassiveKeepAlive(peer_ref.clone(), our_index));
            },
            TimerMessage::PersistentKeepAlive(peer_ref, our_index) => {
                let mut peer = peer_ref.borrow_mut();
                {
                    let (_, session_type) = peer.find_session(our_index).ok_or_else(|| err_msg("missing session for timer"))?;
                    ensure!(session_type == SessionType::Current, "expired session for persistent keepalive timer");
                }

                self.send_to_peer(peer.handle_outgoing_transport(&[])?);
                debug!("sent persistent keepalive packet ({})", our_index);

                if let Some(persistent_keepalive) = peer.info.keep_alive_interval {
                    self.timer.spawn_delayed(&self.handle,
                                             Duration::from_secs(u64::from(persistent_keepalive)),
                                             TimerMessage::PersistentKeepAlive(peer_ref.clone(), our_index));

                }
            }
        }
        Ok(())
    }

    // Just this way to avoid a double-mutable-borrow while peeking.
    fn handle_egress_packet(&mut self, packet: UtunPacket) -> Result<(), Error> {
        ensure!(!packet.payload().is_empty() && packet.payload().len() <= MAX_CONTENT_SIZE, "egress packet outside of size bounds");

        let peer_ref = self.shared_state.borrow_mut().router.route_to_peer(packet.payload())
            .ok_or_else(|| err_msg("no route to peer"))?;

        let needs_handshake = {
            let mut peer = peer_ref.borrow_mut();
            peer.outgoing_queue.push_back(packet);

            if peer.ready_for_transport() {
                if peer.outgoing_queue.len() > 1 {
                    debug!("sending {} queued egress packets", peer.outgoing_queue.len());
                }

                while let Some(packet) = peer.outgoing_queue.pop_front() {
                    self.send_to_peer(peer.handle_outgoing_transport(packet.payload())?);
                }
            }
            peer.needs_new_handshake()
        };

        if needs_handshake {
            debug!("sending handshake init because peer needs it");
            self.send_handshake_init(&peer_ref)?;
        }
        Ok(())
    }
}

impl Future for PeerServer {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        // Handle pending state-changing timers
        loop {
            match self.timer.poll() {
                Ok(Async::Ready(Some(message))) => {
                    let _ = self.handle_timer(message).map_err(|e| debug!("TIMER: {}", e));
                },
                Ok(Async::NotReady) => break,
                Ok(Async::Ready(None)) | Err(_) => return Err(()),
            }
        }

        // Handle UDP packets from the outside world
        loop {
            match self.udp_stream.poll() {
                Ok(Async::Ready(Some((addr, packet)))) => {
                    let _ = self.handle_ingress_packet(addr, &packet).map_err(|e| warn!("UDP ERR: {:?}", e));
                },
                Ok(Async::NotReady) => break,
                Ok(Async::Ready(None)) | Err(_) => return Err(()),
            }
        }

        // Handle packets coming from the local tunnel
        loop {
            match self.outgoing_rx.poll() {
                Ok(Async::Ready(Some(packet))) => {
                    let _ = self.handle_egress_packet(packet).map_err(|e| warn!("UDP ERR: {:?}", e));
                },
                Ok(Async::NotReady) => break,
                Ok(Async::Ready(None)) | Err(_) => return Err(()),
            }
        }

        Ok(Async::NotReady)
    }
}
