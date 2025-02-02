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
    source_id: NodeId,
    session_id: u64,
    gateway: Arc<Gateway>,
    network_controller: Arc<NetworkController>,
    backoff_time: Duration,
}

impl SinglePacketTransmissionHandler {
    /// Returns a new instance of `SinglePacketTransmissionHandler`
    pub fn new(packet_type: PacketType, source_id: NodeId, session_id: u64, gateway: Arc<Gateway>, network_controller: Arc<NetworkController>, backoff_time: Duration) -> Self {
        Self { packet_type, source_id, session_id, gateway, network_controller, backoff_time }
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
            } else {
                thread::sleep(self.backoff_time);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // TODO: add tests
}