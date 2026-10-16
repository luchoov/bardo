//! Debug builds only: `BARDO_DEMO=1` shows made-up servers instead of connecting, to work on the UI
//! without an account. `BARDO_DEMO=login` shows the sign-in screen instead, `BARDO_DEMO=call` an
//! incoming call, `BARDO_DEMO=update` the bar for a newer version, `BARDO_DEMO=settings` the
//! settings, `BARDO_DEMO=search` a search, `BARDO_DEMO=tour` people joining and talking and a search
//! being typed, and `BARDO_DEMO=sounds` plays people coming and going and a call we make, to hear
//! every sound. Nothing is saved. `tools/screenshots.ps1` uses these.

use std::{collections::HashSet, time::Duration};

use bardo_core::{
    CallView, ChannelView, FriendInVoice, GuildView, Id, Moderation, Participant, PrivateChannelView, Snapshot,
    Sound, VoiceStatus, VoiceView,
};

pub fn enabled() -> bool {
    std::env::var_os("BARDO_DEMO").is_some()
}

pub fn login() -> bool {
    std::env::var_os("BARDO_DEMO").is_some_and(|value| value == "login")
}

pub fn update() -> bool {
    std::env::var_os("BARDO_DEMO").is_some_and(|value| value == "update")
}

pub fn settings() -> bool {
    std::env::var_os("BARDO_DEMO").is_some_and(|value| value == "settings")
}

/// What `BARDO_DEMO=search` types into the search field.
pub fn search() -> Option<&'static str> {
    std::env::var_os("BARDO_DEMO").is_some_and(|value| value == "search").then_some("mix")
}

pub fn tour() -> bool {
    std::env::var_os("BARDO_DEMO").is_some_and(|value| value == "tour")
}

pub fn sounds() -> bool {
    std::env::var_os("BARDO_DEMO").is_some_and(|value| value == "sounds")
}

/// One step of the tour, shown for the time next to it.
pub enum TourStep {
    Snapshot(Box<Snapshot>),
    Speaking(&'static [u64]),
    Search(&'static str),
}

/// Two people join our channel and talk, then a search is typed and cleared. Silent: it is recorded
/// for the README. No calls, so our channel shows near the top.
pub fn tour_script() -> Vec<(TourStep, Duration)> {
    let mut base = snapshot();
    base.calls.clear();
    let joined = |people: &[(u64, &str)]| {
        let mut snapshot = base.clone();
        let channel = snapshot.guilds[0].channels.iter_mut().find(|c| c.id == OUR_CHANNEL).unwrap();
        let template = channel.participants[1].clone();
        channel.participants.extend(
            people.iter().map(|&(id, name)| Participant { user_id: Id(id), name: name.to_owned(), ..template.clone() }),
        );
        TourStep::Snapshot(Box::new(snapshot))
    };
    let ms = Duration::from_millis;
    vec![
        (TourStep::Snapshot(Box::new(base.clone())), ms(1000)),
        (joined(&[(140, "paquito")]), ms(900)),
        (joined(&[(140, "paquito"), (141, "SchnitzelLord")]), ms(900)),
        (TourStep::Speaking(&[140]), ms(900)),
        (TourStep::Speaking(&[101]), ms(700)),
        (TourStep::Speaking(&[101, 141]), ms(900)),
        (TourStep::Speaking(&[]), ms(1000)),
        (TourStep::Search("p"), ms(250)),
        (TourStep::Search("pe"), ms(250)),
        (TourStep::Search("pep"), ms(2000)),
        (TourStep::Search(""), ms(1200)),
    ]
}

/// One step of the sound demo, a second or so apart.
pub enum Step {
    /// What the client would send for something that happened to us.
    Sound(Sound),
    /// A new snapshot; the sounds for who came and went are worked out from it.
    Snapshot(Box<Snapshot>),
    Wait,
}

const OUR_CHANNEL: Id = Id(10);
const FANTA_DM: Id = Id(62);

/// We join, people come and go (two at once, which must not stack up), our own cues play, then we
/// call fantaakd, who answers after a few rings and hangs up.
pub fn sound_script() -> Vec<Step> {
    let base = snapshot();
    let with_channel = |update: &dyn Fn(&mut Vec<Participant>)| {
        let mut snapshot = base.clone();
        let channel = snapshot.guilds[0].channels.iter_mut().find(|c| c.id == OUR_CHANNEL).unwrap();
        update(&mut channel.participants);
        snapshot
    };
    let newcomer = |id: u64, name: &str| Participant {
        user_id: Id(id),
        name: name.to_owned(),
        ..base.guilds[0].channels[0].participants[1].clone()
    };
    let paquito = with_channel(&|people| people.push(newcomer(140, "paquito")));
    let crowd = with_channel(&|people| {
        people.push(newcomer(140, "paquito"));
        people.push(newcomer(141, "SchnitzelLord"));
        people.push(newcomer(142, "Jürgen"));
    });
    let fanta_left = with_channel(&|people| {
        people.extend([newcomer(140, "paquito"), newcomer(141, "SchnitzelLord"), newcomer(142, "Jürgen")]);
        people.retain(|p| p.user_id != Id(101));
    });

    let call = |others: &[(u64, &str)], ringing_others: bool| {
        let mut snapshot = base.clone();
        let me = base.guilds[0].channels[0].participants[0].clone();
        snapshot.voice = Some(VoiceView {
            guild_id: None,
            channel_id: FANTA_DM,
            guild_name: "Call".to_owned(),
            channel_name: "fantaakd".to_owned(),
            status: VoiceStatus::Connected,
            self_mute: false,
            self_deaf: false,
        });
        let mut participants = vec![me];
        participants.extend(others.iter().map(|&(id, name)| newcomer(id, name)));
        snapshot.calls.push(CallView {
            channel_id: FANTA_DM,
            name: "fantaakd".to_owned(),
            group: false,
            participants,
            ringing_me: false,
            ringing_others,
        });
        snapshot
    };

    let mut steps = vec![
        Step::Sound(Sound::Join),
        Step::Snapshot(Box::new(base.clone())),
        Step::Snapshot(Box::new(paquito)),
        Step::Snapshot(Box::new(crowd)),
        Step::Snapshot(Box::new(fanta_left)),
        Step::Wait,
    ];
    steps.extend(
        [
            Sound::Mute,
            Sound::Unmute,
            Sound::Deafen,
            Sound::Undeafen,
            Sound::PushToTalkOn,
            Sound::PushToTalkOff,
            Sound::Moved,
            Sound::Disconnected,
            Sound::Leave,
        ]
        .map(Step::Sound),
    );
    steps.extend([
        Step::Wait,
        Step::Snapshot(Box::new(call(&[], false))),
        Step::Snapshot(Box::new(call(&[], true))),
        Step::Wait,
        Step::Wait,
        Step::Wait,
        Step::Wait,
        Step::Wait,
        Step::Snapshot(Box::new(call(&[(101, "fantaakd")], false))),
        Step::Wait,
        Step::Snapshot(Box::new(call(&[], false))),
        Step::Wait,
        Step::Snapshot(Box::new(base)),
    ]);
    steps
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
        avatar: None,
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
            icon: None,
            channels: vec![
                limited(10, "Contador Público", 20, vec![
                    Participant { is_me: true, ..participant(100, "LuchovG") },
                    friend(101, "fantaakd"),
                    Participant { server_mute: true, ..friend(102, "netanyahu") },
                    Participant { streaming: true, ..friend(103, "Titino") },
                ]),
                limited(11, "Ministerio de Counter", 20, vec![Participant {
                    self_deaf: true,
                    self_mute: true,
                    ..friend(104, "brucman")
                }]),
                limited(12, "Campo prendido", 67, Vec::new()),
                channel(13, "Escuela", Vec::new()),
                channel(14, "AFK", Vec::new()),
            ],
        },
        GuildView {
            id: Id(2),
            name: "Schnitzel Squad".to_owned(),
            icon: None,
            channels: vec![
                channel(20, "Lounge", vec![stranger(110, "Klausi"), stranger(111, "mic_off_again")]),
                channel(21, "Beer garden", Vec::new()),
                limited(22, "Raid 1", 4, Vec::new()),
                limited(23, "Raid 2", 4, Vec::new()),
            ],
        },
        GuildView {
            id: Id(3),
            name: "Clutch or Kick".to_owned(),
            icon: None,
            channels: (0..12)
                .map(|n| {
                    let people = match n {
                        0 => vec![stranger(120, "Hans_AWP"), stranger(121, "Wurstmeister"), friend(122, "pepe")],
                        3 => vec![stranger(123, "touchgrass"), stranger(124, "Kevin.exe")],
                        _ => Vec::new(),
                    };
                    limited(30 + n, &format!("Mix #{}", n + 1), 5, people)
                })
                .collect(),
        },
        GuildView {
            id: Id(4),
            name: "Rustaceans".to_owned(),
            icon: None,
            channels: vec![channel(40, "General", Vec::new()), channel(41, "Pair programming", Vec::new())],
        },
    ];

    let mut friends_in_voice = Vec::new();
    for guild in &guilds {
        for channel in &guild.channels {
            for participant in channel.participants.iter().filter(|p| p.is_friend) {
                friends_in_voice.push(FriendInVoice {
                    user_id: participant.user_id,
                    name: participant.name.clone(),
                    avatar: participant.avatar.clone(),
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
            name: "brucman".to_owned(),
            group: false,
            participants: vec![Participant { moderation: Moderation::default(), ..friend(104, "brucman") }],
            ringing_me: std::env::var_os("BARDO_DEMO").is_some_and(|value| value == "call"),
            ringing_others: false,
        },
        CallView {
            channel_id: Id(61),
            name: "Saturday squad".to_owned(),
            group: true,
            participants: vec![stranger(140, "paquito"), stranger(110, "Klausi")],
            ringing_me: false,
            ringing_others: false,
        },
    ];
    let private_channels = vec![
        PrivateChannelView { id: Id(60), name: "brucman".to_owned(), group: false, recipients: vec![Id(104)] },
        PrivateChannelView {
            id: Id(61),
            name: "Saturday squad".to_owned(),
            group: true,
            recipients: vec![Id(140), Id(110)],
        },
        PrivateChannelView { id: Id(62), name: "fantaakd".to_owned(), group: false, recipients: vec![Id(101)] },
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
        me_avatar: None,
    }
}
