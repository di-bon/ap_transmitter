use std::collections::HashSet;
use std::sync::RwLock;
use wg_2024::network::NodeId;
use wg_2024::packet::NodeType;

#[derive(Debug)]
pub struct NetworkNode {
    pub node_id: NodeId,
    pub node_type: NodeType,
    pub num_of_dropped_packets: u64,
    pub neighbors: RwLock<HashSet<NodeId>>,
}

impl PartialEq for NetworkNode {
    fn eq(&self, other: &Self) -> bool {
        let self_guard = self.neighbors.read().unwrap();
        let other_guard = other.neighbors.read().unwrap();

        self.node_id == other.node_id
            && self.node_type == other.node_type
            && self.num_of_dropped_packets == other.num_of_dropped_packets
            && *self_guard == *other_guard
    }
}

impl Eq for NetworkNode {}

impl NetworkNode {
    /// Returns a new instance of `NetworkNode`
    pub fn new(node_id: NodeId, node_type: NodeType) -> Self {
        Self {
            node_id,
            node_type,
            num_of_dropped_packets: 0,
            neighbors: RwLock::new(HashSet::new()),
        }
    }

    /// Inserts an edge from the current `NetworkNode` to the given `NodeId`
    pub fn insert_edge(&self, to: NodeId) {
        self.neighbors.write().unwrap().insert(to);
        log::info!("Inserted edge from {} to {to}", self.node_id);
    }

    /// Removes an edge from the current `NetworkNode` to the given `NodeId`
    pub fn remove_edge(&self, to: NodeId) {
        self.neighbors.write().unwrap().remove(&to);
    }

    /// Increments `self.num_of_dropped_packets` by `1`
    pub fn increment_dropped_packets(&mut self) {
        self.num_of_dropped_packets += 1;
        log::info!("num_of_dropped_packets incremented");
    }
}

#[cfg(test)]
mod tests {
    #![allow(unused_variables)]
    #![allow(unused_mut)]

    use super::*;

    #[test]
    fn initialize_no_neighbors() {
        let node_id = 0;
        let node_type = NodeType::Server;
        let node = NetworkNode::new(node_id, node_type);

        let expected = NetworkNode {
            node_id,
            node_type,
            num_of_dropped_packets: 0,
            neighbors: RwLock::new(HashSet::new()),
        };

        assert_eq!(node, expected);
    }

    #[test]
    fn add_neighbors() {
        let node_id = 0;
        let node_type = NodeType::Server;
        let node = NetworkNode::new(node_id, node_type);

        node.insert_edge(1);

        let mut neigh = HashSet::new();
        neigh.insert(1);
        let expected = NetworkNode {
            node_id,
            node_type,
            num_of_dropped_packets: 0,
            neighbors: RwLock::new(neigh),
        };

        assert_eq!(node, expected);
    }

    #[test]
    fn remove_neighbors() {
        let node_id = 0;
        let node_type = NodeType::Server;
        let node = NetworkNode::new(node_id, node_type);

        node.insert_edge(1);

        let mut neigh = HashSet::new();
        neigh.insert(1);
        let expected = NetworkNode {
            node_id,
            node_type,
            num_of_dropped_packets: 0,
            neighbors: RwLock::new(neigh),
        };

        assert_eq!(node, expected);

        node.remove_edge(1);

        let expected = NetworkNode {
            node_id,
            node_type,
            num_of_dropped_packets: 0,
            neighbors: RwLock::new(HashSet::new()),
        };

        assert_eq!(node, expected);
    }

    #[test]
    fn increment_dropped_packets() {
        let node_id = 0;
        let node_type = NodeType::Server;
        let mut node = NetworkNode::new(node_id, node_type);

        node.increment_dropped_packets();
        node.increment_dropped_packets();
        node.increment_dropped_packets();

        let expected = 3;

        assert_eq!(node.num_of_dropped_packets, expected);
    }
}
