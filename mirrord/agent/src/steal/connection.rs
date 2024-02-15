use std::{
    collections::HashSet,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
};

use bytes::Bytes;
use fancy_regex::Regex;
use mirrord_protocol::{
    tcp::{HttpResponseFallback, NewTcpConnection, TcpClose, HTTP_FRAMED_VERSION},
    RemoteError::{BadHttpFilterExRegex, BadHttpFilterRegex},
};
use streammap_ext::StreamMap;
use tokio::{
    io::{AsyncWriteExt, ReadHalf, WriteHalf},
    net::TcpStream,
    sync::mpsc::{channel, Receiver, Sender},
};
use tokio_stream::StreamExt;
use tokio_util::io::ReaderStream;
use tracing::error;

use self::{
    http::filter_task,
    subscriptions::{IpTablesRedirector, PortSubscription, PortSubscriptions},
};
use super::*;
use crate::{error::Result, steal::http::HttpFilter, AgentError::HttpRequestReceiverClosed};

/// Created once per agent during initialization.
///
/// Runs as a separate thread while the agent lives.
///
/// - (agent -> stealer) communication is handled by [`command_rx`];
/// - (stealer -> agent) communication is handled by [`client_senders`], and the [`Sender`] channels
///   come inside [`StealerCommand`]s through  [`command_rx`];
pub(crate) struct TcpConnectionStealer {
    /// For managing active subscriptions and port redirections.
    port_subscriptions: PortSubscriptions<IpTablesRedirector>,

    /// Communication between (agent -> stealer) task.
    ///
    /// The agent controls the stealer task through [`TcpStealerAPI::command_tx`].
    command_rx: Receiver<StealerCommand>,

    /// Connected clients (layer instances) and the channels which the stealer task uses to send
    /// back messages (stealer -> agent -> layer).
    clients: HashMap<ClientId, (Sender<DaemonTcp>, semver::Version)>,

    index_allocator: IndexAllocator<ConnectionId, 100>,

    /// Used to send data back to the original remote connection.
    write_streams: HashMap<ConnectionId, WriteHalf<TcpStream>>,

    /// Used to read data from the remote connections.
    read_streams: StreamMap<ConnectionId, ReaderStream<ReadHalf<TcpStream>>>,

    /// Associates a `ConnectionId` with a `ClientID`, so we can send the data we read from
    /// [`TcpConnectionStealer::read_streams`] to the appropriate client (layer).
    connection_clients: HashMap<ConnectionId, ClientId>,

    /// Map a `ClientId` to a set of its `ConnectionId`s. Used to close all connections when
    /// client closes.
    client_connections: HashMap<ClientId, HashSet<ConnectionId>>,

    /// Mspc sender to clone and give http filter managers so that they can send back requests.
    http_request_sender: Sender<HandlerHttpRequest>,

    /// Receives filtered HTTP requests that need to be forwarded a client.
    http_request_receiver: Receiver<HandlerHttpRequest>,

    /// For informing the [`Self::start`] task about closed connections.
    http_connection_close_sender: Sender<ConnectionId>,

    /// [`Self::start`] listens to this, removes the connection and frees the index.
    http_connection_close_receiver: Receiver<ConnectionId>,

    /// Keep track of the clients that already received an http request out of a connection.
    /// This is used to inform them when the connection is closed.
    ///
    /// Note: The set of clients here is not the same as the set of clients that subscribe to the
    /// port of a connection, as some clients might have defined a regex that no request matched,
    /// so they did not get any request out of this connection, so they are not even aware of this
    /// connection.
    http_connection_clients: HashMap<ConnectionId, HashSet<ClientId>>,

    /// Maps each pending request id to the sender into the channel with the hyper service that
    /// received that requests and is waiting for the response.
    http_response_senders: HashMap<(ConnectionId, RequestId), oneshot::Sender<Response>>,
}

impl TcpConnectionStealer {
    pub const TASK_NAME: &'static str = "Stealer";

    /// Initializes a new [`TcpConnectionStealer`] fields, but doesn't start the actual working
    /// task (call [`TcpConnectionStealer::start`] to do so).
    #[tracing::instrument(level = "trace")]
    pub(crate) async fn new(command_rx: Receiver<StealerCommand>) -> Result<Self, AgentError> {
        let (http_request_sender, http_request_receiver) = channel(1024);
        let (connection_close_sender, connection_close_receiver) = channel(1024);

        let port_subscriptions = {
            let flush_connections = std::env::var("MIRRORD_AGENT_STEALER_FLUSH_CONNECTIONS")
                .ok()
                .and_then(|var| var.parse::<bool>().ok())
                .unwrap_or_default();
            let redirector = IpTablesRedirector::new(flush_connections).await?;

            PortSubscriptions::new(redirector, 4)
        };

        Ok(Self {
            port_subscriptions,
            command_rx,
            clients: HashMap::with_capacity(8),
            index_allocator: Default::default(),
            write_streams: HashMap::with_capacity(8),
            read_streams: StreamMap::with_capacity(8),
            connection_clients: HashMap::with_capacity(8),
            client_connections: HashMap::with_capacity(8),
            http_request_sender,
            http_request_receiver,
            http_connection_close_sender: connection_close_sender,
            http_connection_close_receiver: connection_close_receiver,
            http_connection_clients: HashMap::with_capacity(8),
            http_response_senders: HashMap::with_capacity(8),
        })
    }

    /// Runs the tcp traffic stealer loop.
    ///
    /// The loop deals with 6 different paths:
    ///
    /// 1. Receiving [`StealerCommand`]s and calling [`TcpConnectionStealer::handle_command`];
    ///
    /// 2. Accepting remote connections through the [`TcpConnectionStealer::stealer`]
    /// [`TcpListener`]. We steal traffic from the created streams.
    ///
    /// 3. Reading incoming data from the stolen remote connections (accepted in 2.) and forwarding
    /// to clients.
    ///
    /// 4. Receiving filtered HTTP requests and forwarding them to clients (layers).
    ///
    /// 5. Receiving the connection IDs of closing filtered HTTP connections, and informing all
    /// clients that were forward a request out of that connection of the closing of that
    /// connection.
    ///
    /// 6. Handling the cancellation of the whole stealer thread.
    #[tracing::instrument(level = "trace", skip(self))]
    pub(crate) async fn start(
        mut self,
        cancellation_token: CancellationToken,
    ) -> Result<(), AgentError> {
        loop {
            select! {
                command = self.command_rx.recv() => {
                    if let Some(command) = command {
                        self.handle_command(command).await.map_err(| e | {
                            error!("Failed handling command {e:#?}");
                            e
                        })?;
                    } else { break; }
                },
                // Accepts a connection that we're going to be stealing traffic from.
                accept = self.port_subscriptions.next_connection() => {
                    match accept {
                        Ok(accept) => {
                            self.incoming_connection(accept).await?;
                        }
                        Err(fail) => {
                            error!("Something went wrong while accepting a connection {:#?}", fail);
                            break;
                        }
                    }
                }
                Some((connection_id, incoming_data)) = self.read_streams.next() => {
                    // TODO: Should we spawn a task to forward the data?
                    if let Err(fail) = self.forward_incoming_tcp_data(connection_id, incoming_data).await {
                        error!("Failed reading incoming tcp data with {fail:#?}!");
                    }
                }
                request = self.http_request_receiver.recv() => self.forward_stolen_http_request(request).await?,
                Some(connection_id) = self.http_connection_close_receiver.recv() => {
                    // Send a close message to all clients that were subscribed to the connection.
                    if let Some(clients) = self.http_connection_clients.remove(&connection_id) {
                        for client_id in clients.into_iter() {
                            if let Some((client_tx, _)) = self.clients.get(&client_id) {
                                client_tx.send(DaemonTcp::Close(TcpClose {connection_id})).await?
                            } else {
                                warn!("Cannot notify client {client_id} on the closing of a connection.")
                            }
                        }
                    }
                    self.index_allocator.free_index(connection_id);
                }

                _ = cancellation_token.cancelled() => {
                    break;
                }
            }
        }

        Ok(())
    }

    /// Forward a stolen HTTP request from the http filter to the direction of the layer.
    ///
    /// HttpFilter --> Stealer --> Layer --> Local App
    ///                        ^- You are here.
    #[tracing::instrument(level = "trace", skip(self))]
    async fn forward_stolen_http_request(
        &mut self,
        request: Option<HandlerHttpRequest>,
    ) -> Result<(), AgentError> {
        let HandlerHttpRequest {
            request,
            response_tx,
        } = request.ok_or(HttpRequestReceiverClosed)?;

        if let Some((daemon_tx, version)) = self.clients.get(&request.client_id) {
            // Note down: client_id got a request out of connection_id.
            self.http_connection_clients
                .entry(request.connection_id)
                .or_insert_with(|| HashSet::with_capacity(2))
                .insert(request.client_id);
            self.http_response_senders
                .insert((request.connection_id, request.request_id), response_tx);

            if HTTP_FRAMED_VERSION.matches(version) {
                Ok(daemon_tx
                    .send(DaemonTcp::HttpRequestFramed(
                        request.into_serializable().await?,
                    ))
                    .await?)
            } else {
                Ok(daemon_tx
                    .send(DaemonTcp::HttpRequest(
                        request.into_serializable_fallback().await?,
                    ))
                    .await?)
            }
        } else {
            warn!(
                "Got stolen request for client {:?} that is not, or no longer, subscribed.",
                request.client_id
            );
            Ok(())
        }
    }

    /// Forwards data from a remote stream to the client with `connection_id`.
    #[tracing::instrument(level = "trace", skip(self))]
    async fn forward_incoming_tcp_data(
        &mut self,
        connection_id: ConnectionId,
        incoming_data: Option<Result<Bytes, io::Error>>,
    ) -> Result<(), AgentError> {
        // Create a message to send to the client, or propagate an error.
        let daemon_tcp_message = incoming_data
            .map(|incoming_data_result| match incoming_data_result {
                Ok(bytes) => Ok(DaemonTcp::Data(TcpData {
                    connection_id,
                    bytes: bytes.to_vec(),
                })),
                Err(fail) => {
                    error!("connection id {connection_id:?} read error: {fail:?}");
                    Err(AgentError::IO(fail))
                }
            })
            .unwrap_or(Ok(DaemonTcp::Close(TcpClose { connection_id })))?;

        if let Some((daemon_tx, _)) = self
            .connection_clients
            .get(&connection_id)
            .and_then(|client_id| self.clients.get(client_id))
        {
            Ok(daemon_tx.send(daemon_tcp_message).await?)
        } else {
            // Either connection_id or client_id does not exist. This would be a bug.
            error!(
                "Internal mirrord error: stealer received data on a connection that was already \
                removed."
            );
            debug_assert!(false);
            Ok(())
        }
    }

    /// Forward the whole connection to given client.
    async fn steal_connection(
        &mut self,
        client_id: ClientId,
        address: SocketAddr,
        port: Port,
        stream: TcpStream,
    ) -> Result<()> {
        let connection_id = self.index_allocator.next_index().unwrap();

        let local_address = stream.local_addr()?.ip();

        let (read_half, write_half) = tokio::io::split(stream);
        self.write_streams.insert(connection_id, write_half);
        self.read_streams
            .insert(connection_id, ReaderStream::new(read_half));

        self.connection_clients.insert(connection_id, client_id);
        self.client_connections
            .entry(client_id)
            .or_default()
            .insert(connection_id);

        let new_connection = DaemonTcp::NewConnection(NewTcpConnection {
            connection_id,
            destination_port: port,
            source_port: address.port(),
            remote_address: address.ip(),
            local_address,
        });

        // Send new connection to subscribed layer.
        match self.clients.get(&client_id) {
            Some((daemon_tx, _)) => Ok(daemon_tx.send(new_connection).await?),
            None => {
                // Should not happen.
                debug_assert!(false);
                error!("Internal error: subscriptions of closed client still present.");
                self.close_client(client_id).await
            }
        }
    }

    /// Handles a new remote connection that was accepted on the [`TcpConnectionStealer::stealer`]
    /// listener.
    ///
    /// We separate the stream created by accepting the connection into [`ReadHalf`] and
    /// [`WriteHalf`] to handle reading and sending separately.
    ///
    /// Also creates an association between `connection_id` and `client_id` to be used by
    /// [`forward_incoming_tcp_data`].
    #[tracing::instrument(level = "trace", skip(self))]
    async fn incoming_connection(
        &mut self,
        (stream, address): (TcpStream, SocketAddr),
    ) -> Result<()> {
        let mut real_address = orig_dst::orig_dst_addr(&stream)?;
        // If we use the original IP we would go through prerouting and hit a loop.
        // localhost should always work.
        real_address.set_ip(IpAddr::V4(Ipv4Addr::LOCALHOST));

        match self.port_subscriptions.get(real_address.port()) {
            // We got an incoming connection in a port that is being stolen in its whole by a single
            // client.
            Some(PortSubscription::Unfiltered(client_id)) => {
                self.steal_connection(*client_id, address, real_address.port(), stream)
                    .await
            }

            // We got an incoming connection in a port that is being http filtered by one or more
            // clients.
            Some(PortSubscription::Filtered(filters)) => {
                let connection_id = self.index_allocator.next_index().unwrap();

                tokio::spawn(filter_task(
                    stream,
                    real_address,
                    connection_id,
                    filters.clone(),
                    self.http_request_sender.clone(),
                    self.http_connection_close_sender.clone(),
                ));

                Ok(())
            }

            // Got connection to port without subscribers.
            // This *can* happen due to race conditions
            // (e.g. we just processed an `unsubscribe` command, but our stealer socket already had
            // a connection queued)
            // Here we just drop the TcpStream between our stealer socket and the remote peer (one
            // that attempted to connect with our target) and the connection is closed.
            None => Ok(()),
        }
    }

    /// Registers a new layer instance that has the `steal` feature enabled.
    #[tracing::instrument(level = "trace", skip(self, sender))]
    fn new_client(
        &mut self,
        client_id: ClientId,
        sender: Sender<DaemonTcp>,
        protocol_version: semver::Version,
    ) {
        self.clients.insert(client_id, (sender, protocol_version));
    }

    /// Helper function to handle [`Command::PortSubscribe`] messages.
    ///
    /// Inserts subscription into [`Self::port_subscriptions`].
    #[tracing::instrument(level = "trace", skip(self))]
    async fn port_subscribe(&mut self, client_id: ClientId, port_steal: StealType) -> Result<()> {
        let spec = match port_steal {
            StealType::All(port) => Ok((port, None)),
            StealType::FilteredHttp(port, filter) => Regex::new(&format!("(?i){filter}"))
                .map(|regex| (port, Some(HttpFilter::new_header_filter(regex))))
                .map_err(|err| BadHttpFilterRegex(filter, err.to_string())),
            StealType::FilteredHttpEx(port, filter) => HttpFilter::try_from(&filter)
                .map(|filter| (port, Some(filter)))
                .map_err(|err| BadHttpFilterExRegex(filter, err.to_string())),
        };

        let res = match spec {
            Ok((port, filter)) => self.port_subscriptions.add(client_id, port, filter).await?,
            Err(e) => Err(e.into()),
        };

        self.send_message_to_single_client(&client_id, DaemonTcp::SubscribeResult(res))
            .await
    }

    /// Helper function to handle [`Command::PortUnsubscribe`] messages.
    ///
    /// Removes subscription from [`Self::port_subscriptions`].
    #[tracing::instrument(level = "trace", skip(self))]
    async fn port_unsubscribe(
        &mut self,
        client_id: ClientId,
        port: Port,
    ) -> Result<(), AgentError> {
        self.port_subscriptions.remove(client_id, port).await
    }

    /// Removes the client with `client_id` from our list of clients (layers), and also removes
    /// their subscriptions from [`Self::port_subscriptions`] and all their open
    /// connections.
    #[tracing::instrument(level = "trace", skip(self))]
    async fn close_client(&mut self, client_id: ClientId) -> Result<(), AgentError> {
        self.port_subscriptions.remove_all(client_id).await?;

        // Close and remove all remaining connections of the closed client.
        if let Some(remaining_connections) = self.client_connections.remove(&client_id) {
            for connection_id in remaining_connections.into_iter() {
                self.remove_connection(connection_id);
            }
        }

        self.clients.remove(&client_id);
        Ok(())
    }

    /// Sends a [`DaemonTcp`] message back to the client with `client_id`.
    #[tracing::instrument(level = "trace", skip(self))]
    async fn send_message_to_single_client(
        &mut self,
        client_id: &ClientId,
        message: DaemonTcp,
    ) -> Result<(), AgentError> {
        if let Some((sender, _)) = self.clients.get(client_id) {
            if let Err(fail) = sender.send(message).await {
                warn!(
                    "Failed to send message to client {} with {:#?}!",
                    client_id, fail
                );

                let _ = self.close_client(*client_id).await;

                return Err(fail.into());
            }
        }

        Ok(())
    }

    /// Write the data received from local app via layer to the stream with end client.
    async fn forward_data(&mut self, tcp_data: TcpData) -> std::result::Result<(), AgentError> {
        if let Some(stream) = self.write_streams.get_mut(&tcp_data.connection_id) {
            stream.write_all(&tcp_data.bytes[..]).await?;
            Ok(())
        } else {
            warn!(
                "Trying to send data to closed connection {:?}",
                tcp_data.connection_id
            );
            Ok(())
        }
    }

    /// Forward an HTTP response to a stolen HTTP request from the layer back to the HTTP client.
    ///
    ///                         _______________agent_______________
    /// Local App --> Layer --> ClientConnectionHandler --> Stealer --> Browser
    ///                                                             ^- You are here.
    #[tracing::instrument(
        level = "trace",
        skip(self),
        fields(response_senders = ?self.http_response_senders.keys()),
    )]
    async fn http_response(&mut self, response: HttpResponseFallback) {
        match self
            .http_response_senders
            .remove(&(response.connection_id(), response.request_id()))
        {
            None => {
                error!("Got unexpected http response. Not forwarding.");
            }
            Some(response_tx) => {
                let _res = response // inspecting errors, not propagating.
                    .into_hyper()
                    .inspect_err(|err| {
                        error!("Could not reconstruct http response: {err:?}");
                        debug_assert!(false);
                    })
                    .map(|response| {
                        let _res = response_tx.send(response).inspect_err(|resp| {
                            warn!(
                                "Hyper service has dropped the response receiver before receiving the \
                        response {:?}.",
                                resp
                            );
                        });
                    });
            }
        }
    }

    /// Removes the ([`ReadHalf`], [`WriteHalf`]) pair of streams, disconnecting the remote
    /// connection.
    /// Also remove connection from connection mappings and free the connection index.
    /// This method does not remove from client_connections so that it can be called while
    #[tracing::instrument(level = "trace", skip(self))]
    fn remove_connection(&mut self, connection_id: ConnectionId) -> Option<ClientId> {
        self.write_streams.remove(&connection_id);
        self.read_streams.remove(&connection_id);
        self.index_allocator.free_index(connection_id);
        self.connection_clients.remove(&connection_id)
    }

    /// Close the connection, remove the id from all maps and free the id.
    #[tracing::instrument(level = "trace", skip(self))]
    fn connection_unsubscribe(&mut self, connection_id: ConnectionId) {
        if let Some(client_id) = self.remove_connection(connection_id) {
            // Remove the connection from the set of the connections that belong to its client.
            let mut no_connections_left = false;
            self.client_connections
                .entry(client_id)
                .and_modify(|connections| {
                    connections.remove(&connection_id);
                    no_connections_left = connections.is_empty();
                });
            // If we removed the last connection of this client, remove client from map.
            if no_connections_left {
                self.client_connections.remove(&client_id);
            }
        }
    }

    fn switch_protocol_version(&mut self, client_id: ClientId, protocol_version: semver::Version) {
        if let Some(guard) = self.clients.get_mut(&client_id) {
            guard.1 = protocol_version;
        }
    }

    /// Handles [`Command`]s that were received by [`TcpConnectionStealer::command_rx`].
    #[tracing::instrument(level = "trace", skip(self))]
    async fn handle_command(&mut self, command: StealerCommand) -> Result<(), AgentError> {
        let StealerCommand { client_id, command } = command;

        match command {
            Command::NewClient(daemon_tx, protocol_version) => {
                self.new_client(client_id, daemon_tx, protocol_version)
            }
            Command::ConnectionUnsubscribe(connection_id) => {
                self.connection_unsubscribe(connection_id)
            }
            Command::PortSubscribe(port_steal) => {
                self.port_subscribe(client_id, port_steal).await?
            }
            Command::PortUnsubscribe(port) => self.port_unsubscribe(client_id, port).await?,
            Command::ClientClose => self.close_client(client_id).await?,
            Command::ResponseData(tcp_data) => self.forward_data(tcp_data).await?,
            Command::HttpResponse(response) => self.http_response(response).await,
            Command::SwitchProtocolVersion(version) => {
                self.switch_protocol_version(client_id, version)
            }
        }

        Ok(())
    }
}
