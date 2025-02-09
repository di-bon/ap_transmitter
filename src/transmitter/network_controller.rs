mod network_graph;

use crate::transmitter::gateway::Gateway;
use crate::transmitter::network_controller::network_graph::NetworkGraph;
use ap_sc_notifier::SimulationControllerNotifier;
use messages::node_event::{EventNetworkGraph, EventNetworkNode, NodeEvent};
use std::sync::{Arc,
                // Mutex,
                RwLock};
use rand::RngCore;
use wg_2024::network::NodeId;
use wg_2024::packet::{FloodRequest, FloodResponse, Nack, NackType, NodeType};

#[derive(Debug)]
pub struct NetworkController {
    node_id: NodeId,
    node_type: NodeType,
    network_graph: RwLock<NetworkGraph>,
    gateway: Arc<Gateway>, // gateway reference used to send all the FloodRequests
    simulation_controller_notifier: Arc<SimulationControllerNotifier>,
    // flood_id: Mutex<u64>,
}

impl PartialEq for NetworkController {
    fn eq(&self, other: &Self) -> bool {
        self.node_id == other.node_id
            && self
                .network_graph
                .read()
                .unwrap()
                .eq(&other.network_graph.read().unwrap())
            && self.gateway.eq(&other.gateway)
    }
}

impl NetworkController {
    pub fn new(
        node_id: NodeId,
        node_type: NodeType,
        gateway: Arc<Gateway>,
        simulation_controller_notifier: Arc<SimulationControllerNotifier>,
    ) -> Self {
        Self {
            node_id,
            node_type,
            network_graph: RwLock::new(NetworkGraph::new(node_id, node_type)),
            gateway,
            simulation_controller_notifier,
            // flood_id: Mutex::new(0),
        }
    }

    /// Floods the network with `FloodRequest`s
    pub fn flood_network(&self) {
        /*
        let mut guard = self.flood_id.lock().unwrap();
        let flood_id = *guard;
        *guard += 1;
         */

        let mut rng = rand::rng();
        let flood_id = rng.next_u64();

        let flood_request = FloodRequest::initialize(flood_id, self.node_id, self.node_type);

        self.gateway.send_flood_request(flood_request);
    }

    /// Computes the path to a given node
    /// # Return
    /// Return the optional path to a node
    pub fn get_path(&self, to: NodeId) -> Option<Vec<NodeId>> {
        self.network_graph.read().unwrap().get_path_to(to)
    }

    /// Updates the known topology with the given `FloodResponse`
    pub fn update_from_flood_response(&self, flood_response: &FloodResponse) {
        self.network_graph
            .read()
            .unwrap()
            .insert_edges_from_path_trace(&flood_response.path_trace);

        self.send_known_network_graph();
    }

    /// Updates the topology information with the given `Nack`
    pub fn update_from_nack(&self, nack: &Nack, source: NodeId) {
        match nack.nack_type {
            NackType::ErrorInRouting(next_hop) => {
                // remove edge between next_hop and source
                self.network_graph
                    .read()
                    .unwrap()
                    .delete_bidirectional_edge(source, next_hop);
                self.send_known_network_graph();
            }
            NackType::DestinationIsDrone | NackType::UnexpectedRecipient(_) => {
                // Something went wrong, reset the network graph and flood the network again
                self.network_graph.write().unwrap().reset_graph();
                self.send_known_network_graph();
                self.flood_network();
            }
            NackType::Dropped => {
                // Update num_of_dropped_packets
                self.network_graph
                    .read()
                    .unwrap()
                    .increment_num_of_dropped_packets(source);
            }
        }
    }

    /// Sends the known topology to the Simulation Controller
    fn send_known_network_graph(&self) {
        let event_graph = self.get_event_graph();
        let event = NodeEvent::KnownNetworkGraph {
            source: self.node_id,
            graph: event_graph,
        };
        self.simulation_controller_notifier.send_event(event);
    }

    /// Generates the `EventNetworkGraph` of the current topology
    fn get_event_graph(&self) -> EventNetworkGraph {
        let mut nodes = vec![];

        let network_graph = self.network_graph.read().unwrap();
        for network_node in network_graph.nodes.read().unwrap().iter() {
            let network_node = network_node.read().unwrap();
            let neighbors = {
                let mut neighbors: Vec<NodeId> = vec![];

                for neighbor in network_node.neighbors.read().unwrap().iter() {
                    neighbors.push(*neighbor);
                }

                neighbors
            };
            let result_node = EventNetworkNode {
                node_id: network_node.node_id,
                node_type: network_node.node_type,
                neighbors,
            };
            nodes.push(result_node);
        }

        EventNetworkGraph { nodes }
    }

    /// Inserts a new neighbor to the current node in the known topology
    pub fn insert_neighbor(&self, to: NodeId) {
        self.network_graph
            .read()
            .unwrap()
            .insert_node_if_not_present(to, NodeType::Drone);

        self.network_graph
            .read()
            .unwrap()
            .insert_bidirectional_edge(self.node_id, to);
    }

    /// Deletes a neighbor to the current node in the known topology
    pub fn delete_neighbor_edge(&self, to: NodeId) {
        self.network_graph
            .read()
            .unwrap()
            .delete_bidirectional_edge(self.node_id, to);
    }

    /// Increments the number of dropped packets of the given `node_id`
    pub fn increment_dropped_count(&self, node_id: NodeId) {
        self.network_graph
            .read()
            .unwrap()
            .increment_num_of_dropped_packets(node_id);
    }
}

#[cfg(test)]
mod tests {
    #![allow(unused_variables)]
    #![allow(unused_mut)]

    use super::*;
    use ap_sc_notifier::SimulationControllerNotifier;
    use crossbeam_channel::{unbounded, Sender};
    use ntest::timeout;
    use std::collections::HashMap;
    use wg_2024::network::SourceRoutingHeader;
    use wg_2024::packet::{Nack, Packet, PacketType};

    #[timeout(2000)]
    #[test]
    fn initialize() {
        let node_id = 0;
        let node_type = NodeType::Server;

        let (simulation_controller_tx, simulation_controller_rx) = unbounded::<NodeEvent>();
        let simulation_controller_notifier =
            SimulationControllerNotifier::new(simulation_controller_tx);
        let simulation_controller_notifier = Arc::new(simulation_controller_notifier);

        let (gateway_to_transmitter_tx, gateway_to_transmitter_rx) = unbounded();

        let gateway = Gateway::new(
            node_id,
            HashMap::new(),
            gateway_to_transmitter_tx,
            simulation_controller_notifier.clone(),
        );
        let gateway = Arc::new(gateway);

        let network_controller = NetworkController::new(
            node_id,
            node_type,
            gateway.clone(),
            simulation_controller_notifier.clone(),
        );

        let expected = NetworkController {
            node_id,
            node_type,
            network_graph: RwLock::new(NetworkGraph::new(node_id, node_type)),
            gateway: gateway.clone(),
            simulation_controller_notifier,
            // flood_id: Mutex::new(0),
        };

        assert_eq!(network_controller, expected);
    }

    #[test]
    fn update_from_error_in_routing() {
        let node_id = 0;
        let node_type = NodeType::Server;

        let (simulation_controller_tx, simulation_controller_rx) = unbounded::<NodeEvent>();
        let simulation_controller_notifier =
            SimulationControllerNotifier::new(simulation_controller_tx);
        let simulation_controller_notifier = Arc::new(simulation_controller_notifier);

        let (gateway_to_transmitter_tx, gateway_to_transmitter_rx) = unbounded();

        let gateway = Gateway::new(
            node_id,
            HashMap::new(),
            gateway_to_transmitter_tx,
            simulation_controller_notifier.clone(),
        );
        let gateway = Arc::new(gateway);

        let mut network_controller = NetworkController::new(
            node_id,
            node_type,
            gateway,
            simulation_controller_notifier.clone(),
        );

        /*
        | --------|
        0 -- 1 -- 2 -- 3
                  |
                  -- 5 -- 8
         */

        let flood_response = FloodResponse {
            flood_id: 0,
            path_trace: vec![
                (node_id, node_type),
                (1, NodeType::Drone),
                (2, NodeType::Drone),
                (3, NodeType::Client),
            ],
        };
        network_controller.update_from_flood_response(&flood_response);
        let flood_response = FloodResponse {
            flood_id: 0,
            path_trace: vec![
                (node_id, node_type),
                (2, NodeType::Drone),
                (5, NodeType::Drone),
                (8, NodeType::Server),
            ],
        };
        network_controller.update_from_flood_response(&flood_response);
        let flood_response = FloodResponse {
            flood_id: 0,
            path_trace: vec![
                (node_id, node_type),
                (1, NodeType::Drone),
                (8, NodeType::Server),
            ],
        };
        network_controller.update_from_flood_response(&flood_response);

        let hops = network_controller.get_path(8);
        let expected = Some(vec![0, 1, 8]);
        assert_eq!(hops, expected);

        let nack = Nack {
            fragment_index: 0,
            nack_type: NackType::ErrorInRouting(8),
        };
        // let nack = Packet {
        //     routing_header: SourceRoutingHeader {
        //         hop_index: 1,
        //         hops: vec![1, 0],
        //     },
        //     session_id: 0,
        //     pack_type: PacketType::Nack(nack),
        // };

        network_controller.update_from_nack(&nack, 1);

        let hops = network_controller.get_path(8);
        let expected = Some(vec![0, 2, 5, 8]);
        assert_eq!(hops, expected);
    }

    #[timeout(2000)]
    #[test]
    fn update_from_dropped() {
        let node_id = 0;
        let node_type = NodeType::Server;

        let (simulation_controller_tx, simulation_controller_rx) = unbounded::<NodeEvent>();
        let simulation_controller_notifier =
            SimulationControllerNotifier::new(simulation_controller_tx);
        let simulation_controller_notifier = Arc::new(simulation_controller_notifier);

        let (gateway_to_transmitter_tx, gateway_to_transmitter_rx) = unbounded();

        let gateway = Gateway::new(
            node_id,
            HashMap::new(),
            gateway_to_transmitter_tx,
            simulation_controller_notifier.clone(),
        );
        let gateway = Arc::new(gateway);

        let mut network_controller = NetworkController::new(
            node_id,
            node_type,
            gateway,
            simulation_controller_notifier.clone(),
        );

        let flood_response = FloodResponse {
            flood_id: 0,
            path_trace: vec![
                (node_id, node_type),
                (1, NodeType::Drone),
                (2, NodeType::Drone),
                (3, NodeType::Client),
            ],
        };
        network_controller.update_from_flood_response(&flood_response);

        let flood_response = FloodResponse {
            flood_id: 0,
            path_trace: vec![
                (node_id, node_type),
                (1, NodeType::Drone),
                (4, NodeType::Drone),
                (3, NodeType::Client),
            ],
        };
        network_controller.update_from_flood_response(&flood_response);

        let nack = Nack {
            fragment_index: 0,
            nack_type: NackType::Dropped,
        };
        // let nack = Packet {
        //     routing_header: SourceRoutingHeader {
        //         hop_index: 2,
        //         hops: vec![2, 1, 0],
        //     },
        //     session_id: 0,
        //     pack_type: PacketType::Nack(nack),
        // };
        network_controller.update_from_nack(&nack, 2);

        let hops = network_controller.get_path(3);
        let expected = Some(vec![0, 1, 4, 3]);
        assert_eq!(hops, expected);
    }

    #[test]
    #[timeout(2000)]
    fn check_flood_network() {
        let node_id = 0;
        let node_type = NodeType::Server;

        let mut connected_drones: HashMap<NodeId, Sender<Packet>> = HashMap::new();

        let drone_1_node_id = 1;
        let (drone_1_tx, drone_1_rx) = unbounded::<Packet>();
        connected_drones.insert(drone_1_node_id, drone_1_tx);

        let drone_2_node_id = 2;
        let (drone_2_tx, drone_2_rx) = unbounded::<Packet>();
        connected_drones.insert(drone_2_node_id, drone_2_tx);

        let (simulation_controller_tx, simulation_controller_rx) = unbounded::<NodeEvent>();
        let simulation_controller_notifier =
            SimulationControllerNotifier::new(simulation_controller_tx);
        let simulation_controller_notifier = Arc::new(simulation_controller_notifier);

        let (gateway_to_transmitter_tx, gateway_to_transmitter_rx) = unbounded();

        let gateway = Gateway::new(
            node_id,
            connected_drones,
            gateway_to_transmitter_tx,
            simulation_controller_notifier.clone(),
        );
        let gateway = Arc::new(gateway);

        let mut network_controller = NetworkController::new(
            node_id,
            node_type,
            gateway,
            simulation_controller_notifier.clone(),
        );

        network_controller.flood_network();

        let expected_source_routing_header = SourceRoutingHeader {
            hop_index: 0,
            hops: vec![],
        };
        let expected_initiator_id = node_id;
        let expected_path_trace = vec![(node_id, node_type)];

        let received = drone_1_rx.recv().unwrap();

        assert_eq!(received.routing_header, expected_source_routing_header);
        let received_flood_request = match &received.pack_type {
            PacketType::FloodRequest(flood_request) => flood_request,
            _ => panic!("Received unexpected packet type"),
        };
        assert_eq!(received_flood_request.initiator_id, expected_initiator_id);
        assert_eq!(received_flood_request.path_trace, expected_path_trace);

        let received = drone_2_rx.recv().unwrap();

        assert_eq!(received.routing_header, expected_source_routing_header);
        let received_flood_request = match &received.pack_type {
            PacketType::FloodRequest(flood_request) => flood_request,
            _ => panic!("Received unexpected packet type"),
        };
        assert_eq!(received_flood_request.initiator_id, expected_initiator_id);
        assert_eq!(received_flood_request.path_trace, expected_path_trace);
    }

    #[test]
    #[timeout(2000)]
    fn reset_graph_from_flood() {
        let node_id = 0;
        let node_type = NodeType::Server;

        let connected_drones = HashMap::new();

        let (simulation_controller_tx, simulation_controller_rx) = unbounded::<NodeEvent>();
        let simulation_controller_notifier =
            SimulationControllerNotifier::new(simulation_controller_tx);
        let simulation_controller_notifier = Arc::new(simulation_controller_notifier);

        let (gateway_to_transmitter_tx, gateway_to_transmitter_rx) = unbounded();

        let gateway = Gateway::new(
            node_id,
            connected_drones,
            gateway_to_transmitter_tx,
            simulation_controller_notifier.clone(),
        );
        let gateway = Arc::new(gateway);

        let network_controller = NetworkController::new(
            node_id,
            node_type,
            gateway,
            simulation_controller_notifier.clone(),
        );

        let flood_response = FloodResponse {
            flood_id: 0,
            path_trace: vec![
                (node_id, node_type),
                (1, NodeType::Drone),
                (2, NodeType::Drone),
                (3, NodeType::Client),
            ],
        };

        network_controller.update_from_flood_response(&flood_response);

        let nack = Nack {
            fragment_index: 0,
            nack_type: NackType::DestinationIsDrone,
        };
        // let nack = Packet {
        //     routing_header: SourceRoutingHeader { hop_index: 0, hops: vec![] },
        //     session_id: 0,
        //     pack_type: PacketType::Nack(nack),
        // };
        network_controller.update_from_nack(&nack, 1);

        assert_eq!(
            network_controller
                .network_graph
                .read()
                .unwrap()
                .nodes
                .read()
                .unwrap()
                .len(),
            1
        );
    }

    /*
    #[test]
    #[should_panic(expected = "Received a NACK packet with no source in header")]
    fn error_in_routing_with_no_header() {
        let node_id = 0;
        let node_type = NodeType::Server;

        let connected_drones = HashMap::new();
        let (gateway_to_listener_tx, gateway_to_listener_rx) = unbounded::<Packet>();

        let (simulation_controller_tx, simulation_controller_rx) = unbounded::<NodeEvent>();
        let simulation_controller_notifier = SimulationControllerNotifier::new(simulation_controller_tx);
        let simulation_controller_notifier = Arc::new(simulation_controller_notifier);

        let gateway = Gateway::new(node_id, connected_drones, gateway_to_listener_tx, simulation_controller_notifier.clone());
        let gateway = Arc::new(gateway);

        let network_controller = NetworkController::new(node_id, node_type, gateway, simulation_controller_notifier.clone());

        let nack = Nack {
            fragment_index: 0,
            nack_type: NackType::ErrorInRouting(10),
        };
        // let nack = Packet {
        //     routing_header: SourceRoutingHeader { hop_index: 0, hops: vec![] },
        //     session_id: 0,
        //     pack_type: PacketType::Nack(nack),
        // };

        network_controller.update_from_nack(&nack, 0);
    }
     */

    /*
    #[test]
    #[should_panic(expected = "Received a packet with no hops in routing header")]
    fn dropped_with_no_header() {
        let node_id = 0;
        let node_type = NodeType::Server;

        let connected_drones = HashMap::new();
        let (gateway_to_listener_tx, gateway_to_listener_rx) = unbounded::<Packet>();

        let (simulation_controller_tx, simulation_controller_rx) = unbounded::<NodeEvent>();
        let simulation_controller_notifier = SimulationControllerNotifier::new(simulation_controller_tx);
        let simulation_controller_notifier = Arc::new(simulation_controller_notifier);

        let gateway = Gateway::new(node_id, connected_drones, gateway_to_listener_tx, simulation_controller_notifier.clone());
        let gateway = Arc::new(gateway);

        let network_controller = NetworkController::new(node_id, node_type, gateway, simulation_controller_notifier.clone());

        let nack = Nack {
            fragment_index: 0,
            nack_type: NackType::Dropped,
        };
        let nack = Packet {
            routing_header: SourceRoutingHeader { hop_index: 0, hops: vec![] },
            session_id: 0,
            pack_type: PacketType::Nack(nack),
        };

        network_controller.update_from_nack(&nack);
    }
     */

    /*
    #[test]
    #[should_panic(expected = "Expected nack packet!")]
    fn update_from_nack_wrong_packet_type() {
        let node_id = 0;
        let node_type = NodeType::Server;

        let connected_drones = HashMap::new();
        let (gateway_to_listener_tx, gateway_to_listener_rx) = unbounded::<Packet>();

        let (simulation_controller_tx, simulation_controller_rx) = unbounded::<NodeEvent>();
        let simulation_controller_notifier = SimulationControllerNotifier::new(simulation_controller_tx);
        let simulation_controller_notifier = Arc::new(simulation_controller_notifier);

        let gateway = Gateway::new(node_id, connected_drones, gateway_to_listener_tx, simulation_controller_notifier.clone());
        let gateway = Arc::new(gateway);

        let network_controller = NetworkController::new(node_id, node_type, gateway, simulation_controller_notifier.clone());

        let ack = Ack {
            fragment_index: 0,
        };
        let ack = Packet {
            routing_header: SourceRoutingHeader { hop_index: 0, hops: vec![] },
            session_id: 0,
            pack_type: PacketType::Ack(ack),
        };

        network_controller.update_from_nack(&ack);
    }
     */
}
