use crate::transmitter::PacketCommand;
use ap_sc_notifier::SimulationControllerNotifier;
use crossbeam_channel::{SendError, Sender};
use messages::node_event::NodeEvent;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use wg_2024::network::{NodeId, SourceRoutingHeader};
use wg_2024::packet::{FloodRequest, FloodResponse, Nack, NackType, Packet, PacketType};

#[derive(Debug)]
pub struct Gateway {
    node_id: NodeId,
    neighbors: RwLock<HashMap<NodeId, Sender<Packet>>>,
    gateway_to_transmitter_tx: Sender<PacketCommand>,
    simulation_controller_notifier: Arc<SimulationControllerNotifier>,
}

impl PartialEq<Self> for Gateway {
    fn eq(&self, other: &Self) -> bool {
        self.node_id == other.node_id && self.neighbors.read().unwrap().keys().eq(other.neighbors.read().unwrap().keys())
    }
}

impl Gateway {
    /// Returns a new instance of `Gateway`
    pub fn new(
        node_id: NodeId,
        neighbors: HashMap<NodeId, Sender<Packet>>,
        gateway_to_transmitter_tx: Sender<PacketCommand>,
        simulation_controller_notifier: Arc<SimulationControllerNotifier>,
    ) -> Self {
        let neighbors = RwLock::new(neighbors);
        Self {
            node_id,
            neighbors,
            gateway_to_transmitter_tx,
            simulation_controller_notifier,
        }
    }

    /// Sends a `FloodRequest` to every connected neighboring node
    pub fn send_flood_request(&self, flood_request: FloodRequest) {
        let packet = Packet {
            routing_header: SourceRoutingHeader {
                hop_index: 0,
                hops: vec![],
            },
            session_id: 0,
            pack_type: PacketType::FloodRequest(flood_request),
        };

        for (node_id, channel) in self.neighbors.read().unwrap().iter() {
            self.send_on_channel_checked(channel, packet.clone(), *node_id);
        }
    }

    /// Sends a `Packet` on the given channel.
    /// If `channel.send` fails, it sends an `ErrorInRouting`
    /// (using `PacketCommand::ProcessNack`) back to the `Transmitter`
    fn send_on_channel_checked(&self, channel: &Sender<Packet>, packet: Packet, next_hop: NodeId) {
        match channel.send(packet.clone()) {
            Ok(()) => {
                log::info!("Packet {packet} successfully sent to {next_hop}");
                let event = NodeEvent::PacketSent(packet);
                self.simulation_controller_notifier.send_event(event);
            }
            Err(SendError(packet)) => {
                let nack_type = NackType::ErrorInRouting(next_hop);
                log::warn!("Error while sending packet {packet} to node {next_hop}: sending nack packet {nack_type:?}");
                let command = PacketCommand::ProcessNack {
                    session_id: packet.session_id,
                    nack: Nack {
                        fragment_index: 0, // Useless if sending ErrorInRouting, so set it to 0
                        nack_type,
                    },
                    source: self.node_id,
                };
                self.send_command_to_transmitter(command);
            }
        }
    }

    /// Sends a `FloodResponse` to a neighbor after creating a `SourceRoutingHeader` from the `path_trace` information
    /// # Panics
    /// - Panics if the `path_trace` does not have a next hop to forward the response
    /// - Panics if there is no channel for the required next hop
    pub fn send_flood_response(&self, flood_response: FloodResponse) {
        let Some((forward_to, _node_type)) = flood_response.path_trace.get(1).copied() else {
            panic!("No next hop in path trace to forward back this FloodResponse");
        };

        let hops: Vec<NodeId> = flood_response
            .path_trace.iter()
            .map(|(node_id, _node_type)| *node_id )
            .collect();

        let wrapper_packet = Packet {
            routing_header: SourceRoutingHeader {
                hop_index: 1,
                hops,
            },
            session_id: 0, // TODO: default value
            pack_type: PacketType::FloodResponse(flood_response),
        };

        let binding = self.neighbors.read().unwrap();
        let Some(channel) = binding.get(&forward_to) else {
            panic!("No channel found to forward the flood response back to who sent it");
        };

        self.send_on_channel_checked(channel, wrapper_packet, forward_to);
    }

    /// Forwards a `Packet` based on its `SourceRoutingHeader`.
    /// It expects to receive `Packet`s with `hop_index` set to `0`
    /// # Panics
    /// - Panics if there is no next hop in the header
    /// - Panics if there is no channel associated to the required next hop
    pub fn forward(&self, mut packet: Packet) {
        let Some(next_hop) = packet.routing_header.next_hop() else {
            panic!("No next hop for packet {packet}");
        };

        packet.routing_header.hop_index += 1;

        if let Some(channel) = self.neighbors.read().unwrap().get(&next_hop) {
            self.send_on_channel_checked(channel, packet, next_hop);
        } else {
            panic!("No channel for required next hop ({next_hop}) for packet {packet}");
        }
    }

    /// Adds or updates a channel associated to the `node_id`
    pub fn add_neighbor(&self, node_id: NodeId, channel: Sender<Packet>) {
        if self.neighbors.write().unwrap().insert(node_id, channel).is_none() {
            log::info!("Added neighbor with NodeId {node_id}");
        } else {
            log::info!("Updated neighbor's channel associated to NodeId {node_id}");
        }
    }

    /// Removes a channel from the connected neighbors
    pub fn remove_neighbor(&self, node_id: NodeId) {
        if self.neighbors.write().unwrap().remove(&node_id).is_some() {
            log::info!("Removed neighbor's channel associated to NodeId {node_id}");
        } else {
            log::warn!("Cannot remove neighbor's channel associated to NodeId {node_id}: there is no channel to remove");
        }
    }

    /// Sends a `PacketCommand` to `Transmitter`
    /// # Panics
    /// - Panics if the communication fails
    pub fn send_command_to_transmitter(&self, command: PacketCommand) {
        match self.gateway_to_transmitter_tx.send(command) {
            Ok(()) => {}
            Err(SendError(command)) => {
                panic!("Cannot send {command:?} to transmitter");
            }
        }
    }
}

#[cfg(test)]
mod test {
    #![allow(unused_variables)]
    #![allow(unused_mut)]

    use super::*;
    use crossbeam_channel::unbounded;
    use ntest::timeout;
    use std::collections::HashMap;
    use wg_2024::packet::{Ack, FloodRequest, NodeType, Packet};

    #[test]
    fn initialize() {
        let node_id = 10;

        let (gateway_to_transmitter_tx, gateway_to_transmitter_rx) = unbounded();

        let (simulation_controller_tx, simulation_controller_rx) = unbounded::<NodeEvent>();
        let simulation_controller_notifier =
            SimulationControllerNotifier::new(simulation_controller_tx);
        let simulation_controller_notifier = Arc::new(simulation_controller_notifier);

        let gateway = Gateway::new(
            node_id,
            HashMap::new(),
            gateway_to_transmitter_tx.clone(),
            simulation_controller_notifier.clone(),
        );

        assert_eq!(gateway.node_id, node_id);
        assert!(gateway.neighbors.read().unwrap().is_empty());

        let (tx, _rx) = unbounded::<Packet>();
        let expected = Gateway {
            node_id,
            neighbors: RwLock::new(HashMap::new()),
            gateway_to_transmitter_tx,
            simulation_controller_notifier,
        };

        assert_eq!(gateway, expected);
    }

    /*
    #[test]
    fn check_forward_failure_error_in_routing() {
        let (simulation_controller_tx, simulation_controller_rx) = unbounded::<NodeEvent>();
        let simulation_controller_notifier = SimulationControllerNotifier::new(simulation_controller_tx);
        let simulation_controller_notifier = Arc::new(simulation_controller_notifier);
        let gateway = Gateway::new(10, HashMap::new(), simulation_controller_notifier);
        let packet = Packet {
            pack_type: PacketType::Ack(Ack { fragment_index: 0 }),
            routing_header: SourceRoutingHeader {
                hop_index: 0,
                hops: vec![10, 1, 2],
            },
            session_id: 0,
        };
        gateway.forward(packet);

        let received = rx.recv().unwrap();
        let expected = Packet {
            routing_header: SourceRoutingHeader {
                hop_index: 0,
                hops: vec![10],
            },
            session_id: 0,
            pack_type: PacketType::Nack(Nack {
                fragment_index: 0,
                nack_type: NackType::ErrorInRouting(1),
            }),
        };
        assert_eq!(received, expected);
    }
     */

    #[test]
    fn check_forward_successful() {
        let (tx_drone, rx_drone) = unbounded::<Packet>();
        let mut neighbors = HashMap::new();
        neighbors.insert(1, tx_drone);

        let (gateway_to_transmitter_tx, gateway_to_transmitter_rx) = unbounded();

        let (simulation_controller_tx, simulation_controller_rx) = unbounded::<NodeEvent>();
        let simulation_controller_notifier =
            SimulationControllerNotifier::new(simulation_controller_tx);
        let simulation_controller_notifier = Arc::new(simulation_controller_notifier);

        let gateway = Gateway::new(
            10,
            neighbors,
            gateway_to_transmitter_tx,
            simulation_controller_notifier,
        );

        let packet = Packet {
            pack_type: PacketType::Ack(Ack { fragment_index: 0 }),
            routing_header: SourceRoutingHeader {
                hop_index: 0,
                hops: vec![10, 1, 2],
            },
            session_id: 0,
        };

        gateway.forward(packet);

        let received = rx_drone.recv().unwrap();

        let expected = Packet {
            pack_type: PacketType::Ack(Ack { fragment_index: 0 }),
            routing_header: SourceRoutingHeader {
                hop_index: 1,
                hops: vec![10, 1, 2],
            },
            session_id: 0,
        };
        assert_eq!(received, expected);
    }

    #[test]
    #[should_panic(
        expected = "No next hop for packet Packet(0) { routing_header: [  ], pack_type Ack(0) }"
    )]
    fn check_forward_no_next_hop() {
        let (tx_drone, rx_drone) = unbounded::<Packet>();
        let mut neighbors = HashMap::new();
        neighbors.insert(1, tx_drone);

        let (gateway_to_transmitter_tx, gateway_to_transmitter_rx) = unbounded();

        let (simulation_controller_tx, simulation_controller_rx) = unbounded::<NodeEvent>();
        let simulation_controller_notifier =
            SimulationControllerNotifier::new(simulation_controller_tx);
        let simulation_controller_notifier = Arc::new(simulation_controller_notifier);

        let gateway = Gateway::new(
            10,
            neighbors,
            gateway_to_transmitter_tx,
            simulation_controller_notifier,
        );

        let packet = Packet {
            pack_type: PacketType::Ack(Ack { fragment_index: 0 }),
            routing_header: SourceRoutingHeader {
                hop_index: 0,
                hops: vec![],
            },
            session_id: 0,
        };

        gateway.forward(packet);
    }

    #[test]
    fn send_on_channel_checked_test_successful() {
        let (tx_drone, rx_drone) = unbounded::<Packet>();
        let mut neighbors = HashMap::new();
        neighbors.insert(1, tx_drone);

        let (gateway_to_transmitter_tx, gateway_to_transmitter_rx) = unbounded();

        let (simulation_controller_tx, simulation_controller_rx) = unbounded::<NodeEvent>();
        let simulation_controller_notifier =
            SimulationControllerNotifier::new(simulation_controller_tx);
        let simulation_controller_notifier = Arc::new(simulation_controller_notifier);

        let gateway = Gateway::new(
            10,
            neighbors.clone(),
            gateway_to_transmitter_tx,
            simulation_controller_notifier,
        );

        let packet = Packet {
            pack_type: PacketType::Ack(Ack { fragment_index: 0 }),
            routing_header: SourceRoutingHeader {
                hop_index: 0,
                hops: vec![10, 1, 2],
            },
            session_id: 0,
        };

        gateway.send_on_channel_checked(
            neighbors.get(&1).unwrap(),
            packet.clone(),
            packet.routing_header.next_hop().unwrap(),
        );

        let received = rx_drone.recv().unwrap();

        assert_eq!(received, packet);
    }

    #[test]
    #[timeout(2000)]
    fn send_flood_request() {
        let gateway_node_id = 9;

        let mut drones_rx = Vec::new();
        let mut neighbors = HashMap::new();
        let (tx_drone_1, rx_drone_1) = unbounded::<Packet>();
        neighbors.insert(1, tx_drone_1);
        drones_rx.push(rx_drone_1);
        let (tx_drone_3, rx_drone_3) = unbounded::<Packet>();
        neighbors.insert(3, tx_drone_3);
        drones_rx.push(rx_drone_3);
        let (tx_drone_10, rx_drone_10) = unbounded::<Packet>();
        neighbors.insert(10, tx_drone_10);
        drones_rx.push(rx_drone_10);

        let (gateway_to_transmitter_tx, gateway_to_transmitter_rx) = unbounded();

        let (simulation_controller_tx, simulation_controller_rx) = unbounded::<NodeEvent>();
        let simulation_controller_notifier =
            SimulationControllerNotifier::new(simulation_controller_tx);
        let simulation_controller_notifier = Arc::new(simulation_controller_notifier);

        let gateway = Gateway::new(
            gateway_node_id,
            neighbors.clone(),
            gateway_to_transmitter_tx,
            simulation_controller_notifier,
        );

        let flood_request = FloodRequest {
            flood_id: 0,
            initiator_id: gateway_node_id,
            path_trace: vec![(gateway_node_id, NodeType::Server)],
        };

        gateway.send_flood_request(flood_request.clone());

        let expected = Packet {
            routing_header: SourceRoutingHeader {
                hop_index: 0,
                hops: vec![],
            },
            session_id: 0,
            pack_type: PacketType::FloodRequest(flood_request),
        };

        for channel in &drones_rx {
            let received = channel.recv().unwrap();
            assert_eq!(received, expected);
        }
    }

    #[test]
    fn send_flood_response_successful() {
        let gateway_node_id = 9;

        let mut neighbors = HashMap::new();
        let (tx_drone_1, rx_drone_1) = unbounded::<Packet>();
        neighbors.insert(1, tx_drone_1);

        let (gateway_to_transmitter_tx, gateway_to_transmitter_rx) = unbounded();

        let (simulation_controller_tx, simulation_controller_rx) = unbounded::<NodeEvent>();
        let simulation_controller_notifier =
            SimulationControllerNotifier::new(simulation_controller_tx);
        let simulation_controller_notifier = Arc::new(simulation_controller_notifier);

        let gateway = Gateway::new(
            gateway_node_id,
            neighbors.clone(),
            gateway_to_transmitter_tx,
            simulation_controller_notifier,
        );

        let session_id = 0;
        let flood_response = FloodResponse {
            flood_id: 0,
            path_trace: vec![(gateway_node_id, NodeType::Server), (1, NodeType::Drone)],
        };

        gateway.send_flood_response(flood_response.clone());

        let received = rx_drone_1.recv().unwrap();

        let expected = Packet {
            routing_header: SourceRoutingHeader {
                hop_index: 1,
                hops: vec![gateway_node_id, 1],
            },
            session_id: 0,
            pack_type: PacketType::FloodResponse(flood_response),
        };
        assert_eq!(received, expected);
    }

    #[test]
    #[should_panic(expected = "No next hop in path trace to forward back this FloodResponse")]
    fn send_flood_response_with_no_next_hop() {
        let gateway_node_id = 9;

        let mut neighbors = HashMap::new();
        let (tx_drone_1, _rx_drone_1) = unbounded::<Packet>();
        neighbors.insert(1, tx_drone_1);

        let (gateway_to_transmitter_tx, gateway_to_transmitter_rx) = unbounded();

        let (simulation_controller_tx, simulation_controller_rx) = unbounded::<NodeEvent>();
        let simulation_controller_notifier =
            SimulationControllerNotifier::new(simulation_controller_tx);
        let simulation_controller_notifier = Arc::new(simulation_controller_notifier);

        let gateway = Gateway::new(
            gateway_node_id,
            neighbors.clone(),
            gateway_to_transmitter_tx,
            simulation_controller_notifier,
        );

        let session_id = 0;
        let flood_response = FloodResponse {
            flood_id: 0,
            path_trace: vec![(gateway_node_id, NodeType::Server)],
        };

        gateway.send_flood_response(flood_response);
    }

    #[test]
    #[should_panic(expected = "No channel found")]
    fn send_flood_response_to_non_existent_node() {
        let gateway_node_id = 9;

        let mut neighbors = HashMap::new();
        let (tx_drone_1, _rx_drone_1) = unbounded::<Packet>();
        neighbors.insert(1, tx_drone_1);

        let (gateway_to_transmitter_tx, gateway_to_transmitter_rx) = unbounded();

        let (simulation_controller_tx, simulation_controller_rx) = unbounded::<NodeEvent>();
        let simulation_controller_notifier =
            SimulationControllerNotifier::new(simulation_controller_tx);
        let simulation_controller_notifier = Arc::new(simulation_controller_notifier);

        let gateway = Gateway::new(
            gateway_node_id,
            neighbors.clone(),
            gateway_to_transmitter_tx,
            simulation_controller_notifier,
        );

        let session_id = 0;
        let flood_response = FloodResponse {
            flood_id: 0,
            path_trace: vec![(gateway_node_id, NodeType::Server), (100, NodeType::Drone)],
        };

        gateway.send_flood_response(flood_response);
    }

    #[test]
    fn check_add_neighbor() {
        let (simulation_controller_tx, simulation_controller_rx) = unbounded::<NodeEvent>();
        let simulation_controller_notifier =
            SimulationControllerNotifier::new(simulation_controller_tx);
        let simulation_controller_notifier = Arc::new(simulation_controller_notifier);

        let (gateway_to_transmitter_tx, gateway_to_transmitter_rx) = unbounded();

        let mut gateway = Gateway::new(
            10,
            HashMap::new(),
            gateway_to_transmitter_tx,
            simulation_controller_notifier,
        );

        assert_eq!(gateway.neighbors.read().unwrap().len(), 0);
        let (tx_drone_5, _rx_drone_5) = unbounded::<Packet>();
        gateway.add_neighbor(5, tx_drone_5);
        assert_eq!(gateway.neighbors.read().unwrap().len(), 1);
        let (tx_drone_5, _rx_drone_5) = unbounded::<Packet>();
        gateway.add_neighbor(5, tx_drone_5);
        assert_eq!(gateway.neighbors.read().unwrap().len(), 1);
        let (tx_drone_8, _rx_drone_8) = unbounded::<Packet>();
        gateway.add_neighbor(8, tx_drone_8);
        assert_eq!(gateway.neighbors.read().unwrap().len(), 2);
    }

    #[test]
    fn check_remove_neighbor() {
        let (simulation_controller_tx, simulation_controller_rx) = unbounded::<NodeEvent>();
        let simulation_controller_notifier =
            SimulationControllerNotifier::new(simulation_controller_tx);
        let simulation_controller_notifier = Arc::new(simulation_controller_notifier);

        let (gateway_to_transmitter_tx, gateway_to_transmitter_rx) = unbounded();

        let mut gateway = Gateway::new(
            10,
            HashMap::new(),
            gateway_to_transmitter_tx,
            simulation_controller_notifier,
        );

        assert_eq!(gateway.neighbors.read().unwrap().len(), 0);
        let (tx_drone_5, _rx_drone_5) = unbounded::<Packet>();
        gateway.add_neighbor(5, tx_drone_5);
        assert_eq!(gateway.neighbors.read().unwrap().len(), 1);
        let (tx_drone_5, _rx_drone_5) = unbounded::<Packet>();
        gateway.add_neighbor(5, tx_drone_5);
        assert_eq!(gateway.neighbors.read().unwrap().len(), 1);
        let (tx_drone_8, _rx_drone_8) = unbounded::<Packet>();
        gateway.add_neighbor(8, tx_drone_8);
        assert_eq!(gateway.neighbors.read().unwrap().len(), 2);
        gateway.remove_neighbor(8);
        assert_eq!(gateway.neighbors.read().unwrap().len(), 1);
        gateway.remove_neighbor(5);
        assert_eq!(gateway.neighbors.read().unwrap().len(), 0);
    }
}
