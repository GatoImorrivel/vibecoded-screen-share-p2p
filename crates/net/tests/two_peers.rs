use bytes::Bytes;
use screenshare_net::protocol::VideoHeader;
use screenshare_net::{NetEvent, Session, ShareKind, Ticket, VideoPacket};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;

async fn wait_for<T>(
    rx: &mut UnboundedReceiver<NetEvent>,
    mut f: impl FnMut(&NetEvent) -> Option<T>,
) -> T {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let ev = rx.recv().await.expect("event channel closed");
            if let Some(v) = f(&ev) {
                return v;
            }
        }
    })
    .await
    .expect("timed out waiting for event")
}

#[tokio::test(flavor = "multi_thread")]
async fn presence_share_and_media() {
    let (alice, mut alice_rx) = Session::create("alice".into()).await.unwrap();
    let ticket: Ticket = alice.ticket().parse().unwrap();
    let (bob, mut bob_rx) = Session::join(&ticket, "bob".into()).await.unwrap();

    let alice_id = alice.id();
    wait_for(&mut bob_rx, |ev| match ev {
        NetEvent::PeerUpdated { peer, nickname, .. } if *peer == alice_id => {
            assert_eq!(nickname, "alice");
            Some(())
        }
        _ => None,
    })
    .await;

    let share = alice.start_share(
        ShareKind::Monitor,
        "Screen 1".into(),
        640,
        480,
        true,
        (2_000_000, 500_000, 4_000_000),
    );
    let stream_id = wait_for(&mut bob_rx, |ev| match ev {
        NetEvent::PeerUpdated {
            share: Some(info), ..
        } => Some(info.stream_id),
        _ => None,
    })
    .await;
    assert_eq!(stream_id, share.stream_id());

    bob.subscribe(alice_id, stream_id);
    // Wait until alice sees the subscriber, then send a keyframe and audio.
    tokio::time::timeout(Duration::from_secs(30), async {
        while share.subscriber_count() == 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("subscriber never registered");

    let payload = Bytes::from(vec![7u8; 200_000]);
    for i in 0..3u64 {
        share.send_video(VideoPacket {
            header: VideoHeader {
                stream_id,
                pts_us: i * 33_000,
                keyframe: true,
                width: 640,
                height: 480,
            },
            data: payload.clone(),
        });
        share.send_audio(i * 20_000, vec![1, 2, 3]);
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    let size = wait_for(&mut bob_rx, |ev| match ev {
        NetEvent::Video { packet, .. } => Some(packet.data.len()),
        _ => None,
    })
    .await;
    assert_eq!(size, payload.len());

    drop(share);
    wait_for(&mut bob_rx, |ev| match ev {
        NetEvent::PeerUpdated { share: None, .. } => Some(()),
        _ => None,
    })
    .await;

    bob.leave().await;
    wait_for(&mut alice_rx, |ev| match ev {
        NetEvent::PeerLeft { .. } => Some(()),
        _ => None,
    })
    .await;
    alice.leave().await;
}
