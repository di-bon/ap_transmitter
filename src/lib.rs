#![allow(clippy::struct_field_names)]
#![allow(clippy::too_many_arguments)]

mod transmitter;

use wg_2024::network::NodeId;
use wg_2024::packet::{Ack, FloodRequest, FloodResponse, Nack};
pub use transmitter::{Command, Transmitter};

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