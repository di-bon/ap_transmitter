use std::collections::HashMap;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use ap_sc_notifier::SimulationControllerNotifier;
use assembler::Assembler;
use assembler::naive_assembler::NaiveAssembler;
use crossbeam_channel::{unbounded, Receiver, Sender};
use messages::{Message, MessageType, MessageUtilities, ResponseType, TextResponse};
use messages::node_event::NodeEvent;
use ntest::timeout;
use wg_2024::network::{NodeId, SourceRoutingHeader};
use wg_2024::packet::{Ack, FloodRequest, FloodResponse, Nack, NackType, NodeType, Packet, PacketType};
use ap_transmitter::{Transmitter, Command, LogicCommand};

pub fn create_transmitter(
    node_id: NodeId,
    node_type: NodeType,
    connected_drones: HashMap<NodeId, Sender<Packet>>,
    simulation_controller_notifier: Arc<SimulationControllerNotifier>,
) -> (Transmitter, Sender<LogicCommand>, Sender<Message>, Sender<Command>) {

    let (listener_to_transmitter_tx, listener_to_transmitter_rx) = unbounded();
    let (logic_to_transmitter_tx, logic_to_transmitter_rx) = unbounded();
    let (transmitter_command_tx, transmitter_command_rx) = unbounded();

    let transmitter = Transmitter::new(
        node_id,
        node_type,
        listener_to_transmitter_rx,
        logic_to_transmitter_rx,
        connected_drones,
        simulation_controller_notifier,
        transmitter_command_rx,
    );

    (transmitter, listener_to_transmitter_tx, logic_to_transmitter_tx, transmitter_command_tx)
}

pub fn create_simulation_controller_notifier() -> (Arc<SimulationControllerNotifier>, Receiver<NodeEvent>) {
    let (simulation_controller_tx, simulation_controller_rx) = unbounded();

    let simulation_controller_notifier = SimulationControllerNotifier::new(simulation_controller_tx);
    let simulation_controller_notifier = Arc::new(simulation_controller_notifier);

    (simulation_controller_notifier, simulation_controller_rx)
}

#[test]
fn check_quit_command() -> thread::Result<()> {
    let node_id = 0;
    let node_type = NodeType::Server;

    let mut connected_drones = HashMap::new();
    let (drone_1_tx, drone_2_rx) = unbounded();
    connected_drones.insert(1, drone_1_tx);

    let (simulation_controller_notifier, simulation_controller_rx) = create_simulation_controller_notifier();

    let (mut transmitter,
        listener_to_transmitter_tx,
        logic_to_transmitter_tx,
        transmitter_command_tx) = create_transmitter(node_id, node_type, connected_drones, simulation_controller_notifier.clone());

    let handle = thread::spawn(move || {
        transmitter.run();
    });

    let _ = transmitter_command_tx.send(Command::Quit);

    handle.join()
}

#[test]
#[timeout(2000)]
fn check_message_handling_and_forwarding() {
    let node_id = 0;
    let node_type = NodeType::Server;

    let mut connected_drones = HashMap::new();
    let mut drones_rx = HashMap::new();
    let (drone_1_tx, drone_1_rx) = unbounded();
    connected_drones.insert(1, drone_1_tx);
    drones_rx.insert(1, drone_1_rx);

    let (simulation_controller_notifier, simulation_controller_rx) = create_simulation_controller_notifier();

    let (mut transmitter,
        listener_to_transmitter_tx,
        logic_to_transmitter_tx,
        transmitter_command_tx) = create_transmitter(node_id, node_type, connected_drones, simulation_controller_notifier.clone());

    let handle = thread::spawn(move || {
        transmitter.run();
    });

    thread::sleep(Duration::from_millis(20));

    let mut flood_id = 0;
    for (id, rx) in &drones_rx {
        let received = rx.recv().expect(&format!("Error while receiving a message in drone {id}"));
        assert!(matches!(received.pack_type, PacketType::FloodRequest(_)));
        if let PacketType::FloodRequest(flood_request) = received.pack_type {
            flood_id = flood_request.flood_id;
        }
        let event = simulation_controller_rx.recv().unwrap();
        assert!(matches!(event, NodeEvent::PacketSent(_)));
    }

    // send FloodResponse to update the network graph

    let flood_response = FloodResponse {
        flood_id,
        path_trace: vec![
            (node_id, node_type),
            (1, NodeType::Drone),
            (2, NodeType::Client),
        ],
    };
    // let flood_response = Packet {
    //     routing_header: SourceRoutingHeader { hop_index: 0, hops: vec![] },
    //     session_id: 0,
    //     pack_type: PacketType::FloodResponse(flood_response),
    // };

    let command = LogicCommand::ProcessFloodResponse(flood_response);

    listener_to_transmitter_tx.send(command).expect("Listener cannot communicate with transmitter");

    // send Message

    let session_id = 10;
    let destination_node_id = 2;
    let message = Message {
        source: node_id,
        destination: destination_node_id,
        session_id,
        content: MessageType::Response(
            ResponseType::TextResponse(
                TextResponse::Text(
                    "My text response".to_string()
                )
            )
        ),
    };

    logic_to_transmitter_tx.send(message.clone()).expect("Logic cannot communicate with transmitter");

    // check forwarded Packets

    let expected_fragments = NaiveAssembler::disassemble(&message.stringify().into_bytes());
    let expected_packets: Vec<Packet> = expected_fragments.iter().map(|fragment|
        Packet {
            routing_header: SourceRoutingHeader { hop_index: 1, hops: vec![node_id, 1, 2] },
            session_id,
            pack_type: PacketType::MsgFragment(fragment.clone()),
        }
    ).collect();

    for expected_packet in &expected_packets {
        let received = drones_rx.get(&1).unwrap().recv().unwrap();
        assert_eq!(received, *expected_packet);
    }

    // check NodeEvents sent to SimulationControllerNotifier

    let event = simulation_controller_rx.recv().unwrap();
    assert!(matches!(event, NodeEvent::KnownNetworkGraph { .. }));

    let event = simulation_controller_rx.recv().unwrap();
    assert!(matches!(event, NodeEvent::StartingMessageTransmission(_)));

    for i in 0..expected_packets.len() {
        let event = simulation_controller_rx.recv().unwrap();
        assert!(matches!(event, NodeEvent::PacketSent(_)));
    }

    // send ACKs back

    for (expected_fragment, expected_packet) in expected_fragments.iter().zip(expected_packets.iter()) {
        let fragment_index = expected_fragment.fragment_index;
        let ack = Ack {
            fragment_index,
        };

        // let ack = Packet {
        //     routing_header: SourceRoutingHeader { hop_index: 2, hops: expected_packet.routing_header.hops.iter().rev().copied().collect() },
        //     session_id,
        //     pack_type: PacketType::Ack(ack),
        // };

        let source = expected_packet.routing_header.source().unwrap();

        let command = LogicCommand::ForwardAckTo {
            session_id,
            ack,
            source,
        };

        listener_to_transmitter_tx.send(command).expect("Listener cannot communicate with transmitter");
    }

    // check NodeEvent::MessageSentSuccessfully sent to SimulationControllerNotifier

    let event = simulation_controller_rx.recv().unwrap();
    assert!(matches!(event, NodeEvent::MessageSentSuccessfully(_)));
}

#[test]
#[timeout(2000)]
fn check_unexpected_ack() {
    let node_id = 0;
    let node_type = NodeType::Server;

    let mut connected_drones = HashMap::new();
    let mut drones_rx = HashMap::new();
    let (drone_1_tx, drone_1_rx) = unbounded();
    connected_drones.insert(1, drone_1_tx);
    drones_rx.insert(1, drone_1_rx);

    let (simulation_controller_notifier, simulation_controller_rx) = create_simulation_controller_notifier();

    let (mut transmitter,
        listener_to_transmitter_tx,
        logic_to_transmitter_tx,
        transmitter_command_tx) = create_transmitter(node_id, node_type, connected_drones, simulation_controller_notifier.clone());

    let handle = thread::spawn(move || {
        transmitter.run();
    });

    thread::sleep(Duration::from_millis(20));

    let mut flood_id = 0;
    for (id, rx) in &drones_rx {
        let received = rx.recv().expect(&format!("Error while receiving a message in drone {id}"));
        assert!(matches!(received.pack_type, PacketType::FloodRequest(_)));
        if let PacketType::FloodRequest(flood_request) = received.pack_type {
            flood_id = flood_request.flood_id;
        }
        let event = simulation_controller_rx.recv().unwrap();
        assert!(matches!(event, NodeEvent::PacketSent(_)));
    }

    // send FloodResponse to update the network graph

    let flood_response = FloodResponse {
        flood_id: 0,
        path_trace: vec![
            (node_id, node_type),
            (1, NodeType::Drone),
            (2, NodeType::Client),
        ],
    };

    let command = LogicCommand::ProcessFloodResponse(flood_response);

    listener_to_transmitter_tx.send(command).expect("Listener cannot communicate with transmitter");

    // send ACK packet

    let fragment_index = 10;
    let ack = Ack {
        fragment_index,
    };

    let session_id = 10;

    let command = LogicCommand::ForwardAckTo {
        session_id,
        ack,
        source: 1,
    };

    listener_to_transmitter_tx.send(command).expect("Listener cannot communicate with transmitter");

    // check NACK response Packet

    let expected = Nack {
        fragment_index: 0,
        nack_type: NackType::UnexpectedRecipient(node_id),
    };
    let expected = Packet {
        routing_header: SourceRoutingHeader { hop_index: 1, hops: vec![node_id, 1] } ,
        session_id,
        pack_type: PacketType::Nack(expected),
    };

    let received = drones_rx.get(&1).unwrap().recv().unwrap();
    assert_eq!(received, expected);
}

#[test]
#[timeout(2000)]
fn check_flood_request_processing() {
    let node_id = 0;
    let node_type = NodeType::Server;

    let mut connected_drones = HashMap::new();
    let mut drones_rx = HashMap::new();
    let (drone_1_tx, drone_1_rx) = unbounded();
    connected_drones.insert(1, drone_1_tx);
    drones_rx.insert(1, drone_1_rx);

    let (simulation_controller_notifier, simulation_controller_rx) = create_simulation_controller_notifier();

    let (mut transmitter,
        listener_to_transmitter_tx,
        logic_to_transmitter_tx,
        transmitter_command_tx) = create_transmitter(node_id, node_type, connected_drones, simulation_controller_notifier.clone());

    let handle = thread::spawn(move || {
        transmitter.run();
    });

    thread::sleep(Duration::from_millis(20));

    let mut flood_id = 0;
    for (id, rx) in &drones_rx {
        let received = rx.recv().expect(&format!("Error while receiving a message in drone {id}"));
        assert!(matches!(received.pack_type, PacketType::FloodRequest(_)));
        if let PacketType::FloodRequest(flood_request) = received.pack_type {
            flood_id = flood_request.flood_id;
        }
        let event = simulation_controller_rx.recv().unwrap();
        assert!(matches!(event, NodeEvent::PacketSent(_)));
    }

    // send FloodResponse to update the network graph

    let flood_response = FloodResponse {
        flood_id,
        path_trace: vec![
            (node_id, node_type),
            (1, NodeType::Drone),
            (2, NodeType::Client),
        ],
    };

    // let flood_response = Packet {
    //     routing_header: SourceRoutingHeader { hop_index: 0, hops: vec![] },
    //     session_id: 0,
    //     pack_type: PacketType::FloodResponse(flood_response),
    // };

    let command = LogicCommand::ProcessFloodResponse(flood_response);

    listener_to_transmitter_tx.send(command).expect("Listener cannot communicate with transmitter");

    // send FloodRequest

    let flood_id = 7;
    let initiator_id = 3;

    let flood_request = FloodRequest {
        flood_id,
        initiator_id,
        path_trace: vec![
            (initiator_id, NodeType::Client),
            (1, NodeType::Drone),
        ],
    };

    // let flood_request = Packet {
    //     routing_header: SourceRoutingHeader { hop_index: 0, hops: vec![] },
    //     session_id,
    //     pack_type: PacketType::FloodRequest(flood_request),
    // };

    let command = LogicCommand::ProcessFloodRequest(flood_request);

    listener_to_transmitter_tx.send(command).expect("Listener cannot communicate with transmitter");

    // check forwarded Packets

    let expected_flood_response = FloodResponse {
        flood_id,
        path_trace: vec![
            (node_id, node_type),
            (1, NodeType::Drone),
            (initiator_id, NodeType::Client),
        ],
    };
    let expected_flood_response = Packet {
        routing_header: SourceRoutingHeader { hop_index: 0, hops: vec![] },
        session_id: 0,
        pack_type: PacketType::FloodResponse(expected_flood_response),
    };

    let received = drones_rx.get(&1).unwrap().recv().unwrap();
    assert_eq!(received, expected_flood_response);
}

#[test]
#[timeout(2000)]
fn check_nack_processing() {
    let node_id = 0;
    let node_type = NodeType::Server;

    let mut connected_drones = HashMap::new();
    let mut drones_rx = HashMap::new();
    let (drone_1_tx, drone_1_rx) = unbounded();
    connected_drones.insert(1, drone_1_tx);
    drones_rx.insert(1, drone_1_rx);

    let (simulation_controller_notifier, simulation_controller_rx) = create_simulation_controller_notifier();

    let (mut transmitter,
        listener_to_transmitter_tx,
        logic_to_transmitter_tx,
        transmitter_command_tx) = create_transmitter(node_id, node_type, connected_drones, simulation_controller_notifier.clone());

    let handle = thread::spawn(move || {
        transmitter.run();
    });

    thread::sleep(Duration::from_millis(20));

    let mut flood_id = 0;
    for (id, rx) in &drones_rx {
        let received = rx.recv().expect(&format!("Error while receiving a message in drone {id}"));
        assert!(matches!(received.pack_type, PacketType::FloodRequest(_)));
        if let PacketType::FloodRequest(flood_request) = received.pack_type {
            flood_id = flood_request.flood_id;
        }
        let event = simulation_controller_rx.recv().unwrap();
        assert!(matches!(event, NodeEvent::PacketSent(_)));
    }

    // send FloodResponses to update the network graph

    let flood_response = FloodResponse {
        flood_id,
        path_trace: vec![
            (node_id, node_type),
            (1, NodeType::Drone),
            (2, NodeType::Drone),
            (3, NodeType::Drone),
            (4, NodeType::Client),
        ],
    };

    let command = LogicCommand::ProcessFloodResponse(flood_response);
    listener_to_transmitter_tx.send(command).expect("Listener cannot communicate with transmitter");

    let event = simulation_controller_rx.recv().unwrap();
    assert!(matches!(event, NodeEvent::KnownNetworkGraph { .. }));

    let flood_response = FloodResponse {
        flood_id: 0,
        path_trace: vec![
            (node_id, node_type),
            (1, NodeType::Drone),
            (2, NodeType::Drone),
            (5, NodeType::Drone),
            (6, NodeType::Drone),
            (7, NodeType::Drone),
            (4, NodeType::Client),
        ],
    };

    let command = LogicCommand::ProcessFloodResponse(flood_response);
    listener_to_transmitter_tx.send(command).expect("Listener cannot communicate with transmitter");

    let event = simulation_controller_rx.recv().unwrap();
    assert!(matches!(event, NodeEvent::KnownNetworkGraph { .. }));

    // send fragments

    let session_id = 10;
    let destination_node_id = 4;
    let message = Message {
        source: node_id,
        destination: destination_node_id,
        session_id,
        content: MessageType::Response(
            ResponseType::TextResponse(
                TextResponse::Text(
                    "My text response".to_string()
                )
            )
        ),
    };

    logic_to_transmitter_tx.send(message.clone()).expect("Logic cannot communicate with transmitter");

    let event = simulation_controller_rx.recv().unwrap();
    assert!(matches!(event, NodeEvent::StartingMessageTransmission(_)));

    let fragments = NaiveAssembler::disassemble(&message.stringify().into_bytes());
    for _ in 0..fragments.len() {
        let event = simulation_controller_rx.recv().unwrap();
        assert!(matches!(event, NodeEvent::PacketSent(_)));
    }

    // send NACK
    thread::sleep(Duration::from_millis(50));

    let nack = Nack {
        fragment_index: 0,
        nack_type: NackType::ErrorInRouting(3),
    };
    let command = LogicCommand::ProcessNack { session_id, nack, source: 2 };
    listener_to_transmitter_tx.send(command).expect("Listener cannot communicate with transmitter");

    let event = simulation_controller_rx.recv().unwrap();
    assert!(matches!(event, NodeEvent::KnownNetworkGraph { .. }));

    let event = simulation_controller_rx.recv().unwrap();
    assert!(matches!(event, NodeEvent::PacketSent(_)));

    // check forwarded Packets

    for i in 0..fragments.len() {
        let received = drones_rx.get(&1).unwrap().recv().unwrap();
        assert!(matches!(received.pack_type, PacketType::MsgFragment(_)));
    }

    let received =  drones_rx.get(&1).unwrap().recv().unwrap();
    let expected_routing_header = SourceRoutingHeader {
        hop_index: 1,
        hops: vec![node_id, 1, 2, 5, 6, 7, 4],
    };

    assert_eq!(received.routing_header, expected_routing_header);

    let dropped = Nack {
        fragment_index: 0,
        nack_type: NackType::Dropped,
    };
    // let dropped = Packet {
    //     routing_header: SourceRoutingHeader {
    //         hop_index: 1,
    //         hops: vec![1, node_id],
    //     },
    //     session_id,
    //     pack_type: PacketType::Nack(dropped),
    // };

    let command = LogicCommand::ProcessNack {
        session_id,
        nack: dropped,
        source: 1,
    };

    listener_to_transmitter_tx.send(command).expect("Listener cannot communicate with transmitter");

    let expected = received.clone();
    let received =  drones_rx.get(&1).unwrap().recv().unwrap();

    assert_eq!(received, expected);
}