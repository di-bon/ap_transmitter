mod network_node;

use crate::transmitter::network_controller::network_graph::network_node::NetworkNode;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use wg_2024::network::NodeId;
use wg_2024::packet::NodeType;

#[derive(Debug)]
pub struct NetworkGraph {
    owner_node_id: NodeId,
    owner_node_type: NodeType,
    pub nodes: RwLock<Vec<Arc<RwLock<NetworkNode>>>>,
}

impl PartialEq for NetworkGraph {
    fn eq(&self, other: &Self) -> bool {
        let nodes = self.nodes.read().unwrap();
        let other_nodes = other.nodes.read().unwrap();

        if nodes.len() != other_nodes.len() {
            return false;
        }

        // if NodeType were to implement the Hash trait, this comparison could be done using a HashSet
        for i in 0..nodes.len() {
            let node = nodes[i].clone();
            let other_node = other_nodes[i].clone();
            if !node.read().unwrap().eq(&*other_node.read().unwrap()) {
                return false;
            }
        }

        self.owner_node_id == other.owner_node_id && self.owner_node_type == other.owner_node_type
    }
}

impl Eq for NetworkGraph {}

impl NetworkGraph {
    /// Returns a new instance of `NetworkGraph`
    pub(super) fn new(owner_node_id: NodeId, owner_node_type: NodeType) -> Self {
        let result = Self {
            owner_node_id,
            owner_node_type,
            nodes: RwLock::new(vec![]),
        };
        result.insert_node(owner_node_id, owner_node_type);
        result
    }

    /// Inserts a new node with into `NetworkGraph`. This function does NOT check whether there
    /// already is a node with the passed `node_id: NodeId`. If that happens, there may be issues
    /// with the path finding, so call `insert_node_if_not_present` to insert a new node
    fn insert_node(&self, node_id: NodeId, node_type: NodeType) {
        let node = NetworkNode::new(node_id, node_type);
        let node = RwLock::new(node);
        let node = Arc::new(node);
        self.nodes.write().unwrap().push(node);
        log::info!("Inserted node with NodeId {node_id}");
    }

    /// Inserts a new node with into `NetworkGraph` if there is no node with the required `node_id: NodeId`
    pub(super) fn insert_node_if_not_present(&self, node_id: NodeId, node_type: NodeType) {
        let insert_node = {
            let nodes = self.nodes.read().unwrap();
            !nodes
                .iter()
                .any(|node| node.read().unwrap().node_id == node_id)
        };

        if insert_node {
            self.insert_node(node_id, node_type);
        } else {
            log::warn!("Tried to insert into NetworkGraph a NetworkNode with an already present node_id ({node_id})");
        }
    }

    /// Resets the `NetworkGraph` to contain just the `NodeId` and `NodeType` of the transmitter node
    pub(super) fn reset_graph(&mut self) {
        let owner_node = NetworkNode::new(self.owner_node_id, self.owner_node_type);
        let owner_node = Arc::new(RwLock::new(owner_node));
        self.nodes = RwLock::new(vec![owner_node]);
        log::info!("Network graph reset");
    }

    /// Inserts a bidirectional edge between `from` and `to`
    /// # Panics
    /// Panics if there is no node with `NodeId` equal to `from` or `to`
    pub(super) fn insert_bidirectional_edge(&self, from: NodeId, to: NodeId) {
        let nodes = self.nodes.read().unwrap();

        let Some(node_from) = nodes
            .iter()
            .find(|node| node.read().unwrap().node_id == from)
        else {
            panic!("Node with node_id {from} does not exist");
        };

        let Some(node_to) = nodes.iter().find(|node| node.read().unwrap().node_id == to) else {
            panic!("Node with node_id {to} does not exist");
        };

        node_from.write().unwrap().insert_edge(to);
        node_to.write().unwrap().insert_edge(from);
    }

    /// Processes a `path_trace`, inserting the new nodes and connections not yet stored
    pub fn insert_edges_from_path_trace(&self, path_trace: &[(NodeId, NodeType)]) {
        log::info!("Processing FloodResponse's path_trace");
        for (node_id, node_type) in path_trace {
            self.insert_node_if_not_present(*node_id, *node_type);
        }

        for ((first_id, _first_type), (second_id, _second_type)) in
            path_trace.iter().zip(path_trace.iter().skip(1))
        {
            self.insert_bidirectional_edge(*first_id, *second_id);
        }
        log::info!("FloodResponse's path_trace successfully processed");
    }

    /// Deletes a bidirectional edge between `from` and `to`
    pub(super) fn delete_bidirectional_edge(&self, from: NodeId, to: NodeId) {
        let nodes = self.nodes.read().unwrap();
        let node_from = nodes
            .iter()
            .find(|node| node.read().unwrap().node_id == from);
        let node_to = nodes.iter().find(|node| node.read().unwrap().node_id == to);
        match (node_from, node_to) {
            (Some(node_from), Some(node_to)) => {
                node_from.write().unwrap().remove_edge(to);
                node_to.write().unwrap().remove_edge(from);
            }
            _ => {
                log::warn!("Cannot delete bidirectional edge between {from} and {to}: at least one of them does not exist");
            }
        }
    }

    /// Increments the number of dropped packets for the given `node_id`
    pub(super) fn increment_num_of_dropped_packets(&self, node_id: NodeId) {
        use wg_2024::packet::NackType;

        let borrow = self.nodes.read().unwrap();
        let faulty_node = borrow
            .iter()
            .find(|node| node.read().unwrap().node_id == node_id);
        match faulty_node {
            Some(node) => {
                node.write().unwrap().increment_dropped_packets();
            }
            None => {
                // just ignore this case?
                // It may arise when an old Nack::Dropped is received after resetting the
                // graph and flooding it again
                log::info!("Ignoring old {:?}: graph has been reset", NackType::Dropped);
            }
        }
    }

    /// # Returns
    /// Returns an `HashMap` associating every node to its predecessor based on the computed cost
    /// (i.e. the total number of dropped packets so far in a given path)
    /// # Panics
    /// Panics if a required Node has been removed from the internal data structures
    fn get_paths(&self) -> HashMap<NodeId, NodeId> {
        let mut come_from: HashMap<NodeId, NodeId> = HashMap::new();
        let mut to_be_examined: Vec<NodeId> = Vec::new();
        let mut costs: HashMap<NodeId, u64> = HashMap::new();

        costs.insert(self.owner_node_id, 0);
        to_be_examined.push(self.owner_node_id);

        while !to_be_examined.is_empty() {
            let current_node_id = to_be_examined[0];
            to_be_examined.remove(0);

            let borrow = self.nodes.read().unwrap();
            let Some(current_node) = borrow
                .iter()
                .find(|node| node.read().unwrap().node_id == current_node_id)
            else {
                panic!("Cannot get node with NodeId {current_node_id} while getting min paths");
            };

            if current_node.read().unwrap().node_type != NodeType::Drone
                && current_node.read().unwrap().node_id != self.owner_node_id
            {
                continue;
            }

            let Some(current_cost) = costs.get(&current_node_id).copied() else {
                panic!(
                    "Cannot get node's cost with NodeId {current_node_id} while getting min paths"
                );
            };

            for neighbor_node_id in current_node
                .read()
                .unwrap()
                .neighbors
                .read()
                .unwrap()
                .iter()
            {
                let neighbor_current_cost = match costs.get(neighbor_node_id) {
                    Some(cost) => *cost,
                    None => u64::MAX,
                };

                let neighbor = {
                    let binding = self.nodes.read().unwrap();
                    if let Some(node) = binding
                        .iter()
                        .find(|node| node.read().unwrap().node_id == *neighbor_node_id)
                    {
                        node.clone()
                    } else {
                        panic!("Cannot find neighbor with NodeId {neighbor_node_id} for current node {current_node_id}");
                    }
                };

                let neighbor_proposed_cost =
                    current_cost + neighbor.read().unwrap().num_of_dropped_packets;

                if neighbor_proposed_cost < neighbor_current_cost {
                    come_from.insert(*neighbor_node_id, current_node_id);
                    costs.insert(*neighbor_node_id, neighbor_proposed_cost);

                    if !to_be_examined.contains(neighbor_node_id) {
                        to_be_examined.push(*neighbor_node_id);
                    }
                }
            }

            to_be_examined.sort_by_key(|node| costs.get(node).unwrap());
        }

        come_from
    }

    /// # Returns
    /// Returns the optional path to a given node
    pub fn get_path_to(&self, to: NodeId) -> Option<Vec<NodeId>> {
        let distances = self.get_paths();
        let mut result: Vec<NodeId> = Vec::new();

        let mut current = distances.get(&to)?;
        result.push(to);
        while *current != self.owner_node_id {
            result.push(*current);
            current = distances.get(current)?;
        }
        result.push(self.owner_node_id);

        result.reverse();
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    #![allow(unused_variables)]
    #![allow(unused_mut)]

    use super::*;
    use wg_2024::packet::FloodResponse;

    #[test]
    fn initialize() {
        let node_id = 0;
        let node_type = NodeType::Server;
        let graph = NetworkGraph::new(node_id, node_type);

        let node = NetworkNode::new(node_id, node_type);
        let node = Arc::new(RwLock::new(node));
        let expected = NetworkGraph {
            owner_node_id: node_id,
            owner_node_type: node_type,
            nodes: RwLock::new(vec![node]),
        };

        assert_eq!(graph, expected);
    }

    #[test]
    fn insert_two_equal_nodes() {
        let node_id = 0;
        let node_type = NodeType::Server;
        let graph = NetworkGraph::new(node_id, node_type);

        let owner_node = NetworkNode::new(node_id, node_type);
        let owner_node = Arc::new(RwLock::new(owner_node));

        let new_node_id = 1;
        let new_node_type = NodeType::Drone;
        let node = NetworkNode::new(new_node_id, new_node_type);
        let node = Arc::new(RwLock::new(node));

        graph.insert_node(new_node_id, new_node_type);
        graph.insert_node(new_node_id, new_node_type);

        let expected = NetworkGraph {
            owner_node_id: node_id,
            owner_node_type: node_type,
            nodes: RwLock::new(vec![owner_node.clone(), node.clone(), node.clone()]),
        };

        assert_eq!(graph, expected);
    }

    #[test]
    fn insert_node_if_not_present_twice() {
        let node_id = 0;
        let node_type = NodeType::Server;
        let graph = NetworkGraph::new(node_id, node_type);

        let owner_node = NetworkNode::new(node_id, node_type);
        let owner_node = Arc::new(RwLock::new(owner_node));

        let new_node_id = 1;
        let new_node_type = NodeType::Drone;
        let node = NetworkNode::new(new_node_id, new_node_type);
        let node = Arc::new(RwLock::new(node));

        graph.insert_node_if_not_present(new_node_id, new_node_type);
        graph.insert_node_if_not_present(new_node_id, new_node_type);

        let expected = NetworkGraph {
            owner_node_id: node_id,
            owner_node_type: node_type,
            nodes: RwLock::new(vec![owner_node.clone(), node.clone()]),
        };

        assert_eq!(graph, expected);
    }

    #[test]
    fn reset_graph_to_initial_state() {
        let node_id = 0;
        let node_type = NodeType::Server;
        let mut graph = NetworkGraph::new(node_id, node_type);

        let owner_node = NetworkNode::new(node_id, node_type);
        let owner_node = Arc::new(RwLock::new(owner_node));

        let new_node_id = 1;
        let new_node_type = NodeType::Drone;
        let node_1 = NetworkNode::new(new_node_id, new_node_type);
        let node_1 = Arc::new(RwLock::new(node_1));

        graph.insert_node(new_node_id, new_node_type);
        graph.insert_node(new_node_id, new_node_type);

        let expected = NetworkGraph {
            owner_node_id: node_id,
            owner_node_type: node_type,
            nodes: RwLock::new(vec![owner_node.clone(), node_1.clone(), node_1.clone()]),
        };

        assert_eq!(graph, expected);

        let new_node_id = 2;
        let new_node_type = NodeType::Drone;
        let node_2 = NetworkNode::new(new_node_id, new_node_type);
        let node_2 = Arc::new(RwLock::new(node_2));

        graph.insert_node_if_not_present(new_node_id, new_node_type);
        graph.insert_node_if_not_present(new_node_id, new_node_type);

        let expected = NetworkGraph {
            owner_node_id: node_id,
            owner_node_type: node_type,
            nodes: RwLock::new(vec![
                owner_node.clone(),
                node_1.clone(),
                node_1.clone(),
                node_2.clone(),
            ]),
        };

        assert_eq!(graph, expected);

        graph.reset_graph();

        let expected = NetworkGraph {
            owner_node_id: node_id,
            owner_node_type: node_type,
            nodes: RwLock::new(vec![owner_node.clone()]),
        };

        assert_eq!(graph, expected);
    }

    #[test]
    fn add_edges_from_path_trace() {
        let node_id = 0;
        let node_type = NodeType::Server;
        let graph = NetworkGraph::new(node_id, node_type);

        let flood_response = FloodResponse {
            flood_id: 0,
            path_trace: vec![
                (node_id, node_type),
                (1, NodeType::Drone),
                (2, NodeType::Drone),
                (3, NodeType::Drone),
                (4, NodeType::Client),
            ],
        };

        graph.insert_edges_from_path_trace(&flood_response.path_trace);

        let owner_node = create_arc_rwlock_node(node_id, node_type);
        let node_1 = create_arc_rwlock_node(1, NodeType::Drone);
        let node_2 = create_arc_rwlock_node(2, NodeType::Drone);
        let node_3 = create_arc_rwlock_node(3, NodeType::Drone);
        let node_4 = create_arc_rwlock_node(4, NodeType::Client);

        owner_node
            .write()
            .unwrap()
            .neighbors
            .write()
            .unwrap()
            .push(1);
        node_1.write().unwrap().neighbors.write().unwrap().push(0);
        node_1.write().unwrap().neighbors.write().unwrap().push(2);
        node_2.write().unwrap().neighbors.write().unwrap().push(1);
        node_2.write().unwrap().neighbors.write().unwrap().push(3);
        node_3.write().unwrap().neighbors.write().unwrap().push(2);
        node_3.write().unwrap().neighbors.write().unwrap().push(4);
        node_4.write().unwrap().neighbors.write().unwrap().push(3);

        let expected = NetworkGraph {
            owner_node_id: node_id,
            owner_node_type: node_type,
            nodes: RwLock::new(vec![
                owner_node.clone(),
                node_1.clone(),
                node_2.clone(),
                node_3.clone(),
                node_4.clone(),
            ]),
        };

        assert_eq!(graph, expected);
    }

    #[test]
    fn add_bidirectional_graph() {
        let node_id = 0;
        let node_type = NodeType::Server;
        let graph = NetworkGraph::new(node_id, node_type);

        graph.insert_node_if_not_present(1, NodeType::Drone);
        graph.insert_node_if_not_present(2, NodeType::Drone);

        let expected = NetworkGraph {
            owner_node_id: node_id,
            owner_node_type: node_type,
            nodes: RwLock::new(vec![
                create_arc_rwlock_node(node_id, node_type),
                create_arc_rwlock_node(1, NodeType::Drone),
                create_arc_rwlock_node(2, NodeType::Drone),
            ]),
        };

        assert_eq!(graph, expected);

        graph.insert_bidirectional_edge(1, 2);

        let node_1 = graph.nodes.read().unwrap()[1].clone();
        let node_2 = graph.nodes.read().unwrap()[2].clone();

        let expected_1 = create_arc_rwlock_node(1, NodeType::Drone);
        expected_1
            .write()
            .unwrap()
            .neighbors
            .write()
            .unwrap()
            .push(2);

        let expected_2 = create_arc_rwlock_node(2, NodeType::Drone);
        expected_2
            .write()
            .unwrap()
            .neighbors
            .write()
            .unwrap()
            .push(1);

        assert_eq!(&*node_1.read().unwrap(), &*expected_1.read().unwrap());
        assert_eq!(&*node_2.read().unwrap(), &*expected_2.read().unwrap());
    }

    #[test]
    fn delete_edge() {
        let node_id = 0;
        let node_type = NodeType::Server;
        let graph = NetworkGraph::new(node_id, node_type);

        graph.insert_node_if_not_present(1, NodeType::Drone);
        graph.insert_node_if_not_present(2, NodeType::Drone);

        let expected = NetworkGraph {
            owner_node_id: node_id,
            owner_node_type: node_type,
            nodes: RwLock::new(vec![
                create_arc_rwlock_node(node_id, node_type),
                create_arc_rwlock_node(1, NodeType::Drone),
                create_arc_rwlock_node(2, NodeType::Drone),
            ]),
        };

        assert_eq!(graph, expected);

        graph.insert_bidirectional_edge(1, 2);

        let node_1 = graph.nodes.read().unwrap()[1].clone();
        let node_2 = graph.nodes.read().unwrap()[2].clone();

        let expected_1 = create_arc_rwlock_node(1, NodeType::Drone);
        expected_1
            .write()
            .unwrap()
            .neighbors
            .write()
            .unwrap()
            .push(2);

        let expected_2 = create_arc_rwlock_node(2, NodeType::Drone);
        expected_2
            .write()
            .unwrap()
            .neighbors
            .write()
            .unwrap()
            .push(1);

        assert_eq!(&*node_1.read().unwrap(), &*expected_1.read().unwrap());
        assert_eq!(&*node_2.read().unwrap(), &*expected_2.read().unwrap());

        graph.delete_bidirectional_edge(1, 2);
        let expected_1 = create_arc_rwlock_node(1, NodeType::Drone);
        let expected_2 = create_arc_rwlock_node(2, NodeType::Drone);

        assert_eq!(&*node_1.read().unwrap(), &*expected_1.read().unwrap());
        assert_eq!(&*node_2.read().unwrap(), &*expected_2.read().unwrap());
    }

    #[test]
    fn get_path_to_node() {
        let node_id = 0;
        let node_type = NodeType::Server;
        let graph = NetworkGraph::new(node_id, node_type);

        let flood_response = FloodResponse {
            flood_id: 0,
            path_trace: vec![
                (node_id, node_type),
                (1, NodeType::Drone),
                (2, NodeType::Drone),
                (3, NodeType::Drone),
                (4, NodeType::Client),
            ],
        };

        graph.insert_edges_from_path_trace(&flood_response.path_trace);

        let hops = graph.get_path_to(100);
        assert_eq!(hops, None);

        let hops = graph.get_path_to(4);
        let expected = Some(vec![0, 1, 2, 3, 4]);
        assert_eq!(hops, expected);

        graph.insert_bidirectional_edge(1, 4);
        let hops = graph.get_path_to(4);
        let expected = Some(vec![0, 1, 4]);
        assert_eq!(hops, expected);
    }

    #[test]
    fn get_path_to_should_return_none() {
        let node_id = 0;
        let node_type = NodeType::Server;
        let graph = NetworkGraph::new(node_id, node_type);

        let flood_response = FloodResponse {
            flood_id: 0,
            path_trace: vec![
                (node_id, node_type),
                (1, NodeType::Drone),
                (2, NodeType::Client),
                (3, NodeType::Drone),
                (4, NodeType::Client),
            ],
        };

        graph.insert_edges_from_path_trace(&flood_response.path_trace);

        let hops = graph.get_path_to(4);
        assert_eq!(hops, None);
    }

    #[test]
    fn check_reset_graph() {
        let node_id = 0;
        let node_type = NodeType::Server;
        let mut graph = NetworkGraph::new(node_id, node_type);

        let owner_node = create_arc_rwlock_node(node_id, node_type);
        let nodes = RwLock::new(vec![owner_node.clone()]);

        let expected = NetworkGraph {
            owner_node_id: node_id,
            owner_node_type: node_type,
            nodes,
        };

        assert_eq!(graph, expected);

        graph.reset_graph();

        assert_eq!(graph, expected);

        let flood_response = FloodResponse {
            flood_id: 0,
            path_trace: vec![(node_id, node_type), (1, NodeType::Drone)],
        };

        graph.insert_edges_from_path_trace(&flood_response.path_trace);

        let owner_node = create_arc_rwlock_node(node_id, node_type);
        owner_node
            .write()
            .unwrap()
            .neighbors
            .write()
            .unwrap()
            .push(1);
        let drone_1 = create_arc_rwlock_node(1, NodeType::Drone);
        drone_1
            .write()
            .unwrap()
            .neighbors
            .write()
            .unwrap()
            .push(node_id);
        let nodes = RwLock::new(vec![owner_node, drone_1]);
        let expected = NetworkGraph {
            owner_node_id: node_id,
            owner_node_type: node_type,
            nodes,
        };

        assert_eq!(graph, expected);

        graph.reset_graph();

        let owner_node = create_arc_rwlock_node(node_id, node_type);
        let nodes = RwLock::new(vec![owner_node]);

        let expected = NetworkGraph {
            owner_node_id: node_id,
            owner_node_type: node_type,
            nodes,
        };

        assert_eq!(graph, expected);
    }

    fn create_arc_rwlock_node(node_id: NodeId, node_type: NodeType) -> Arc<RwLock<NetworkNode>> {
        Arc::new(RwLock::new(NetworkNode::new(node_id, node_type)))
    }
}
