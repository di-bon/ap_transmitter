use std::collections::HashMap;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use crossbeam_channel::{select_biased, unbounded, Receiver, Sender};
use messages::Message;
use wg_2024::network::NodeId;
use wg_2024::packet::{Ack, FloodRequest, FloodResponse, Nack, NackType, NodeType, Packet, PacketType};
use ap_sc_notifier::SimulationControllerNotifier;
use crate::transmitter::network_controller::NetworkController;
use crate::transmitter::gateway::Gateway;
use crate::transmitter::single_packet_transmission_handler::SinglePacketTransmissionHandler;
use crate::transmitter::transmission_handler::TransmissionHandler;

mod network_controller;
mod gateway;
mod transmission_handler;
mod single_packet_transmission_handler;

#[derive(Debug)]
pub struct Transmitter {
    node_id: NodeId,
    listener_rx: Receiver<PacketCommand>,
    gateway_to_transmitter_rx: Receiver<PacketCommand>,
    server_logic_rx: Receiver<Message>,
    network_controller: Arc<NetworkController>,
    transmission_handlers: HashMap<u64, Sender<TransmissionHandlerCommand>>,
    transmission_handler_event_rx: Receiver<TransmissionHandlerEvent>,
    transmission_handler_event_tx: Sender<TransmissionHandlerEvent>,
    gateway: Arc<Gateway>,
    simulation_controller_notifier: Arc<SimulationControllerNotifier>,
    transmitter_command_rx: Receiver<Command>,
    last_flood_timestamp: SystemTime,
    flood_interval: Duration,
}

impl PartialEq for Transmitter {
    fn eq(&self, other: &Self) -> bool {
        self.node_id == other.node_id
            && self.network_controller == other.network_controller
            && self.transmission_handlers.keys().eq(other.transmission_handlers.keys())
            && self.gateway.eq(&other.gateway)
    }
}

#[derive(Debug, Clone)]
pub enum TransmissionHandlerCommand {
    Resend(u64),
    Confirmed(u64),
    Quit,
    UpdateHeader,
}

#[derive(Debug, Clone)]
enum TransmissionHandlerEvent {
    TransmissionCompleted(u64)
}

#[derive(Debug, Clone, PartialEq)]
pub enum PacketCommand {
    SendAckFor { session_id: u64, fragment_index: u64, destination: NodeId },
    ForwardAckTo { session_id: u64, ack: Ack, source: NodeId },
    ProcessNack { session_id: u64, nack: Nack, source: NodeId },
    ProcessFloodRequest(FloodRequest),
    ProcessFloodResponse(FloodResponse),
    SendNack { session_id: u64, nack: Nack, destination: NodeId },
}

#[derive(Debug, Clone)]
pub enum Command {
    Quit,
    AddNeighbor(NodeId, Sender<Packet>),
    RemoveNeighbor(NodeId),
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
    ) -> Self {
        let (gateway_to_transmitter_tx, gateway_to_transmitter_rx) = unbounded();
        let gateway = Gateway::new(node_id, connected_drones, gateway_to_transmitter_tx, simulation_controller_notifier.clone());
        let gateway = Arc::new(gateway);

        let (transmission_handler_event_tx, transmission_handler_event_rx) = unbounded::<TransmissionHandlerEvent>();

        Self {
            node_id,
            listener_rx,
            gateway_to_transmitter_rx,
            server_logic_rx,
            network_controller: Arc::new(NetworkController::new(node_id, node_type, gateway.clone(), simulation_controller_notifier.clone())),
            transmission_handlers: HashMap::new(),
            transmission_handler_event_tx,
            transmission_handler_event_rx,
            gateway,
            simulation_controller_notifier,
            transmitter_command_rx,
            last_flood_timestamp: SystemTime::UNIX_EPOCH,
            flood_interval
        }
    }

    /// Returns the value of `self.node_id`
    #[must_use]
    pub fn get_node_id(&self) -> NodeId {
        self.node_id
    }

    /// Starts the `Transmitter`, allowing it to process any message sent to it
    pub fn run(&mut self) {
        // when run is called, transmitter should instantaneously flood the network to discover routes
        // self.network_controller.flood_network();
        loop {
            self.flood_if_enough_time_elapsed();
            select_biased! {
                recv(self.transmitter_command_rx) -> command => {
                    if let Ok(command) = command {
                        match command {
                            Command::Quit => {
                                for session_id in self.transmission_handlers.keys() {
                                    let command = TransmissionHandlerCommand::Quit;
                                    self.send_transmission_handler_command(*session_id, command, self.node_id);
                                }
                                break
                            },
                            Command::AddNeighbor(node_id, channel) => {
                                self.network_controller.insert_neighbor(node_id);
                                self.gateway.add_neighbor(node_id, channel);
                            },
                            Command::RemoveNeighbor(node_id) => {
                                self.network_controller.delete_edge(node_id);
                                self.gateway.remove_neighbor(node_id);
                            },
                        }
                    }
                },
                recv(self.listener_rx) -> command => {
                    if let Ok(command) = command {
                        self.process_logic_command(command);
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
                recv(self.server_logic_rx) -> message_data => {
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

    fn flood_if_enough_time_elapsed(&mut self) {
        match self.last_flood_timestamp.elapsed() {
            Ok(elapsed) => {
                if elapsed > self.flood_interval {
                    self.last_flood_timestamp = SystemTime::now();
                    self.network_controller.flood_network();
                }
            },
            Err(err) => {
                log::error!("{}", err.to_string());
                panic!("{}", err.to_string())
            }
        }
    }

    /// Processes a `Message` received from the logic channel
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
            .unwrap();

        self.transmission_handlers.insert(session_id, command_tx);
    }

    /// Processes a `TransmitterInternalCommand` received from `Gateway`
    fn process_gateway_command(&self, command: PacketCommand) {
        if let PacketCommand::SendNack { session_id: _session_id, nack, destination: _destination } = &command {
            match &nack.nack_type {
                NackType::ErrorInRouting(_) | NackType::UnexpectedRecipient(_) => {
                    self.process_logic_command(command);
                },
                _ => {
                    log::error!("Unexpected NACK type received from gateway for error propagation");
                    panic!("Unexpected NACK type received from gateway for error propagation");
                },
            }
        } else {
            log::error!("Received unexpected command from gateway: {command:?}");
            panic!("Received unexpected command from gateway: {command:?}");
        }
    }

    /// Sends a `TransmissionHandlerCommand` to the `TransmissionHandler` associated to the given `session_id`
    fn send_transmission_handler_command(&self, session_id: u64, command: TransmissionHandlerCommand, source: NodeId) {
        let Some(handler_channel) = self.transmission_handlers.get(&session_id)
        else {
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
            Ok(()) => {},
            Err(err) => {
                log::error!("Cannot communicate with handler for session_id {session_id}. Error: {err:?}");
                panic!("Cannot communicate with handler for session_id {session_id}. Error: {err:?}");
            }
        }
    }

    /// Processes a `TransmitterInternalCommand`
    fn process_logic_command(&self, command: PacketCommand) {
        match command {
            PacketCommand::SendAckFor { session_id, fragment_index, destination} => {
                let ack = Ack {
                    fragment_index,
                };

                let packet_type = PacketType::Ack(ack);

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
                    .unwrap();
            }
            PacketCommand::ForwardAckTo { session_id, ack, source } => {
                let command = TransmissionHandlerCommand::Confirmed(ack.fragment_index);
                self.send_transmission_handler_command(session_id, command, source);
            }
            PacketCommand::ProcessNack { session_id, nack, source } => {
                self.process_nack(session_id, &nack, source);
            }
            PacketCommand::SendNack { session_id, nack, destination} => {
                let packet_type = PacketType::Nack(nack);

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
                    .unwrap();
            }
            PacketCommand::ProcessFloodRequest(flood_request) => {
                self.process_flood_request(flood_request);
            }
            PacketCommand::ProcessFloodResponse(flood_response) => {
                self.network_controller.update_from_flood_response(&flood_response);
            }
        }
    }

    fn process_flood_request(&self, flood_request: FloodRequest) {
        // check if, by any chance, the flood request initiator is self
        // if so, just ignore the flood request
        if flood_request.initiator_id == self.node_id {
            return;
        }

        // if a valid flood request is received, send a flood_response
        let mut path_trace = flood_request.path_trace;
        path_trace.push((self.node_id, NodeType::Server));
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
            },
            NackType::ErrorInRouting(_next_hop) => {
                let fragment_index = nack.fragment_index;

                let command = TransmissionHandlerCommand::UpdateHeader;
                self.send_transmission_handler_command(session_id, command, source);

                let command = TransmissionHandlerCommand::Resend(fragment_index);
                self.send_transmission_handler_command(session_id, command, source);
            },
            NackType::UnexpectedRecipient(_unexpected_recipient_id) => {
                // don't do anything, case already handled by updating the network_controller
            }
            NackType::DestinationIsDrone => {
                // don't do anything, case already handled by updating the network_controller
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(unused_variables)]

    use std::collections::HashMap;
    use std::sync::Arc;
    use std::thread;
    use std::thread::{sleep};
    use std::time::{Duration, SystemTime};
    use crossbeam_channel::{unbounded, Receiver, Sender};
    use messages::{Message, MessageType, ResponseType, TextResponse};
    use messages::node_event::NodeEvent;
    use ntest::timeout;
    use wg_2024::network::{NodeId, SourceRoutingHeader};
    use wg_2024::packet::{FloodResponse, Fragment, NodeType, Packet, PacketType};
    use ap_sc_notifier::SimulationControllerNotifier;
    use crate::transmitter::gateway::Gateway;
    use crate::transmitter::network_controller::NetworkController;
    use crate::transmitter::{TransmissionHandlerEvent, Transmitter, PacketCommand, Command};

    fn create_transmitter(node_id: NodeId, node_type: NodeType, connected_drones: HashMap<NodeId, Sender<Packet>>, flooding_interval: Duration)
                          -> (Transmitter, Sender<PacketCommand>, Sender<Message>, Receiver<NodeEvent>, Sender<Command>)
    {
        let (listener_to_transmitter_tx, listener_to_transmitter_rx) = unbounded::<PacketCommand>();
        let (server_logic_to_transmitter_tx, server_logic_to_transmitter_rx) = unbounded::<Message>();

        let (simulation_controller_tx, simulation_controller_rx) = unbounded::<NodeEvent>();
        let simulation_controller_notifier = SimulationControllerNotifier::new(simulation_controller_tx);
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
            flooding_interval
        );

        (transmitter, listener_to_transmitter_tx, server_logic_to_transmitter_tx, simulation_controller_rx, transmitter_command_tx)
    }

    #[test]
    fn initialize() {
        let node_id = 0;
        let node_type = NodeType::Server;
        let mut connected_drones: HashMap<NodeId, Sender<Packet>> = HashMap::new();

        let drone_1_id = 1;
        let (drone_1_tx, drone_1_rx) = unbounded::<Packet>();
        connected_drones.insert(drone_1_id, drone_1_tx);

        let (transmitter,
            listener_to_transmitter_tx,
            server_logic_to_transmitter_tx,
            simulation_controller_rx,
            transmitter_command_tx) = create_transmitter(node_id, node_type, connected_drones, Duration::from_secs(60));


        let mut neighbors = HashMap::new();
        let (tx, rx) = unbounded::<Packet>();
        neighbors.insert(1, tx);

        let (simulation_controller_tx, simulation_controller_rx) = unbounded::<NodeEvent>();
        let simulation_controller_notifier = SimulationControllerNotifier::new(simulation_controller_tx);
        let simulation_controller_notifier = Arc::new(simulation_controller_notifier);

        let (gateway_to_transmitter_tx, gateway_to_transmitter_rx) = unbounded();

        let gateway = Gateway::new(node_id, neighbors, gateway_to_transmitter_tx, simulation_controller_notifier.clone());
        let gateway = Arc::new(gateway);

        let (listener_tx, listener_rx) = unbounded::<PacketCommand>();
        let (server_logic_tx, server_logic_rx) = unbounded::<Message>();
        let (transmitter_to_transmission_handler_event_tx, transmitter_to_transmission_handler_event_rx) = unbounded::<TransmissionHandlerEvent>();
        let (transmission_handler_to_transmitter_event_tx, transmission_handler_to_transmitter_event_rx) = unbounded::<TransmissionHandlerEvent>();

        let (simulation_controller_tx, simulation_controller_rx) = unbounded::<NodeEvent>();

        let (transmitter_command_tx, transmitter_command_rx) = unbounded::<Command>();

        let expected = Transmitter {
            node_id,
            listener_rx,
            gateway_to_transmitter_rx,
            server_logic_rx,
            network_controller: Arc::new(NetworkController::new(node_id, node_type, gateway.clone(), simulation_controller_notifier.clone())),
            transmission_handlers: Default::default(),
            transmission_handler_event_rx: transmission_handler_to_transmitter_event_rx,
            transmission_handler_event_tx: transmission_handler_to_transmitter_event_tx,
            gateway: gateway.clone(),
            simulation_controller_notifier: Arc::new(SimulationControllerNotifier::new(simulation_controller_tx)),
            transmitter_command_rx,
            last_flood_timestamp: SystemTime::UNIX_EPOCH,
            flood_interval: Duration::from_secs(60),
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

        let (mut transmitter,
            listener_to_transmitter_tx,
            server_logic_to_transmitter_tx,
            simulation_controller_rx,
            transmitter_command_tx
        ) = create_transmitter(node_id, node_type, connected_drones, Duration::from_secs(60));

        let message = Message {
            source: 0,
            destination: 1,
            session_id: 0,
            content: MessageType::Response(
                ResponseType::TextResponse(
                    TextResponse::Text(
                        "test".to_string()
                    )
                )
            ),
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

        let (mut transmitter,
            listener_to_transmitter_tx,
            server_logic_to_transmitter_tx,
            simulation_controller_rx,
            transmitter_command_tx) = create_transmitter(node_id, node_type, connected_drones, Duration::from_secs(60));

        let packet = Packet {
            routing_header: SourceRoutingHeader { hop_index: 1, hops: vec![1, 0] },
            session_id: 0,
            pack_type: PacketType::MsgFragment( Fragment {
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
            path_trace: vec![
                (node_id, node_type),
                (1, NodeType::Drone),
            ],
        };

        let flood_response_command = PacketCommand::ProcessFloodResponse(flood_response);
        listener_to_transmitter_tx.send(flood_response_command).expect("Cannot communicate with transmitter");

        sleep(Duration::from_millis(200));

        transmitter_command_tx.send(Command::Quit);

        handle.join()
    }

    #[test]
    fn check_flood_response_processing() {
        let node_id = 0;
        let node_type = NodeType::Server;

        let (internal_transmitter_to_listener_tx, internal_transmitter_to_listener_rx) = unbounded::<Packet>();
        let (internal_listener_to_transmitter_tx, internal_listener_to_transmitter_rx) = unbounded::<PacketCommand>();
        let (internal_server_logic_to_transmitter_tx, internal_server_logic_to_transmitter_rx) = unbounded();

        let (simulation_controller_tx, simulation_controller_rx) = unbounded::<NodeEvent>();
        let simulation_controller_notifier = SimulationControllerNotifier::new(simulation_controller_tx);
        let simulation_controller_notifier = Arc::new(simulation_controller_notifier);

        let (transmitter_command_tx, transmitter_command_rx) = unbounded::<Command>();

        let connected_drones = HashMap::new();

        // let mut connected_drones = HashMap::new();
        // let (drone_1_tx, drone_1_rx) = unbounded::<Packet>();
        // connected_drones.insert(1, drone_1_tx);

        let mut transmitter = Transmitter::new(
            node_id,
            node_type,
            internal_listener_to_transmitter_rx,
            internal_server_logic_to_transmitter_rx,
            connected_drones,
            simulation_controller_notifier,
            transmitter_command_rx,
            Duration::from_secs(60),
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
        internal_listener_to_transmitter_tx.send(flood_response_command).expect("Cannot communicate with transmitter");

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
        internal_listener_to_transmitter_tx.send(flood_response_command).expect("Cannot communicate with transmitter");

        // TODO: complete this
        // let expected = NodeEvent::KnownNetworkGraph(
        //
        // );
    }
}