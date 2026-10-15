//! Debug builds only: `BARDO_DEMO=1` shows made-up servers instead of connecting, to work on the UI
//! without an account. `BARDO_DEMO=login` shows the sign-in screen instead, and `BARDO_DEMO=call` an
//! incoming call. Nothing is saved.

use std::collections::HashSet;

use bardo_core::{
    CallView, ChannelView, FriendInVoice, GuildView, Id, Moderation, Participant, PrivateChannelView, Snapshot,
    VoiceStatus, VoiceView,
};

pub fn enabled() -> bool {
    std::env::var_os("BARDO_DEMO").is_some()
}

pub fn login() -> bool {
    std::env::var_os("BARDO_DEMO").is_some_and(|value| value == "login")
}

pub fn speaking() -> HashSet<Id> {
    HashSet::from([Id(101)])
}

/// The demo's servers to pin, like a user who already picked theirs.
pub fn pinned() -> Vec<Id> {
    vec![Id(1)]
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
        is_friend: false,
        moderation: all,
    };
    let friend = |id: u64, name: &str| Participant { is_friend: true, ..participant(id, name) };
    let stranger = |id: u64, name: &str| Participant { moderation: Moderation::default(), ..participant(id, name) };
    let channel = |id: u64, name: &str, participants: Vec<Participant>| ChannelView {
        id: Id(id),
        name: name.to_owned(),
        user_limit: 0,
        can_connect: true,
        participants,
    };
    let limited = |id: u64, name: &str, limit: u32, participants: Vec<Participant>| ChannelView {
        user_limit: limit,
        ..channel(id, name, participants)
    };

    let guilds = vec![
        GuildView {
            id: Id(1),
            name: "La Dictadura".to_owned(),
            channels: vec![
                limited(10, "Contador Público", 20, vec![
                    Participant { is_me: true, ..participant(100, "LuchovG") },
                    friend(101, "Fanta"),
                    Participant { server_mute: true, ..friend(102, "Santijaja") },
                    Participant { streaming: true, ..friend(103, "Titino") },
                ]),
                limited(11, "Ministerio de Counter", 20, vec![Participant {
                    self_deaf: true,
                    self_mute: true,
                    ..friend(104, "Dante")
                }]),
                limited(12, "Campo prendido", 67, Vec::new()),
                channel(13, "Escuela", Vec::new()),
                channel(14, "AFK", Vec::new()),
            ],
        },
        GuildView {
            id: Id(2),
            name: "GTA MONEY DROPS!".to_owned(),
            channels: vec![
                channel(20, "Social 1", vec![stranger(110, "fanta"), stranger(111, "xX_Drop_Xx")]),
                channel(21, "Social 2", Vec::new()),
                limited(22, "Crew 1", 4, Vec::new()),
                limited(23, "Crew 2", 4, Vec::new()),
            ],
        },
        GuildView {
            id: Id(3),
            name: "CS2 Argentina".to_owned(),
            channels: (0..12)
                .map(|n| {
                    let people = match n {
                        0 => vec![stranger(120, "Nico"), stranger(121, "Rama"), friend(122, "Mati")],
                        3 => vec![stranger(123, "pibe_awp"), stranger(124, "elcapo")],
                        _ => Vec::new(),
                    };
                    limited(30 + n, &format!("Mix #{}", n + 1), 5, people)
                })
                .collect(),
        },
        GuildView {
            id: Id(4),
            name: "Rust en Español".to_owned(),
            channels: vec![channel(40, "Charla", Vec::new()), channel(41, "Pair programming", Vec::new())],
        },
    ];

    let mut friends_in_voice = Vec::new();
    for guild in &guilds {
        for channel in &guild.channels {
            for participant in channel.participants.iter().filter(|p| p.is_friend) {
                friends_in_voice.push(FriendInVoice {
                    user_id: participant.user_id,
                    name: participant.name.clone(),
                    guild_id: guild.id,
                    guild_name: guild.name.clone(),
                    channel_id: channel.id,
                    channel_name: channel.name.clone(),
                    can_connect: channel.can_connect,
                    self_mute: participant.self_mute,
                    self_deaf: participant.self_deaf,
                    streaming: participant.streaming,
                });
            }
        }
    }

    let calls = vec![
        CallView {
            channel_id: Id(60),
            name: "Dante".to_owned(),
            group: false,
            participants: vec![Participant { moderation: Moderation::default(), ..friend(104, "Dante") }],
            ringing_me: std::env::var_os("BARDO_DEMO").is_some_and(|value| value == "call"),
        },
        CallView {
            channel_id: Id(61),
            name: "Los pibes".to_owned(),
            group: true,
            participants: vec![stranger(120, "Nico"), stranger(121, "Rama")],
            ringing_me: false,
        },
    ];
    let private_channels = vec![
        PrivateChannelView { id: Id(60), name: "Dante".to_owned(), group: false, recipients: vec![Id(104)] },
        PrivateChannelView { id: Id(61), name: "Los pibes".to_owned(), group: true, recipients: vec![Id(120), Id(121)] },
        PrivateChannelView { id: Id(62), name: "Fanta".to_owned(), group: false, recipients: vec![Id(101)] },
    ];

    Snapshot {
        guilds,
        calls,
        private_channels,
        voice: Some(VoiceView {
            guild_id: Some(Id(1)),
            channel_id: Id(10),
            guild_name: "La Dictadura".to_owned(),
            channel_name: "Contador Público".to_owned(),
            status: VoiceStatus::Connected,
            self_mute: false,
            self_deaf: false,
        }),
        self_mute: false,
        self_deaf: false,
        friends_in_voice,
        me: Some((Id(100), "LuchovG".to_owned())),
    }
}
