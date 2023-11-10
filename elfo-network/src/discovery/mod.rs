use std::sync::Arc;

use eyre::{bail, eyre, Result, WrapErr};
use futures::StreamExt;
use tracing::{debug, error, info, warn};

use elfo_core::{
    message, msg, scope, Envelope, Message, MoveOwnership, RestartPolicy,
    _priv::{GroupNo, MessageKind},
    messages::ConfigUpdated,
    stream::Stream,
    Topology,
};

use crate::{
    codec::format::{NetworkAddr, NetworkEnvelope, NetworkEnvelopePayload},
    config::{CompressionAlgorithm, Transport},
    node_map::{NodeInfo, NodeMap},
    protocol::{internode, DataConnectionFailed, GroupInfo, HandleConnection},
    socket::{self, ReadError, Socket},
    NetworkContext,
};

/// Initial window size of every flow.
/// TODO: should be different for groups and actors.
const INITIAL_WINDOW_SIZE: i32 = 100_000;

#[message]
struct ConnectionEstablished {
    role: ConnectionRole,
    socket: MoveOwnership<Socket>,
    // `Some` only on the client side.
    transport: Option<Transport>,
}

#[message(part)]
enum ConnectionRole {
    // Only possible if this node is a server.
    Unknown,
    Control(internode::SwitchToControl),
    Data(internode::SwitchToData),
}

impl ConnectionRole {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Unknown => "Unknown",
            Self::Control(_) => "Control",
            Self::Data(_) => "Data",
        }
    }
}

#[message]
struct ConnectionAccepted {
    role: ConnectionRole,
    socket: MoveOwnership<Socket>,
    // `Some` only on the client side.
    transport: Option<Transport>,
}

#[message]
struct ConnectionRejected {
    error: String,
}

#[message]
struct ControlConnectionFailed {
    // `Some` only on the client side.
    transport: Option<Transport>,
}

pub(super) struct Discovery {
    ctx: NetworkContext,
    node_map: Arc<NodeMap>,
}

// TODO: detect duplicate nodes.
// TODO: discover tick.
// TODO: status of in-progress connections
// TODO: launch_id changed.

impl Discovery {
    pub(super) fn new(ctx: NetworkContext, topology: Topology) -> Self {
        Self {
            ctx,
            node_map: Arc::new(NodeMap::new(&topology)),
        }
    }

    pub(super) async fn main(mut self) -> Result<()> {
        // The default restart policy of this group is `never`, so override it.
        self.ctx.set_restart_policy(RestartPolicy::on_failures());

        self.listen().await?;
        self.discover();

        while let Some(envelope) = self.ctx.recv().await {
            msg!(match envelope {
                ConfigUpdated => {
                    // TODO: update listeners.
                    // TODO: stop discovering for removed transports.
                    // TODO: self.discover();
                }
                msg @ ConnectionEstablished => self.on_connection_established(msg),
                msg @ ConnectionAccepted => self.on_connection_accepted(msg),
                msg @ ConnectionRejected => self.on_connection_rejected(msg),
                msg @ DataConnectionFailed => {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    let role = ConnectionRole::Data(internode::SwitchToData {
                        my_group_no: msg.local,
                        your_group_no: msg.remote.1,
                        initial_window: INITIAL_WINDOW_SIZE,
                    });
                    self.open_connection(&msg.transport, role);
                }
                msg @ ControlConnectionFailed => {
                    if let Some(transport) = msg.transport {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        self.discover_one(transport);
                    }
                }
            });
        }

        Ok(())
    }

    fn get_capabilities(&self) -> socket::Capabilities {
        let mut capabilities = socket::Capabilities::empty();
        if self.ctx.config().compression.algorithm == CompressionAlgorithm::Lz4 {
            capabilities |= socket::Capabilities::LZ4;
        }
        capabilities
    }

    async fn listen(&mut self) -> Result<()> {
        let node_no = self.node_map.this.node_no;
        let launch_id = self.node_map.this.launch_id;
        let capabilities = self.get_capabilities();

        for transport in self.ctx.config().listen.clone() {
            let stream = socket::listen(&transport, node_no, launch_id, capabilities)
                .await
                .wrap_err_with(|| eyre!("cannot listen {}", transport))?
                .filter_map(move |socket| async move {
                    if socket.peer.node_no != node_no {
                        Some(socket)
                    } else {
                        info!(
                            message = "connection to self ignored",
                            socket = %socket.info,
                            peer = %socket.peer,
                        );
                        None
                    }
                })
                .map(|socket| ConnectionEstablished {
                    role: ConnectionRole::Unknown,
                    socket: socket.into(),
                    transport: None,
                });

            info!(
                message = "listening for connections",
                addr = %transport,
            );

            self.ctx.attach(Stream::from_futures03(stream));
        }

        Ok(())
    }

    fn discover(&mut self) {
        for transport in self.ctx.config().discovery.predefined.clone() {
            self.discover_one(transport);
        }
    }

    fn discover_one(&mut self, transport: Transport) {
        let msg = internode::SwitchToControl {
            groups: self.node_map.this.groups.clone(),
        };
        self.open_connection(&transport, ConnectionRole::Control(msg));
    }

    fn open_connection(
        &mut self,
        transport: &Transport,
        role: ConnectionRole,
    ) -> Stream<ConnectionEstablished> {
        let interval = self.ctx.config().discovery.attempt_interval;
        let transport = transport.clone();
        let node_no = self.node_map.this.node_no;
        let launch_id = self.node_map.this.launch_id;
        let capabilities = self.get_capabilities();

        let shift =
            std::time::Duration::from_millis(self.node_map.this.launch_id.into_bits() % 5000);

        self.ctx.attach(Stream::once(async move {
            loop {
                debug!(message = "connecting to peer", addr = %transport, role = ?role);

                match socket::connect(&transport, node_no, launch_id, capabilities).await {
                    Ok(socket) => {
                        if socket.peer.node_no != node_no {
                            break ConnectionEstablished {
                                role,
                                socket: socket.into(),
                                transport: Some(transport),
                            };
                        } else {
                            info!(
                                message = "connection to self ignored",
                                socket = %socket.info,
                                peer = %socket.peer,
                            );
                        }
                    }
                    Err(err) => {
                        // TODO: some errors should be logged as warnings.
                        info!(
                            message = "cannot connect",
                            error = %err,
                            addr = %transport,
                        );
                    }
                }

                let delay = interval + shift;

                // TODO: should we change trace_id?
                debug!(message = "retrying after some time", addr = %transport, delay = ?delay);
                tokio::time::sleep(delay).await;
            }
        }))
    }

    fn on_connection_established(&mut self, msg: ConnectionEstablished) {
        let socket = msg.socket.take().unwrap();
        let transport = msg.transport;

        info!(
            message = "new connection established",
            socket = %socket.info,
            peer = %socket.peer,
            role = msg.role.as_str(),
        );

        let node_map = self.node_map.clone();
        self.ctx.attach(Stream::once(async move {
            let info = socket.info.clone();
            let peer = socket.peer.clone();

            let result = accept_connection(socket, msg.role, transport, &node_map.this).await;
            match result {
                Ok(accepted) => Ok(accepted),
                Err(err) => {
                    let error = format!("{:#}", err);
                    warn!(
                        message = "new connection rejected",
                        socket = %info,
                        peer = %peer,
                        error = %error,
                    );
                    Err(ConnectionRejected { error })
                }
            }
        }));
    }

    fn on_connection_accepted(&mut self, msg: ConnectionAccepted) {
        let socket = msg.socket.take().unwrap();

        info!(
            message = "new connection accepted",
            socket = %socket.info,
            peer = %socket.peer,
            role = msg.role.as_str(),
        );

        match msg.role {
            ConnectionRole::Unknown => unreachable!(),
            ConnectionRole::Control(remote) => {
                {
                    let mut nodes = self.node_map.nodes.lock();
                    nodes.insert(
                        socket.peer.node_no,
                        NodeInfo {
                            node_no: socket.peer.node_no,
                            launch_id: socket.peer.launch_id,
                            groups: remote.groups.clone(),
                        },
                    );

                    // TODO: check launch_id.
                }

                self.control_maintenance(socket, msg.transport.clone());

                // Only initiator (client) can start new connections,
                // because he knows the transport address.
                let Some(transport) = msg.transport else {
                    return;
                };

                let this_node = &self.node_map.clone().this;

                // Open connections for all interesting pairs of groups.
                infer_connections(&remote.groups, &this_node.groups)
                    .map(|(remote_group_no, local_group_no)| (local_group_no, remote_group_no))
                    .chain(infer_connections(&this_node.groups, &remote.groups))
                    .collect::<Vec<_>>()
                    .into_iter()
                    .for_each(|(local_group_no, remote_group_no)| {
                        // TODO: save stream to cancel later.
                        // TODO: connect without DNS resolving here.
                        self.open_connection(
                            &transport,
                            ConnectionRole::Data(internode::SwitchToData {
                                my_group_no: local_group_no,
                                your_group_no: remote_group_no,
                                initial_window: INITIAL_WINDOW_SIZE,
                            }),
                        );
                    });

                // TODO: start ping-pong process on the socket.
            }
            ConnectionRole::Data(remote) => {
                let local_group_name = self
                    .node_map
                    .this
                    .groups
                    .iter()
                    .find(|g| g.group_no == remote.your_group_no)
                    .map(|g| g.name.clone());

                let remote_group_name = self
                    .node_map
                    .nodes
                    .lock()
                    .get(&socket.peer.node_no)
                    .and_then(|n| {
                        n.groups
                            .iter()
                            .find(|g| g.group_no == remote.my_group_no)
                            .map(|g| g.name.clone())
                    });

                let (local_group_name, remote_group_name) =
                    ward!(local_group_name.zip(remote_group_name), {
                        // TODO: it should be error once connection manager is implemented.
                        info!("control and data connections contradict each other");
                        return;
                    });

                let res = self.ctx.try_send_to(
                    self.ctx.group(),
                    HandleConnection {
                        local: GroupInfo {
                            node_no: self.node_map.this.node_no,
                            group_no: remote.your_group_no,
                            group_name: local_group_name,
                        },
                        remote: GroupInfo {
                            node_no: socket.peer.node_no,
                            group_no: remote.my_group_no,
                            group_name: remote_group_name,
                        },
                        transport: msg.transport.clone(),
                        socket: socket.into(),
                        initial_window: remote.initial_window,
                    },
                );

                if let Err(err) = res {
                    error!(message = "cannot start connection handler", error = %err);
                    // TODO: something else?
                }
            }
        }
    }

    fn on_connection_rejected(&mut self, _msg: ConnectionRejected) {
        // TODO: something else? Retries?
    }

    fn control_maintenance(&mut self, mut socket: Socket, transport: Option<Transport>) {
        self.ctx.attach(Stream::once(async move {
            // TODO: graceful termination.
            let err = control_maintenance(&mut socket).await.unwrap_err();

            info!(
                message = "control connection closed",
                socket = %socket.info,
                peer = %socket.peer,
                reason = %err,
            );

            ControlConnectionFailed { transport }
        }));
    }
}

async fn accept_connection(
    mut socket: Socket,
    role: ConnectionRole,
    transport: Option<Transport>,
    this_node: &NodeInfo,
) -> Result<ConnectionAccepted> {
    let role = match role {
        ConnectionRole::Unknown => {
            msg!(match recv(&mut socket).await? {
                msg @ internode::SwitchToControl => {
                    let my_msg = internode::SwitchToControl {
                        groups: this_node.groups.clone(),
                    };
                    send_regular(&mut socket, my_msg).await?;
                    ConnectionRole::Control(msg)
                }
                msg @ internode::SwitchToData => {
                    let my_msg = internode::SwitchToData {
                        my_group_no: msg.your_group_no,
                        your_group_no: msg.my_group_no,
                        initial_window: INITIAL_WINDOW_SIZE,
                    };
                    send_regular(&mut socket, my_msg).await?;
                    ConnectionRole::Data(msg)
                }
                envelope =>
                    return Err(unexpected_message_error(
                        envelope,
                        &["SwitchToControl", "SwitchToData"]
                    )),
            })
        }
        ConnectionRole::Control(msg) => {
            send_regular(&mut socket, msg).await?;
            let msg = recv_regular::<internode::SwitchToControl>(&mut socket).await?;
            ConnectionRole::Control(msg)
        }
        ConnectionRole::Data(msg) => {
            send_regular(&mut socket, msg).await?;
            let msg = recv_regular::<internode::SwitchToData>(&mut socket).await?;
            ConnectionRole::Data(msg)
        }
    };

    Ok(ConnectionAccepted {
        role,
        socket: socket.into(),
        transport,
    })
}

async fn control_maintenance(socket: &mut Socket) -> Result<()> {
    loop {
        send_regular(socket, internode::Ping { payload: 0 }).await?;
        recv_regular::<internode::Ping>(socket).await?;
        send_regular(socket, internode::Pong { payload: 0 }).await?;
        recv_regular::<internode::Pong>(socket).await?;
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
}

fn infer_connections<'a>(
    one: &'a [internode::GroupInfo],
    two: &'a [internode::GroupInfo],
) -> impl Iterator<Item = (GroupNo, GroupNo)> + 'a {
    one.iter().flat_map(move |o| {
        two.iter()
            .filter(move |t| o.interests.contains(&t.name))
            .map(move |t| (o.group_no, t.group_no))
    })
}

async fn send_regular<M: Message>(socket: &mut Socket, msg: M) -> Result<()> {
    let name = msg.name();
    let envelope = NetworkEnvelope {
        sender: NetworkAddr::NULL,    // doesn't matter
        recipient: NetworkAddr::NULL, // doesn't matter
        trace_id: scope::trace_id(),
        payload: NetworkEnvelopePayload::Regular {
            message: msg.upcast(),
        },
    };

    let send_future = socket.write.send(&envelope);
    send_future
        .await
        .wrap_err_with(|| eyre!("cannot send {}", name))
}

async fn recv(socket: &mut Socket) -> Result<Envelope> {
    let envelope = socket
        .read
        .recv()
        .await
        .map_err(|e| match e {
            ReadError::EnvelopeSkipped(..) => eyre!("failed to decode message"),
            ReadError::Fatal(report) => report,
        })
        .wrap_err("cannot receive a message")?
        .ok_or_else(|| eyre!("connection closed before receiving any messages"))?;

    let message = match envelope.payload {
        NetworkEnvelopePayload::Regular { message } => message,
        _ => bail!("unexpected message kind"),
    };

    // TODO: should we skip changing here if it's an initiator?
    scope::set_trace_id(envelope.trace_id);

    Ok(Envelope::new(
        message,
        MessageKind::Regular {
            sender: envelope.sender.into_remote(),
        },
    ))
}

async fn recv_regular<M: Message>(socket: &mut Socket) -> Result<M> {
    msg!(match recv(socket).await? {
        msg @ M => Ok(msg),
        envelope => Err(unexpected_message_error(
            envelope,
            &[&elfo_core::dumping::extract_name_by_type::<M>().to_string()]
        )),
    })
}

fn unexpected_message_error(envelope: Envelope, expected: &[&str]) -> eyre::Report {
    eyre!(
        "unexpected message: {}, expected: {}",
        envelope.message().name(),
        expected.join(" or "),
    )
}
