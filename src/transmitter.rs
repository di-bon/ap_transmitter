use crate::transmitter::gateway::Gateway;
use crate::transmitter::network_controller::NetworkController;
use crate::transmitter::single_packet_transmission_handler::SinglePacketTransmissionHandler;
use crate::transmitter::transmission_handler::{
    Command as TransmissionHandlerCommand, Event as TransmissionHandlerEvent, TransmissionHandler,
};
use ap_sc_notifier::SimulationControllerNotifier;
use crossbeam_channel::{select_biased, unbounded, Receiver, Sender};
use messages::Message;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use std::{panic, thread};
use wg_2024::controller::DroneCommand;
use wg_2024::network::NodeId;
use wg_2024::packet::{
    Ack, FloodRequest, FloodResponse, Nack, NackType, NodeType, Packet, PacketType,
};

mod gateway;
mod network_controller;
mod single_packet_transmission_handler;
mod transmission_handler;

#[derive(Debug)]
pub struct Transmitter {
    node_id: NodeId,
    node_type: NodeType,
    listener_to_transmitter_rx: Receiver<PacketCommand>,
    gateway_to_transmitter_rx: Receiver<PacketCommand>,
    logic_to_transmitter_rx: Receiver<Message>,
    network_controller: Arc<NetworkController>,
    transmission_handlers: HashMap<u64, Sender<TransmissionHandlerCommand>>,
    transmission_handler_event_rx: Receiver<TransmissionHandlerEvent>,
    transmission_handler_event_tx: Sender<TransmissionHandlerEvent>, // stores the channel to pass to every transmission handler to send TransmissionHandlerEvent
    gateway: Arc<Gateway>,
    simulation_controller_notifier: Arc<SimulationControllerNotifier>,
    command_rx: Receiver<Command>,
    last_flood_timestamp: SystemTime,
    flood_interval: Duration,
    logic_to_transmitter_drone_command_rx: Receiver<DroneCommand>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PacketCommand {
    SendAckFor {
        session_id: u64,
        fragment_index: u64,
        destination: NodeId,
    },
    ForwardAckTo {
        session_id: u64,
        ack: Ack,
        source: NodeId,
    },
    ProcessNack {
        session_id: u64,
        nack: Nack,
        source: NodeId,
    },
    ProcessFloodRequest(FloodRequest),
    ProcessFloodResponse(FloodResponse),
    SendNack {
        session_id: u64,
        nack: Nack,
        destination: NodeId,
    },
}

impl PartialEq for Transmitter {
    fn eq(&self, other: &Self) -> bool {
        self.node_id == other.node_id
            && self.network_controller == other.network_controller
            && self
                .transmission_handlers
                .keys()
                .eq(other.transmission_handlers.keys())
            && self.gateway.eq(&other.gateway)
    }
}

#[derive(Debug, Clone)]
pub enum Command {
    Quit,
}

impl Transmitter {
    /// Returns a new instance of `Transmitter`
    #[must_use]
    pub fn new(
        node_id: NodeId,
        node_type: NodeType,
        listener_rx: Receiver<PacketCommand>,
        server_logic_rx: Receiver<Message>,
        connected_drones: HashMap<NodeId, Sender<Packet>>,
        simulation_controller_notifier: Arc<SimulationControllerNotifier>,
        transmitter_command_rx: Receiver<Command>,
        flood_interval: Duration,
        server_to_transmitter_drone_command_rx: Receiver<DroneCommand>,
    ) -> Self {
        let (gateway_to_transmitter_tx, gateway_to_transmitter_rx) = unbounded();
        let gateway = Gateway::new(
            node_id,
            connected_drones,
            gateway_to_transmitter_tx,
            simulation_controller_notifier.clone(),
        );
        let gateway = Arc::new(gateway);

        let (transmission_handler_event_tx, transmission_handler_event_rx) =
            unbounded::<TransmissionHandlerEvent>();

        Self {
            node_id,
            node_type,
            listener_to_transmitter_rx: listener_rx,
            gateway_to_transmitter_rx,
            logic_to_transmitter_rx: server_logic_rx,
            network_controller: Arc::new(NetworkController::new(
                node_id,
                node_type,
                gateway.clone(),
                simulation_controller_notifier.clone(),
            )),
            transmission_handlers: HashMap::new(),
            transmission_handler_event_tx,
            transmission_handler_event_rx,
            gateway,
            simulation_controller_notifier,
            command_rx: transmitter_command_rx,
            last_flood_timestamp: SystemTime::UNIX_EPOCH,
            flood_interval,
            logic_to_transmitter_drone_command_rx: server_to_transmitter_drone_command_rx,
        }
    }

    /// Returns the value of `self.node_id`
    #[must_use]
    pub fn get_node_id(&self) -> NodeId {
        self.node_id
    }

    /// Starts the `Transmitter`, allowing it to process any message sent to it
    /// # Panics
    /// - Panics if the transmitter end of one of the `Receiver` channels gets unexpectedly dropped
    /// - Panics if an unsupported `DroneCommand` is received
    pub fn run(&mut self) {
        panic::set_hook(Box::new(|info| {
            let panic_msg = format!("Panic occurred: {info}");
            log::error!("{panic_msg}");
            eprintln!("{panic_msg}");
        }));

        loop {
            // Periodically flood the network
            self.flood_if_enough_time_elapsed();
            select_biased! {
                recv(self.logic_to_transmitter_drone_command_rx) -> drone_command => {
                    if let Ok(drone_command) = drone_command {
                        match drone_command {
                            DroneCommand::AddSender(node_id, channel) => {
                                self.network_controller.insert_neighbor(node_id);
                                self.gateway.add_neighbor(node_id, channel);
                            },
                            DroneCommand::RemoveSender(node_id) => {
                                self.network_controller.delete_neighbor_edge(node_id);
                                self.gateway.remove_neighbor(node_id);
                            },
                            DroneCommand::SetPacketDropRate(_)
                            | DroneCommand::Crash => {
                                panic!("Received unsupported {drone_command:?}");
                            },
                        }
                    } else {
                        panic!("Error while receiving DroneCommand from server");
                    }
                },
                recv(self.command_rx) -> command => {
                    if let Ok(command) = command {
                        match command {
                            Command::Quit => {
                                for session_id in self.transmission_handlers.keys() {
                                    let command = TransmissionHandlerCommand::Quit;
                                    self.send_transmission_handler_command(*session_id, command, self.node_id);
                                }
                                break
                            },
                        }
                    }
                    panic!("Error while receiving Command");
                },
                recv(self.listener_to_transmitter_rx) -> command => {
                    if let Ok(command) = command {
                        self.process_packet_command(command);
                    } else {
                        panic!("Error while receiving from listener_channel");
                    }
                },
                recv(self.gateway_to_transmitter_rx) -> command => {
                    if let Ok(command) = command {
                        self.process_gateway_command(command);
                    } else {
                        panic!("Error while receiving from gateway")
                    }
                },
                recv(self.logic_to_transmitter_rx) -> message_data => {
                    if let Ok(message) = message_data {
                        self.process_high_level_message(message);
                    } else {
                        panic!("Error while receiving from server_logic")
                    }
                },
                recv(self.transmission_handler_event_rx) -> event => {
                    if let Ok(event) = event {
                        let TransmissionHandlerEvent::TransmissionCompleted(session_id) = event;
                        self.transmission_handlers.remove(&session_id);
                    }
                },
            }
        }
    }

    /// Floods the network if the last flooding was done at least `self.flood_interval` time ago
    /// Note: the previous known network gets deleted
    /// # Panics
    /// Panics if `SystemTime::elapsed(&self)` fails
    fn flood_if_enough_time_elapsed(&mut self) {
        match self.last_flood_timestamp.elapsed() {
            Ok(elapsed) => {
                if elapsed >= self.flood_interval {
                    log::info!("Flooding the network");
                    self.last_flood_timestamp = SystemTime::now();
                    self.network_controller.flood_network();
                }
            }
            Err(err) => {
                log::error!("{}", err.to_string());
                panic!("{}", err.to_string())
            }
        }
    }

    /// Processes a `Message` received from the logic channel
    /// # Panics
    /// Panics if a new thread cannot be created
    fn process_high_level_message(&mut self, message: Message) {
        let (command_tx, command_rx) = unbounded::<TransmissionHandlerCommand>();

        let session_id = message.session_id; // TODO: or should be a random number?
        let mut transmission_handler = TransmissionHandler::new(
            message,
            self.gateway.clone(),
            self.network_controller.clone(),
            command_rx,
            self.transmission_handler_event_tx.clone(),
            self.simulation_controller_notifier.clone(),
            Duration::from_millis(2000),
        );

        thread::Builder::new()
            .name(format!("transmission_handler_{session_id}"))
            .spawn(move || {
                transmission_handler.run();
            })
            .unwrap_or_else(|_| panic!("Cannot create 'transmission_handler_{session_id}' thread"));

        self.transmission_handlers.insert(session_id, command_tx);
    }

    /// Processes a `TransmitterInternalCommand` received from `Gateway`
    /// # Panics
    /// - Panics if a `PacketCommand` different from `PacketCommand::SendNack` is received
    /// - Panics if `PacketCommand::SendNack` contains a NACK type different from `NackType::ErrorInRouting` or `NackType::UnexpectedRecipient`
    fn process_gateway_command(&self, command: PacketCommand) {
        if let PacketCommand::SendNack {
            session_id: _session_id,
            nack,
            destination: _destination,
        } = &command
        {
            match &nack.nack_type {
                NackType::ErrorInRouting(_) | NackType::UnexpectedRecipient(_) => {
                    self.process_packet_command(command);
                }
                nack => {
                    panic!("Unexpected NACK type ({nack:?}) received from gateway for error propagation");
                }
            }
        } else {
            panic!("Received unexpected PacketCommand from gateway: {command:?}");
        }
    }

    /// Sends a `TransmissionHandlerCommand` to the `TransmissionHandler` associated to the given `session_id`
    /// # Panics
    /// Panics if the communication to the required `TransmissionHandler` fails
    fn send_transmission_handler_command(
        &self,
        session_id: u64,
        command: TransmissionHandlerCommand,
        source: NodeId,
    ) {
        let Some(handler_channel) = self.transmission_handlers.get(&session_id) else {
            let command = PacketCommand::SendNack {
                session_id,
                nack: Nack {
                    fragment_index: 0, // Useless when sending UnexpectedRecipient, so set it to 0
                    nack_type: NackType::UnexpectedRecipient(self.node_id),
                },
                destination: source,
            };
            self.gateway.send_command_to_transmitter(command);

            return;
        };
        match handler_channel.send(command) {
            Ok(()) => {}
            Err(err) => {
                panic!(
                    "Cannot communicate with handler for session_id {session_id}. Error: {err:?}"
                );
            }
        }
    }

    /// Processes a `PacketCommand` based on the variant
    fn process_packet_command(&self, command: PacketCommand) {
        match command {
            PacketCommand::SendAckFor {
                session_id,
                fragment_index,
                destination,
            } => {
                let ack = Ack { fragment_index };
                let packet_type = PacketType::Ack(ack);
                self.spawn_single_packet_transmission_handler_thread(
                    session_id,
                    destination,
                    packet_type,
                );
            }
            PacketCommand::ForwardAckTo {
                session_id,
                ack,
                source,
            } => {
                let command = TransmissionHandlerCommand::Confirmed(ack.fragment_index);
                self.send_transmission_handler_command(session_id, command, source);
            }
            PacketCommand::ProcessNack {
                session_id,
                nack,
                source,
            } => {
                self.process_nack(session_id, &nack, source);
            }
            PacketCommand::SendNack {
                session_id,
                nack,
                destination,
            } => {
                let packet_type = PacketType::Nack(nack);
                self.spawn_single_packet_transmission_handler_thread(
                    session_id,
                    destination,
                    packet_type,
                );
            }
            PacketCommand::ProcessFloodRequest(flood_request) => {
                self.process_flood_request(flood_request);
            }
            PacketCommand::ProcessFloodResponse(flood_response) => {
                self.network_controller
                    .update_from_flood_response(&flood_response);
            }
        }
    }

    /// Spawns a `SinglePacketTransmissionHandler` for the `PacketType` argument
    /// # Panic
    /// Panics if a new thread cannot be spawned
    fn spawn_single_packet_transmission_handler_thread(
        &self,
        session_id: u64,
        destination: NodeId,
        packet_type: PacketType,
    ) {
        let handler = SinglePacketTransmissionHandler::new(
            packet_type,
            session_id,
            self.gateway.clone(),
            self.network_controller.clone(),
            Duration::from_millis(2000),
        );

        thread::Builder::new()
            .name(format!("single_packet_transmission_handler_{session_id}"))
            .spawn(move || {
                handler.send_packet(destination);
            })
            .unwrap_or_else(|_| panic!("Cannot create 'transmission_handler_{session_id}' thread"));
    }

    /// Processes a `FloodRequest`, sending a `FloodResponse` back to `initiator`
    fn process_flood_request(&self, flood_request: FloodRequest) {
        // check if, by any chance, the flood request initiator is self.node_id
        // if so, just ignore the flood request
        if flood_request.initiator_id == self.node_id {
            return;
        }

        // if a valid flood request is received, send a flood_response
        let mut path_trace = flood_request.path_trace;
        path_trace.push((self.node_id, self.node_type));
        path_trace.reverse();
        let flood_response = FloodResponse {
            flood_id: flood_request.flood_id,
            path_trace,
        };
        self.gateway.send_flood_response(flood_response);
    }

    /// Process a `Nack`, updating the `NetworkController` and, if needed, the required `TransmissionHandler`
    fn process_nack(&self, session_id: u64, nack: &Nack, source: NodeId) {
        self.network_controller.update_from_nack(nack, source);
        match nack.nack_type {
            NackType::Dropped => {
                self.network_controller.increment_dropped_count(source);

                let fragment_index = nack.fragment_index;
                let command = TransmissionHandlerCommand::Resend(fragment_index);
                self.send_transmission_handler_command(session_id, command, source);
            }
            NackType::ErrorInRouting(_next_hop) => {
                let fragment_index = nack.fragment_index;

                let command = TransmissionHandlerCommand::UpdateHeader;
                self.send_transmission_handler_command(session_id, command, source);

                let command = TransmissionHandlerCommand::Resend(fragment_index);
                self.send_transmission_handler_command(session_id, command, source);
            }
            NackType::UnexpectedRecipient(_) | NackType::DestinationIsDrone => {
                // don't do anything, case already handled by updating the network_controller
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(unused_variables)]
    #![allow(unused_mut)]

    use crate::transmitter::gateway::Gateway;
    use crate::transmitter::network_controller::NetworkController;
    use crate::transmitter::{Command, PacketCommand, TransmissionHandlerEvent, Transmitter};
    use ap_sc_notifier::SimulationControllerNotifier;
    use crossbeam_channel::{unbounded, Receiver, Sender};
    use messages::node_event::NodeEvent;
    use messages::{Message, MessageType, ResponseType, TextResponse};
    use ntest::timeout;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::thread;
    use std::thread::sleep;
    use std::time::{Duration, SystemTime};
    use wg_2024::controller::DroneCommand;
    use wg_2024::network::{NodeId, SourceRoutingHeader};
    use wg_2024::packet::{FloodResponse, Fragment, NodeType, Packet, PacketType};

    fn create_transmitter(
        node_id: NodeId,
        node_type: NodeType,
        connected_drones: HashMap<NodeId, Sender<Packet>>,
        flooding_interval: Duration,
        server_to_transmitter_drone_command_rx: Receiver<DroneCommand>,
    ) -> (
        Transmitter,
        Sender<PacketCommand>,
        Sender<Message>,
        Receiver<NodeEvent>,
        Sender<Command>,
    ) {
        let (listener_to_transmitter_tx, listener_to_transmitter_rx) = unbounded::<PacketCommand>();
        let (server_logic_to_transmitter_tx, server_logic_to_transmitter_rx) =
            unbounded::<Message>();

        let (simulation_controller_tx, simulation_controller_rx) = unbounded::<NodeEvent>();
        let simulation_controller_notifier =
            SimulationControllerNotifier::new(simulation_controller_tx);
        let simulation_controller_notifier = Arc::new(simulation_controller_notifier);

        let (transmitter_command_tx, transmitter_command_rx) = unbounded::<Command>();

        let transmitter = Transmitter::new(
            node_id,
            node_type,
            listener_to_transmitter_rx,
            server_logic_to_transmitter_rx,
            connected_drones,
            simulation_controller_notifier,
            transmitter_command_rx,
            flooding_interval,
            server_to_transmitter_drone_command_rx,
        );

        (
            transmitter,
            listener_to_transmitter_tx,
            server_logic_to_transmitter_tx,
            simulation_controller_rx,
            transmitter_command_tx,
        )
    }

    #[test]
    fn initialize() {
        let node_id = 0;
        let node_type = NodeType::Server;
        let mut connected_drones: HashMap<NodeId, Sender<Packet>> = HashMap::new();

        let drone_1_id = 1;
        let (drone_1_tx, drone_1_rx) = unbounded::<Packet>();
        connected_drones.insert(drone_1_id, drone_1_tx);

        let (server_to_transmitter_drone_command_tx, server_to_transmitter_drone_command_rx) =
            unbounded();

        let (
            transmitter,
            listener_to_transmitter_tx,
            server_logic_to_transmitter_tx,
            simulation_controller_rx,
            transmitter_command_tx,
        ) = create_transmitter(
            node_id,
            node_type,
            connected_drones,
            Duration::from_secs(60),
            server_to_transmitter_drone_command_rx,
        );

        let mut neighbors = HashMap::new();
        let (tx, rx) = unbounded::<Packet>();
        neighbors.insert(1, tx);

        let (simulation_controller_tx, simulation_controller_rx) = unbounded::<NodeEvent>();
        let simulation_controller_notifier =
            SimulationControllerNotifier::new(simulation_controller_tx);
        let simulation_controller_notifier = Arc::new(simulation_controller_notifier);

        let (gateway_to_transmitter_tx, gateway_to_transmitter_rx) = unbounded();

        let gateway = Gateway::new(
            node_id,
            neighbors,
            gateway_to_transmitter_tx,
            simulation_controller_notifier.clone(),
        );
        let gateway = Arc::new(gateway);

        let (listener_tx, listener_rx) = unbounded::<PacketCommand>();
        let (server_logic_tx, server_logic_rx) = unbounded::<Message>();
        let (
            transmitter_to_transmission_handler_event_tx,
            transmitter_to_transmission_handler_event_rx,
        ) = unbounded::<TransmissionHandlerEvent>();
        let (
            transmission_handler_to_transmitter_event_tx,
            transmission_handler_to_transmitter_event_rx,
        ) = unbounded::<TransmissionHandlerEvent>();

        let (simulation_controller_tx, simulation_controller_rx) = unbounded::<NodeEvent>();

        let (transmitter_command_tx, transmitter_command_rx) = unbounded::<Command>();

        let (server_to_transmitter_drone_command_tx, server_to_transmitter_drone_command_rx) =
            unbounded();

        let expected = Transmitter {
            node_id,
            node_type,
            listener_to_transmitter_rx: listener_rx,
            gateway_to_transmitter_rx,
            logic_to_transmitter_rx: server_logic_rx,
            network_controller: Arc::new(NetworkController::new(
                node_id,
                node_type,
                gateway.clone(),
                simulation_controller_notifier.clone(),
            )),
            transmission_handlers: Default::default(),
            transmission_handler_event_rx: transmission_handler_to_transmitter_event_rx,
            transmission_handler_event_tx: transmission_handler_to_transmitter_event_tx,
            gateway: gateway.clone(),
            simulation_controller_notifier: Arc::new(SimulationControllerNotifier::new(
                simulation_controller_tx,
            )),
            command_rx: transmitter_command_rx,
            last_flood_timestamp: SystemTime::UNIX_EPOCH,
            flood_interval: Duration::from_secs(60),
            logic_to_transmitter_drone_command_rx: server_to_transmitter_drone_command_rx,
        };

        assert_eq!(transmitter, expected);
    }

    #[test]
    #[timeout(2000)]
    fn check_process_high_level_message() {
        let node_id = 0;
        let node_type = NodeType::Server;
        let mut connected_drones: HashMap<NodeId, Sender<Packet>> = HashMap::new();

        let drone_1_id = 1;
        let (drone_1_tx, drone_1_rx) = unbounded::<Packet>();
        connected_drones.insert(drone_1_id, drone_1_tx);

        let (server_to_transmitter_drone_command_tx, server_to_transmitter_drone_command_rx) =
            unbounded();

        let (
            mut transmitter,
            listener_to_transmitter_tx,
            server_logic_to_transmitter_tx,
            simulation_controller_rx,
            transmitter_command_tx,
        ) = create_transmitter(
            node_id,
            node_type,
            connected_drones,
            Duration::from_secs(60),
            server_to_transmitter_drone_command_rx,
        );

        let message = Message {
            source: 0,
            destination: 1,
            session_id: 0,
            content: MessageType::Response(ResponseType::TextResponse(TextResponse::Text(
                "test".to_string(),
            ))),
        };

        transmitter.process_high_level_message(message.clone());

        // let received = simulation_controller_rx.recv().unwrap();
        //
        // assert!(matches!(received, NodeEvent::PacketSent(_)));

        assert_eq!(transmitter.transmission_handlers.len(), 1);
    }

    #[test]
    #[timeout(2000)]
    fn check_command() -> thread::Result<()> {
        let node_id = 0;
        let node_type = NodeType::Server;
        let mut connected_drones: HashMap<NodeId, Sender<Packet>> = HashMap::new();

        let drone_1_id = 1;
        let (drone_1_tx, drone_1_rx) = unbounded::<Packet>();
        connected_drones.insert(drone_1_id, drone_1_tx);

        let (server_to_transmitter_drone_command_tx, server_to_transmitter_drone_command_rx) =
            unbounded();

        let (
            mut transmitter,
            listener_to_transmitter_tx,
            server_logic_to_transmitter_tx,
            simulation_controller_rx,
            transmitter_command_tx,
        ) = create_transmitter(
            node_id,
            node_type,
            connected_drones,
            Duration::from_secs(60),
            server_to_transmitter_drone_command_rx,
        );

        let packet = Packet {
            routing_header: SourceRoutingHeader {
                hop_index: 1,
                hops: vec![1, 0],
            },
            session_id: 0,
            pack_type: PacketType::MsgFragment(Fragment {
                fragment_index: 0,
                total_n_fragments: 1,
                length: 128,
                data: [0; 128],
            }),
        };

        let handle = thread::spawn(move || {
            transmitter.run();
        });

        let flood_response = FloodResponse {
            flood_id: 0,
            path_trace: vec![(node_id, node_type), (1, NodeType::Drone)],
        };

        let flood_response_command = PacketCommand::ProcessFloodResponse(flood_response);
        listener_to_transmitter_tx
            .send(flood_response_command)
            .expect("Cannot communicate with transmitter");

        sleep(Duration::from_millis(200));

        let _ = transmitter_command_tx.send(Command::Quit);

        handle.join()
    }

    #[test]
    fn check_flood_response_processing() {
        let node_id = 0;
        let node_type = NodeType::Server;

        let (internal_transmitter_to_listener_tx, internal_transmitter_to_listener_rx) =
            unbounded::<Packet>();
        let (internal_listener_to_transmitter_tx, internal_listener_to_transmitter_rx) =
            unbounded::<PacketCommand>();
        let (internal_server_logic_to_transmitter_tx, internal_server_logic_to_transmitter_rx) =
            unbounded();

        let (simulation_controller_tx, simulation_controller_rx) = unbounded::<NodeEvent>();
        let simulation_controller_notifier =
            SimulationControllerNotifier::new(simulation_controller_tx);
        let simulation_controller_notifier = Arc::new(simulation_controller_notifier);

        let (transmitter_command_tx, transmitter_command_rx) = unbounded::<Command>();

        let connected_drones = HashMap::new();

        // let mut connected_drones = HashMap::new();
        // let (drone_1_tx, drone_1_rx) = unbounded::<Packet>();
        // connected_drones.insert(1, drone_1_tx);

        let (server_to_transmitter_drone_command_tx, server_to_transmitter_drone_command_rx) =
            unbounded();

        let mut transmitter = Transmitter::new(
            node_id,
            node_type,
            internal_listener_to_transmitter_rx,
            internal_server_logic_to_transmitter_rx,
            connected_drones,
            simulation_controller_notifier,
            transmitter_command_rx,
            Duration::from_secs(60),
            server_to_transmitter_drone_command_rx,
        );

        thread::spawn(move || {
            transmitter.run();
        });

        let flood_response = FloodResponse {
            flood_id: 0,
            path_trace: vec![
                (node_id, node_type),
                (1, NodeType::Drone),
                (2, NodeType::Client),
            ],
        };
        // let flood_response = Packet {
        //     routing_header: SourceRoutingHeader {
        //         hop_index: 0,
        //         hops: vec![],
        //     },
        //     session_id: 0,
        //     pack_type: PacketType::FloodResponse(flood_response),
        // };
        let flood_response_command = PacketCommand::ProcessFloodResponse(flood_response);
        internal_listener_to_transmitter_tx
            .send(flood_response_command)
            .expect("Cannot communicate with transmitter");

        // TODO: complete this
        // let expected = NodeEvent::KnownNetworkGraph(
        //
        // );

        let flood_response = FloodResponse {
            flood_id: 0,
            path_trace: vec![
                (node_id, node_type),
                (1, NodeType::Drone),
                (2, NodeType::Client),
            ],
        };
        // let flood_response = Packet {
        //     routing_header: SourceRoutingHeader {
        //         hop_index: 0,
        //         hops: vec![],
        //     },
        //     session_id: 0,
        //     pack_type: PacketType::FloodResponse(flood_response),
        // };
        let flood_response_command = PacketCommand::ProcessFloodResponse(flood_response);
        internal_listener_to_transmitter_tx
            .send(flood_response_command)
            .expect("Cannot communicate with transmitter");

        // TODO: complete this
        // let expected = NodeEvent::KnownNetworkGraph(
        //
        // );
    }
}
