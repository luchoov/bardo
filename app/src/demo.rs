//! Debug builds only: `BARDO_DEMO=1` shows made-up servers instead of connecting, to work on the UI
//! without an account.

use std::collections::HashSet;

use bardo_core::{ChannelView, GuildView, Id, Moderation, Participant, Snapshot, VoiceStatus, VoiceView};

pub fn enabled() -> bool {
    std::env::var_os("BARDO_DEMO").is_some()
}

pub fn speaking() -> HashSet<Id> {
    HashSet::from([Id(101)])
}

pub fn snapshot() -> Snapshot {
    let all = Moderation { mute: true, deafen: true, move_: true };
    let participant = |id: u64, name: &str| Participant {
        user_id: Id(id),
        name: name.to_owned(),
        self_mute: false,
        self_deaf: false,
        server_mute: false,
        server_deaf: false,
        streaming: false,
        video: false,
        is_me: false,
        moderation: all,
    };
    let channel = |id: u64, name: &str, participants: Vec<Participant>| ChannelView {
        id: Id(id),
        name: name.to_owned(),
        user_limit: 0,
        can_connect: true,
        participants,
    };

    Snapshot {
        guilds: vec![
            GuildView {
                id: Id(1),
                name: "Demo Server".to_owned(),
                channels: vec![
                    channel(10, "General", vec![
                        Participant { is_me: true, self_mute: true, ..participant(100, "me") },
                        participant(101, "Ana"),
                        Participant { server_mute: true, ..participant(102, "Bruno") },
                        Participant { streaming: true, ..participant(103, "Carla") },
                    ]),
                    channel(11, "Competitive", vec![Participant {
                        self_deaf: true,
                        self_mute: true,
                        ..participant(104, "Dante")
                    }]),
                    channel(12, "AFK", Vec::new()),
                ],
            },
            GuildView {
                id: Id(2),
                name: "Friends".to_owned(),
                channels: vec![channel(20, "Lobby", vec![Participant {
                    moderation: Moderation::default(),
                    ..participant(105, "Eva")
                }])],
            },
        ],
        voice: Some(VoiceView {
            guild_id: Id(1),
            channel_id: Id(10),
            guild_name: "Demo Server".to_owned(),
            channel_name: "General".to_owned(),
            status: VoiceStatus::Connected,
            self_mute: true,
            self_deaf: false,
        }),
    }
}
