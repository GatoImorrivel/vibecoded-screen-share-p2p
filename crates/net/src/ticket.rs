use anyhow::{Context, Result};
use iroh::EndpointAddr;
use iroh_gossip::proto::TopicId;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Everything needed to join a session: the gossip topic and some members to dial.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ticket {
    pub topic: TopicId,
    pub peers: Vec<EndpointAddr>,
}

impl Ticket {
    pub fn new_topic() -> TopicId {
        TopicId::from_bytes(rand::random())
    }
}

impl fmt::Display for Ticket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bytes = postcard::to_stdvec(self).expect("ticket serialization is infallible");
        let mut text = data_encoding::BASE32_NOPAD.encode(&bytes);
        text.make_ascii_lowercase();
        f.write_str(&text)
    }
}

impl FromStr for Ticket {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        let bytes = data_encoding::BASE32_NOPAD
            .decode(cleaned.to_ascii_uppercase().as_bytes())
            .context("invite code is not valid base32")?;
        postcard::from_bytes(&bytes).context("invite code is malformed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let key = iroh::SecretKey::generate();
        let ticket = Ticket {
            topic: Ticket::new_topic(),
            peers: vec![EndpointAddr::new(key.public())],
        };
        let text = ticket.to_string();
        let parsed: Ticket = format!("  {text}\n").parse().unwrap();
        assert_eq!(parsed.topic, ticket.topic);
        assert_eq!(parsed.peers[0].id, key.public());
    }
}
