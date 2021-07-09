use crate::channel::ChannelConfig;
use crate::client::{LocalClient, LocalClientConnected};
use crate::error::{ConnectionError, RenetError};
use crate::packet::{Packet, Unauthenticaded};
use crate::protocol::ServerAuthenticationProtocol;
use crate::remote_connection::{ClientId, ConnectionConfig, NetworkInfo, RemoteConnection};

use super::ServerConfig;

use log::{debug, error, info};

use std::collections::HashMap;
use std::collections::VecDeque;
use std::io;
use std::net::{SocketAddr, UdpSocket};

#[derive(Debug, Clone)]
pub enum ServerEvent {
    ClientConnected(ClientId),
    ClientDisconnected(ClientId),
}

// TODO: add internal buffer?
pub struct Server<P: ServerAuthenticationProtocol> {
    config: ServerConfig,
    socket: UdpSocket,
    remote_clients: HashMap<ClientId, RemoteConnection<P>>,
    locale_clients: HashMap<ClientId, LocalClient>,
    connecting: HashMap<ClientId, RemoteConnection<P>>,
    channels_config: HashMap<u8, Box<dyn ChannelConfig>>,
    events: VecDeque<ServerEvent>,
    connection_config: ConnectionConfig,
}

impl<P> Server<P>
where
    P: ServerAuthenticationProtocol,
{
    pub fn new(
        socket: UdpSocket,
        config: ServerConfig,
        connection_config: ConnectionConfig,
        channels_config: HashMap<u8, Box<dyn ChannelConfig>>,
    ) -> Result<Self, RenetError> {
        socket.set_nonblocking(true)?;

        Ok(Self {
            socket,
            remote_clients: HashMap::new(),
            locale_clients: HashMap::new(),
            connecting: HashMap::new(),
            config,
            channels_config,
            connection_config,
            events: VecDeque::new(),
        })
    }

    pub fn create_local_client(&mut self, client_id: u64) -> LocalClientConnected {
        let channels = self.channels_config.keys().copied().collect();
        self.events
            .push_back(ServerEvent::ClientConnected(client_id));
        let (local_client_connected, local_client) = LocalClientConnected::new(client_id, channels);
        self.locale_clients.insert(client_id, local_client);
        local_client_connected
    }

    pub fn has_clients(&self) -> bool {
        !self.remote_clients.is_empty() || !self.locale_clients.is_empty()
    }

    pub fn get_event(&mut self) -> Option<ServerEvent> {
        self.events.pop_front()
    }

    fn find_client_by_addr(&mut self, addr: &SocketAddr) -> Option<&mut RemoteConnection<P>> {
        self.remote_clients
            .values_mut()
            .find(|c| *c.addr() == *addr)
    }

    fn find_connecting_by_addr(&mut self, addr: &SocketAddr) -> Option<&mut RemoteConnection<P>> {
        self.connecting.values_mut().find(|c| c.addr() == addr)
    }

    pub fn get_client_network_info(&mut self, client_id: ClientId) -> Option<&NetworkInfo> {
        if let Some(connection) = self.remote_clients.get_mut(&client_id) {
            return Some(connection.network_info());
        }
        None
    }

    // TODO: Add method _to_all_remote/ _to_all_local
    pub fn send_message_to_all_clients<C: Into<u8>>(&mut self, channel_id: C, message: Box<[u8]>) {
        let channel_id = channel_id.into();
        for remote_connection in self.remote_clients.values_mut() {
            remote_connection.send_message(channel_id, message.clone());
        }

        for local_connection in self.locale_clients.values_mut() {
            local_connection.send_message(channel_id, message.clone());
        }
    }

    pub fn send_message_to_client<C: Into<u8>>(
        &mut self,
        client_id: ClientId,
        channel_id: C,
        message: Box<[u8]>,
    ) {
        let channel_id = channel_id.into();
        if let Some(remote_connection) = self.remote_clients.get_mut(&client_id) {
            remote_connection.send_message(channel_id, message);
        } else if let Some(local_connection) = self.locale_clients.get_mut(&client_id) {
            local_connection.send_message(channel_id, message);
        }
    }

    pub fn receive_message<C: Into<u8>>(
        &mut self,
        client_id: ClientId,
        channel_id: C,
    ) -> Option<Box<[u8]>> {
        let channel_id = channel_id.into();
        if let Some(remote_client) = self.remote_clients.get_mut(&client_id) {
            return remote_client.receive_message(channel_id);
        } else if let Some(local_client) = self.locale_clients.get_mut(&client_id) {
            return local_client.receive_message(channel_id);
        }

        None
    }

    pub fn get_clients_id(&self) -> Vec<ClientId> {
        let mut clients: Vec<ClientId> = self.remote_clients.keys().copied().collect();
        let mut local_clients: Vec<ClientId> = self.locale_clients.keys().copied().collect();
        clients.append(&mut local_clients);

        clients
    }

    pub fn update(&mut self) {
        let mut timed_out_connections: Vec<ClientId> = vec![];
        for (&client_id, connection) in self.remote_clients.iter_mut() {
            connection.update();
            if connection.has_timed_out() {
                timed_out_connections.push(client_id);
            }
        }

        for &client_id in timed_out_connections.iter() {
            self.remote_clients.remove(&client_id);
            self.events
                .push_back(ServerEvent::ClientDisconnected(client_id));
            info!("Client {} disconnected.", client_id);
        }

        if let Err(e) = self.process_events() {
            error!("Error while processing events:\n{:?}", e);
        }
        self.update_pending_connections();
    }

    pub fn send_packets(&mut self) {
        for (client_id, connection) in self.remote_clients.iter_mut() {
            if let Err(e) = connection.send_packets(&self.socket) {
                error!("Failed to send packet for client {}: {:?}", client_id, e);
            }
        }
    }

    pub fn process_payload_from(
        &mut self,
        payload: &[u8],
        addr: &SocketAddr,
    ) -> Result<(), RenetError> {
        if let Some(client) = self.find_client_by_addr(addr) {
            return client.process_payload(payload);
        }

        if self.remote_clients.len() >= self.config.max_clients {
            let packet = Unauthenticaded::ConnectionError(ConnectionError::MaxPlayer);
            try_send_packet(&self.socket, packet, addr)?;
            debug!("Connection Denied to addr {}, server is full.", addr);
            return Ok(());
        }

        match self.find_connecting_by_addr(addr) {
            Some(connection) => {
                if let Err(e) = connection.process_payload(payload) {
                    error!("{}", e)
                }
            }
            None => {
                let packet = bincode::deserialize::<Packet>(payload)?;
                if let Packet::Unauthenticaded(Unauthenticaded::Protocol { payload }) = packet {
                    let protocol = P::from_payload(&payload)?;
                    let id = protocol.id();
                    info!("Created new protocol from payload with client id {}", id);
                    let new_connection = RemoteConnection::new(
                        protocol.id(),
                        *addr,
                        self.connection_config.clone(),
                        protocol,
                    );
                    self.connecting.insert(id, new_connection);
                }
            }
        };

        Ok(())
    }

    fn process_events(&mut self) -> Result<(), RenetError> {
        let mut buffer = vec![0u8; self.config.max_payload_size];
        loop {
            match self.socket.recv_from(&mut buffer) {
                Ok((len, addr)) => {
                    if let Err(e) = self.process_payload_from(&buffer[..len], &addr) {
                        error!("Error while processing events:\n{:?}", e);
                    }
                }
                // Break from the loop if would block
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    return Ok(());
                }
                Err(e) => return Err(RenetError::IOError(e)),
            };
        }
    }

    fn update_pending_connections(&mut self) {
        let mut connected_clients = vec![];
        let mut disconnected_clients = vec![];
        for connection in self.connecting.values_mut() {
            connection.update();
            if connection.has_timed_out() {
                disconnected_clients.push(connection.client_id());
                continue;
            }

            if connection.is_connected() {
                connected_clients.push(connection.client_id());
            } else if let Ok(Some(payload)) = connection.create_protocol_payload() {
                let packet = Packet::Unauthenticaded(Unauthenticaded::Protocol { payload });
                send_packet(&self.socket, packet, connection.addr());
            }
        }

        for client_id in disconnected_clients {
            let connection = self
                .connecting
                .remove(&client_id)
                .expect("Disconnected Clients always exists");
            info!("Request connection {} failed.", connection.client_id());
        }

        for client_id in connected_clients {
            let mut connection = self
                .connecting
                .remove(&client_id)
                .expect("Connected Client always exist");
            if self.remote_clients.len() >= self.config.max_clients {
                info!(
                    "Connection from {} successfuly stablished but server was full.",
                    connection.addr()
                );
                let packet = Unauthenticaded::ConnectionError(ConnectionError::MaxPlayer);
                send_packet(&self.socket, packet, connection.addr());

                continue;
            }

            info!(
                "Connection stablished with client {} ({}).",
                connection.client_id(),
                connection.addr(),
            );

            for (channel_id, channel_config) in self.channels_config.iter() {
                let channel = channel_config.new_channel();
                connection.add_channel(*channel_id, channel);
            }

            self.events
                .push_back(ServerEvent::ClientConnected(connection.client_id()));
            self.remote_clients
                .insert(connection.client_id(), connection);
        }
    }
}

fn try_send_packet(socket: &UdpSocket, packet: impl Into<Packet>, addr: &SocketAddr) -> Result<(), RenetError> {
    let packet: Packet = packet.into();
    let packet = bincode::serialize(&packet)?;
    socket.send_to(&packet, addr)?;
    Ok(())
}

fn send_packet(socket: &UdpSocket, packet: impl Into<Packet>, addr: &SocketAddr) {
    let packet: Packet = packet.into();
    let packet = match bincode::serialize(&packet) {
        Err(e) => {
            error!("Failed to serialize packet {}", e);
            return;
        }
        Ok(p) => p,
    };
    if let Err(e) = socket.send_to(&packet, addr) {
        error!("Failed to serialize packet {}", e);
    }
}
