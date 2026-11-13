//! Peer-to-peer session layer: tickets, gossip presence and the media protocol,
//! all on top of a single iroh endpoint.

mod media;
pub mod protocol;
mod session;
pub mod ticket;

pub use iroh::EndpointId as PeerId;
pub use media::{EncoderControl, ShareHandle};
pub use protocol::{AudioPacket, ShareDetails, ShareInfo, ShareKind, VideoPacket};
pub use session::{NetEvent, Session};
pub use ticket::Ticket;
