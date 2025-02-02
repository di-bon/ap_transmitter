use std::collections::HashSet;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime};
use assembler::Assembler;
use assembler::naive_assembler::NaiveAssembler;
use crossbeam_channel::{Receiver, Sender};
use messages::{Message, MessageUtilities};
use messages::node_event::NodeEvent;
use wg_2024::network::{NodeId, SourceRoutingHeader};
use wg_2024::packet::{Fragment, Packet, PacketType};
use ap_sc_notifier::SimulationControllerNotifier;
use crate::transmitter::{TransmissionHandlerCommand, TransmissionHandlerEvent};
use crate::transmitter::gateway::Gateway;
use crate::transmitter::network_controller::NetworkController;

/// A `TransmissionHandler` struct that will transmit a `Message`, fragmenting it into `Fragment`s,
/// creating the required `Packet`s and sending them via the `Gateway`
pub struct TransmissionHandler {
    message: Message,
    fragments: Vec<Fragment>,
    session_id: u64,
    gateway: Arc<Gateway>,
    network_controller: Arc<NetworkController>,
    destination: NodeId,
    command_rx: Receiver<TransmissionHandlerCommand>,
    received_acks: HashSet<u64>,
    transmission_handler_event_tx: Sender<TransmissionHandlerEvent>,
    simulation_controller_notifier: Arc<SimulationControllerNotifier>,
    backoff_time: Duration, // the time to wait before trying again to find a new SourceRoutingHeader if the previous time there was no known path
    last_header_update_time: SystemTime,
}

impl TransmissionHandler {
    pub fn new(
        message: Message,
        gateway: Arc<Gateway>,
        network_controller: Arc<NetworkController>,
        command_rx: Receiver<TransmissionHandlerCommand>,
        transmission_handler_event_tx: Sender<TransmissionHandlerEvent>,
        simulation_controller_notifier: Arc<SimulationControllerNotifier>,
        backoff_time: Duration,
    ) -> Self {
        let fragments = NaiveAssembler::disassemble(&message.stringify().into_bytes());
        let session_id = message.session_id;
        let destination = message.destination;
        Self {
            message,
            fragments,
            session_id,
            gateway,
            network_controller,
            destination,
            command_rx,
            received_acks: HashSet::new(),
            transmission_handler_event_tx,
            simulation_controller_notifier,
            backoff_time,
            last_header_update_time: SystemTime::UNIX_EPOCH // Set last update to 1970-01-01 00:00:00 UTC to make sure that the header will be updated on the first TransmissionHandlerCommand::UpdateHeader received
        }
    }

    // Basic version: send all the fragments all at once, then wait for commands, exit when receiving an ACK for each fragment
    // Refined version: use a sliding window (using AIMD? (i.e. Additive Increase Multiplicative Decrease)) to send the fragments
    pub fn run(&mut self) {
        let mut source_routing_header = self.find_new_routing_header();

        let event = NodeEvent::StartingMessageTransmission(self.message.clone());
        self.simulation_controller_notifier.send_event(event);

        // Send all packets at once
        for fragment in &self.fragments {
            let packet = self.create_packet_for_fragment(fragment.clone(), source_routing_header.clone());
            self.gateway.forward(packet);
        };

        // wait for commands from transmitter
        loop {
            let command = self.command_rx.recv();
            if let Ok(command) = command {
                log::info!("Received command {command:?}");
                match command {
                    TransmissionHandlerCommand::Resend(fragment_index) => {
                        self.process_resend_command(fragment_index, &mut source_routing_header);
                    },
                    TransmissionHandlerCommand::Confirmed(fragment_index) => {
                        if self.process_confirmed_command(fragment_index) {
                            let event = NodeEvent::MessageSentSuccessfully(self.message.clone());
                            self.simulation_controller_notifier.send_event(event);
                            break
                        }
                    },
                    TransmissionHandlerCommand::UpdateHeader => {
                        self.process_update_header_command(&mut source_routing_header);
                    },
                    TransmissionHandlerCommand::Quit => {
                        break;
                    },
                }
            } else {
                log::error!("Error while receiving commands from transmitter");
                panic!("Error while receiving commands from transmitter");
            }
        }

        let event = TransmissionHandlerEvent::TransmissionCompleted(self.session_id);
        self.notify_transmitter(event);

        log::info!("Transmission handler for session {} terminated", self.session_id);
    }

    fn process_resend_command(&self, fragment_index: u64, source_routing_header: &mut SourceRoutingHeader) {
        #[allow(clippy::cast_possible_truncation)]
        let fragment = self.fragments.get(fragment_index as usize);
        match fragment {
            Some(fragment) => {
                let packet = self.create_packet_for_fragment(fragment.clone(), source_routing_header.clone());
                self.gateway.forward(packet);
            },
            None => {
                log::warn!("TransmissionHandler for session {} received a command {:?} with fragment index {fragment_index} out of bounds", self.session_id, TransmissionHandlerCommand::Resend(fragment_index));
            }
        }
    }

    fn process_confirmed_command(&mut self, fragment_index: u64) -> bool {
        self.received_acks.insert(fragment_index);
        self.received_acks.len() == self.fragments.len()
    }

    fn process_update_header_command(&mut self, source_routing_header: &mut SourceRoutingHeader) {
        match self.last_header_update_time.elapsed() {
            Ok(elapsed) => {
                if elapsed > Duration::from_millis(100) {
                    *source_routing_header = self.find_new_routing_header();
                    self.last_header_update_time = SystemTime::now();
                }
            },
            Err(err) => {
                log::error!("{}", err.to_string());
                panic!("{}", err.to_string())
            }
        }
    }

    fn notify_transmitter(&self, event: TransmissionHandlerEvent) {
        log::info!("Transmission handler for session {} is sending {:?} to transmitter", self.session_id, event);
        match self.transmission_handler_event_tx.send(event) {
            Ok(()) => {
                log::info!("Transmission handler for session {} sent a TransmissionHandlerEvent to transmitter", self.session_id);
            }
            Err(err) => {
                log::warn!("Transmission handler for session {} cannot send TransmissionHandlerEvent messages to transmitter. Error: {err:?}", self.session_id);
            }
        }
    }

    fn create_packet_for_fragment(&self, fragment: Fragment, source_routing_header: SourceRoutingHeader) -> Packet {
        Packet {
            routing_header: source_routing_header,
            session_id: self.session_id,
            pack_type: PacketType::MsgFragment(fragment),
        }
    }

    fn find_new_routing_header(&self) -> SourceRoutingHeader {
        loop {
            let hops = self.network_controller.get_path(self.destination);
            if let Some(hops) = hops {
                let source_routing_header = SourceRoutingHeader {
                    hop_index: 0,
                    hops,
                };
                return source_routing_header;
            }
            thread::sleep(self.backoff_time);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use crossbeam_channel::unbounded;
    use super::*;
    use messages::{ChatResponse, Message, MessageType, ResponseType};
    use messages::TextResponse::Text;
    use ntest::timeout;
    use wg_2024::packet::{FloodResponse, NodeType, Packet, PacketType};
    use crate::transmitter::LogicCommand;

    fn create_transmission_handler(message: &Message, node_id: NodeId, node_type: NodeType, destination_node_id: NodeId, paths: Vec<FloodResponse>, backoff_time: Duration) -> (TransmissionHandler, Receiver<Packet>, Receiver<NodeEvent>, Sender<TransmissionHandlerCommand>, Receiver<LogicCommand>) {
        let (simulation_controller_tx, simulation_controller_rx) = unbounded::<NodeEvent>();
        let simulation_controller_notifier = SimulationControllerNotifier::new(simulation_controller_tx);
        let simulation_controller_notifier = Arc::new(simulation_controller_notifier);

        let mut connected_drones = HashMap::new();

        let (drone_tx, drone_rx) = unbounded::<Packet>();
        connected_drones.insert(1, drone_tx);

        let (gateway_to_transmitter_tx, gateway_to_transmitter_rx) = unbounded();

        let gateway = Gateway::new(0, connected_drones, gateway_to_transmitter_tx, simulation_controller_notifier.clone());
        let gateway = Arc::new(gateway);

        let (command_tx, command_rx) = unbounded::<TransmissionHandlerCommand>();
        let network_controller = NetworkController::new(node_id, node_type, gateway.clone(), simulation_controller_notifier.clone());
        let network_controller = Arc::new(network_controller);
        let (transmission_handler_event_tx, transmission_handler_event_rx) = unbounded::<TransmissionHandlerEvent>();

        for path in paths {
            network_controller.update_from_flood_response(&path);
            let _ = simulation_controller_rx.recv().unwrap();
        }

        let transmission_handler = TransmissionHandler::new(
            message.clone(),
            gateway,
            network_controller,
            command_rx,
            transmission_handler_event_tx,
            simulation_controller_notifier.clone(),
            backoff_time,
        );

        (transmission_handler, drone_rx, simulation_controller_rx, command_tx, gateway_to_transmitter_rx)
    }

    #[test]
    fn initialize() {
        let message = Message {
            source: 0,
            destination: 1,
            session_id: 0,
            content: MessageType::Response(ResponseType::ChatResponse(ChatResponse::MessageSent)),
        };

        let paths = vec![];
        let (transmission_handler, drone_rx, simulation_controller_rx, command_tx, gateway_to_transmitter_rx) = create_transmission_handler(&message, 0, NodeType::Server, 1, paths, Duration::from_millis(2000));

        assert_eq!(message.destination, transmission_handler.destination);
        assert_eq!(message.session_id, transmission_handler.session_id);
        assert_eq!(message.content, transmission_handler.message.content);
    }

    #[test]
    fn check_create_packets() {
        let message = Message {
            source: 1,
            destination: 2,
            session_id: 51,
            content: MessageType::Response(ResponseType::ChatResponse(ChatResponse::MessageSent)),
        };

        let paths = vec![];
        let (transmission_handler, drone_rx, simulation_controller_rx, command_tx, gateway_to_transmitter_rx) = create_transmission_handler(&message, 0, NodeType::Server, 1, paths, Duration::from_millis(2000));

        let expected_packet = Packet {
            routing_header: Default::default(),
            session_id: 51,
            pack_type: PacketType::MsgFragment(transmission_handler.fragments[0].clone()),
        };
        assert_eq!(expected_packet, transmission_handler.create_packet_for_fragment(transmission_handler.fragments[0].clone(), SourceRoutingHeader { hop_index: 0, hops: vec![] }))
    }

    /*
    // To be removed
    #[test]
    fn update_source_routing_header() {
        let message = Message {
            source_id: 1,
            session_id: 51,
            content: MessageType::Response(ResponseType::ChatResponse(ChatResponse::MessageSent)),
        };
        let source_routing_header = SourceRoutingHeader {
            hop_index: 0,
            hops: vec![],
        };

        let paths = vec![];
        let (mut transmission_handler, drone_rx, simulation_controller_rx, command_tx) = create_transmission_handler(&message, 0, NodeType::Server, 1, paths);

        let expected_source_routing_header = SourceRoutingHeader {
            hop_index: 0,
            hops: vec![],
        };
        assert_eq!(transmission_handler.source_routing_header, expected_source_routing_header);

        let new_source_routing_header = SourceRoutingHeader {
            hop_index: 0,
            hops: vec![1, 2, 3, 4],
        };
        transmission_handler.update_source_routing_header(new_source_routing_header.clone());

        assert_eq!(transmission_handler.source_routing_header, new_source_routing_header);
    }
     */

    #[test]
    #[timeout(2000)]
    fn send_packets() {
        let session_id = 0;

        let message = Message {
            source: 0,
            destination: 1,
            session_id,
            content: MessageType::Response(ResponseType::TextResponse(Text("My super long text response .....................".to_string()))),
        };

        let paths = vec![
            FloodResponse {
                flood_id: 0,
                path_trace: vec![
                    (0, NodeType::Server),
                    (1, NodeType::Drone),
                ],
            }
        ];
        let (mut transmission_handler, drone_rx, simulation_controller_rx, command_tx, gateway_to_transmitter_rx) = create_transmission_handler(&message, 0, NodeType::Server, 1, paths, Duration::from_millis(2000));

        thread::spawn(move || {
            transmission_handler.run();
        });

        let _ = command_tx.send(TransmissionHandlerCommand::Resend(0)).unwrap();

        let fragments = NaiveAssembler::disassemble(&message.stringify().into_bytes());
        let expected_packets: Vec<Packet> = fragments.iter().map(|fragment: &Fragment|
            Packet {
                routing_header: SourceRoutingHeader { hop_index: 1, hops: vec![0, 1] },
                session_id,
                pack_type: PacketType::MsgFragment(fragment.clone()),
            }
        ).collect();

        for expected_packet in &expected_packets {
            let received = drone_rx.recv().unwrap();
            assert_eq!(received, *expected_packet);

            if let PacketType::MsgFragment(fragment) = received.pack_type {
                match command_tx.send(TransmissionHandlerCommand::Confirmed(fragment.fragment_index)) {
                    Ok(()) => (),
                    Err(err) => panic!("Cannot communicate with transmission handler"),
                }
            } else {
                panic!("Got wrong message type")
            }
        }

        let received = drone_rx.recv().unwrap();
        assert_eq!(received, expected_packets[0]);

        let event = simulation_controller_rx.recv().unwrap();
        assert!(matches!(event, NodeEvent::StartingMessageTransmission(_)));

        let event = simulation_controller_rx.recv().unwrap();
        assert!(matches!(event, NodeEvent::PacketSent(_)));

        let event = simulation_controller_rx.recv().unwrap();
        assert!(matches!(event, NodeEvent::PacketSent(_)));

        let event = simulation_controller_rx.recv().unwrap();
        assert!(matches!(event, NodeEvent::PacketSent(_)));

        let event = simulation_controller_rx.recv().unwrap();
        assert!(matches!(event, NodeEvent::MessageSentSuccessfully(_)));
    }

    #[test]
    #[timeout(2000)]
    fn check_quit_command() -> std::thread::Result<()> {
        let session_id = 0;

        let message = Message {
            source: 0,
            destination: 1,
            session_id,
            content: MessageType::Response(ResponseType::TextResponse(Text("My super long text response .....................".to_string()))),
        };

        let paths = vec![
            FloodResponse {
                flood_id: 0,
                path_trace: vec![
                    (0, NodeType::Server),
                    (1, NodeType::Drone),
                ],
            }
        ];
        let (mut transmission_handler, drone_rx, simulation_controller_rx, command_tx, gateway_to_transmitter_rx) = create_transmission_handler(&message, 0, NodeType::Server, 1, paths, Duration::from_millis(2000));

        let handle = thread::spawn(move || {
            transmission_handler.run();
        });

        let _ = command_tx.send(TransmissionHandlerCommand::Quit).unwrap();

        handle.join()
    }
}