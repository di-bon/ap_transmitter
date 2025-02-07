use std::sync::Arc;
use std::thread;
use std::time::Duration;
use wg_2024::network::{NodeId, SourceRoutingHeader};
use wg_2024::packet::{Packet, PacketType};
use crate::transmitter::gateway::Gateway;
use crate::transmitter::network_controller::NetworkController;

/// A `SinglePacketTransmissionHandler` struct that will send a single `Packet`, automatically
/// finding an appropriate `SourceRoutingHeader`
pub struct SinglePacketTransmissionHandler {
    packet_type: PacketType,
    session_id: u64,
    gateway: Arc<Gateway>,
    network_controller: Arc<NetworkController>,
    backoff_time: Duration,
}

impl SinglePacketTransmissionHandler {
    /// Returns a new instance of `SinglePacketTransmissionHandler`
    pub fn new(packet_type: PacketType, session_id: u64, gateway: Arc<Gateway>, network_controller: Arc<NetworkController>, backoff_time: Duration) -> Self {
        Self { packet_type, session_id, gateway, network_controller, backoff_time }
    }

    /// Sends a `Packet` to its `destination`
    pub fn send_packet(&self, destination: NodeId) {
        let source_routing_header = self.find_new_routing_header(destination);

       let packet = Packet {
           routing_header: source_routing_header,
           session_id: self.session_id,
           pack_type: self.packet_type.clone(),
       };

        self.gateway.forward(packet);
    }

    /// Returns a `SourceRoutingHeader` for the required `destination`
    fn find_new_routing_header(&self, destination: NodeId) -> SourceRoutingHeader {
        loop {
            let hops = self.network_controller.get_path(destination);
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
    use super::*;
    use std::collections::HashMap;
    use std::thread::spawn;
    use std::time::SystemTime;
    use ap_sc_notifier::SimulationControllerNotifier;
    use crossbeam_channel::unbounded;
    use ntest::timeout;
    use wg_2024::packet::{Ack, FloodResponse, NodeType};

    #[test]
    #[timeout(2000)]
    fn find_new_routing_header_after_backoff() -> std::thread::Result<()> {
        let node_id = 0;
        let node_type = NodeType::Server;

        let mut neighbors = HashMap::new();
        let neighbor_id = 5;
        let (tx, rx) = unbounded();
        neighbors.insert(neighbor_id, tx);

        let (gateway_to_transmitter_tx, gateway_to_transmitter_rx) = unbounded();

        let (sc_tx, sc_rx) = unbounded();

        let simulation_controller_notifier = SimulationControllerNotifier::new(sc_tx);
        let simulation_controller_notifier = Arc::new(simulation_controller_notifier);

        let gateway = Gateway::new(node_id, neighbors, gateway_to_transmitter_tx, simulation_controller_notifier.clone());
        let gateway = Arc::new(gateway);

        let network_controller = NetworkController::new(
            node_id,
            node_type,
            gateway.clone(),
            simulation_controller_notifier.clone()
        );
        let network_controller = Arc::new(network_controller);

        let packet_type = PacketType::Ack(Ack {
            fragment_index: 0,
        });
        let session_id = 1;

        let backoff_time = Duration::from_millis(100);
        let single_packet_transmission_handler = SinglePacketTransmissionHandler::new(
            packet_type,
            session_id,
            gateway.clone(),
            network_controller.clone(),
            backoff_time
        );

        let destination = 10;

        let handle = spawn(move || {
            let start = SystemTime::now();
            single_packet_transmission_handler.send_packet(destination);
            let elapsed = match start.elapsed() {
                Ok(elapsed) => elapsed,
                Err(error) => panic!("{error:?}"),
            };
            assert!(elapsed >= backoff_time);
        });

        thread::sleep(Duration::from_millis(20));

        let flood_response = FloodResponse {
            flood_id: 0,
            path_trace: vec![(node_id, node_type), (neighbor_id, NodeType::Drone), (destination, NodeType::Client)],
        };
        network_controller.update_from_flood_response(&flood_response);

        handle.join()
    }
}