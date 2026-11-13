use anyhow::{Context, Result, bail};
use bytes::Bytes;
use iroh::endpoint::{RecvStream, SendStream};
use iroh::{EndpointAddr, PublicKey, SecretKey, Signature};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub const MEDIA_ALPN: &[u8] = b"screenshare/media/2";

/// Largest encoded video frame accepted from a peer.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const MAX_CONTROL_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShareKind {
    Monitor,
    Window,
}

/// Announced description of a peer's active share.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareInfo {
    pub stream_id: u64,
    pub kind: ShareKind,
    pub title: String,
    pub width: u32,
    pub height: u32,
    pub has_audio: bool,
    /// Video encoder backend, e.g. "NVENC" or "software". Informational; the
    /// bitstream is always H.264.
    pub codec: String,
}

/// What a new share announces; see [`crate::Session::start_share`].
#[derive(Debug, Clone)]
pub struct ShareDetails {
    pub kind: ShareKind,
    pub title: String,
    pub width: u32,
    pub height: u32,
    pub has_audio: bool,
    pub codec: String,
}

/// Presence broadcast over gossip on join, on change and as a heartbeat.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Presence {
    pub nickname: String,
    pub share: Option<ShareInfo>,
    pub addr: EndpointAddr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GossipMsg {
    Presence(Presence),
    Leave,
}

#[derive(Debug, Serialize, Deserialize)]
struct Envelope {
    nonce: u64,
    msg: GossipMsg,
}

/// A gossip message signed by its author, since gossip itself is unauthenticated.
#[derive(Debug, Serialize, Deserialize)]
struct Signed {
    from: PublicKey,
    data: Vec<u8>,
    signature: Vec<u8>,
}

pub fn sign_gossip(secret: &SecretKey, msg: GossipMsg) -> Result<Bytes> {
    // A fresh nonce prevents gossip from deduplicating repeated heartbeats.
    let data = postcard::to_stdvec(&Envelope {
        nonce: rand::random(),
        msg,
    })?;
    let signature = secret.sign(&data).to_bytes().to_vec();
    let signed = Signed {
        from: secret.public(),
        data,
        signature,
    };
    Ok(postcard::to_stdvec(&signed)?.into())
}

pub fn verify_gossip(bytes: &[u8]) -> Result<(PublicKey, GossipMsg)> {
    let signed: Signed = postcard::from_bytes(bytes).context("decoding signed message")?;
    let sig: [u8; Signature::LENGTH] = signed
        .signature
        .as_slice()
        .try_into()
        .context("bad signature length")?;
    signed
        .from
        .verify(&signed.data, &Signature::from_bytes(&sig))
        .context("bad signature")?;
    let env: Envelope = postcard::from_bytes(&signed.data).context("decoding message")?;
    Ok((signed.from, env.msg))
}

/// Messages a viewer sends to a sharer on the control stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlMsg {
    Subscribe { stream_id: u64 },
    Unsubscribe { stream_id: u64 },
    RequestKeyframe { stream_id: u64 },
}

/// Header preceding the payload on each per-frame unidirectional stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoHeader {
    pub stream_id: u64,
    pub pts_us: u64,
    pub keyframe: bool,
    pub width: u32,
    pub height: u32,
}

/// One encoded video frame as received from or sent to the network.
#[derive(Debug, Clone)]
pub struct VideoPacket {
    pub header: VideoHeader,
    pub data: Bytes,
}

/// One 20 ms Opus packet, sent as a single QUIC datagram.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioPacket {
    pub stream_id: u64,
    pub seq: u32,
    pub pts_us: u64,
    pub data: Vec<u8>,
}

pub async fn write_framed<T: Serialize>(send: &mut SendStream, msg: &T) -> Result<()> {
    let bytes = postcard::to_stdvec(msg)?;
    send.write_all(&(bytes.len() as u32).to_le_bytes()).await?;
    send.write_all(&bytes).await?;
    Ok(())
}

pub async fn read_framed<T: DeserializeOwned>(recv: &mut RecvStream) -> Result<Option<T>> {
    let mut len = [0u8; 4];
    match recv.read_exact(&mut len).await {
        Ok(()) => {}
        Err(iroh::endpoint::ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_CONTROL_BYTES {
        bail!("control message too large: {len}");
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await?;
    Ok(Some(postcard::from_bytes(&buf)?))
}

pub async fn write_video(send: &mut SendStream, packet: &VideoPacket) -> Result<()> {
    write_framed(send, &packet.header).await?;
    send.write_chunk(packet.data.clone()).await?;
    send.finish()?;
    Ok(())
}

pub async fn read_video(mut recv: RecvStream) -> Result<VideoPacket> {
    let header: VideoHeader = read_framed(&mut recv)
        .await?
        .context("video stream ended before header")?;
    let data = recv.read_to_end(MAX_FRAME_BYTES).await?;
    Ok(VideoPacket {
        header,
        data: data.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gossip_sign_verify() {
        let key = SecretKey::generate();
        let msg = GossipMsg::Presence(Presence {
            nickname: "ana".into(),
            share: None,
            addr: EndpointAddr::new(key.public()),
        });
        let bytes = sign_gossip(&key, msg).unwrap();
        let (from, msg) = verify_gossip(&bytes).unwrap();
        assert_eq!(from, key.public());
        assert!(matches!(msg, GossipMsg::Presence(p) if p.nickname == "ana"));

        let mut tampered = bytes.to_vec();
        let last = tampered.len() - 70;
        tampered[last] ^= 1;
        assert!(verify_gossip(&tampered).is_err());
    }
}
