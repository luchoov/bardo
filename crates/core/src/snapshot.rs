//! UI-agnostic view of the state, rebuilt whenever the store changes.

use bardo_gateway::{Channel, Id, VoiceState, channel_kind};

use crate::{
    permissions::{self, CONNECT, DEAFEN_MEMBERS, MOVE_MEMBERS, MUTE_MEMBERS, VIEW_CHANNEL},
    store::{Guild, Store},
};

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub guilds: Vec<GuildView>,
    /// Our voice connection, if we are in (or joining) a channel.
    pub voice: Option<VoiceView>,
    /// Our own mute and deafen, which persist while we are not in voice.
    pub self_mute: bool,
    pub self_deaf: bool,
    /// Friends in voice channels we can see, in sidebar order.
    pub friends_in_voice: Vec<FriendInVoice>,
    /// Our own user id and name, once known.
    pub me: Option<(Id, String)>,
    /// Calls in DMs and groups that have someone in them or are ringing us.
    pub calls: Vec<CallView>,
    /// DMs and groups we can call, most recently active first.
    pub private_channels: Vec<PrivateChannelView>,
}

#[derive(Debug, Clone)]
pub struct CallView {
    pub channel_id: Id,
    pub name: String,
    pub group: bool,
    pub participants: Vec<Participant>,
    /// Someone is calling us.
    pub ringing_me: bool,
}

#[derive(Debug, Clone)]
pub struct PrivateChannelView {
    pub id: Id,
    pub name: String,
    pub group: bool,
    pub recipients: Vec<Id>,
}

#[derive(Debug, Clone)]
pub struct FriendInVoice {
    pub user_id: Id,
    pub name: String,
    pub guild_id: Id,
    pub guild_name: String,
    pub channel_id: Id,
    pub channel_name: String,
    pub can_connect: bool,
    pub self_mute: bool,
    pub self_deaf: bool,
    pub streaming: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VoiceStatus {
    #[default]
    Disconnected,
    Connecting,
    Connected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceView {
    /// `None` for DM and group calls.
    pub guild_id: Option<Id>,
    pub channel_id: Id,
    pub guild_name: String,
    pub channel_name: String,
    pub status: VoiceStatus,
    pub self_mute: bool,
    pub self_deaf: bool,
}

#[derive(Debug, Clone)]
pub struct GuildView {
    pub id: Id,
    pub name: String,
    pub channels: Vec<ChannelView>,
}

#[derive(Debug, Clone)]
pub struct ChannelView {
    pub id: Id,
    pub name: String,
    pub user_limit: u32,
    /// Whether we may join it (or move someone into it).
    pub can_connect: bool,
    pub participants: Vec<Participant>,
}

impl GuildView {
    /// How many people are in this guild's voice channels.
    pub fn voice_count(&self) -> usize {
        self.channels.iter().map(|channel| channel.participants.len()).sum()
    }
}

#[derive(Debug, Clone)]
pub struct Participant {
    pub user_id: Id,
    pub name: String,
    pub self_mute: bool,
    pub self_deaf: bool,
    pub server_mute: bool,
    pub server_deaf: bool,
    pub streaming: bool,
    pub video: bool,
    pub is_me: bool,
    pub is_friend: bool,
    /// Moderation actions we may take on this participant.
    pub moderation: Moderation,
}

/// Moderation actions allowed on a participant, from our permissions in their channel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Moderation {
    pub mute: bool,
    pub deafen: bool,
    /// Move to another channel or disconnect.
    pub move_: bool,
}

impl Moderation {
    pub fn from_permissions(permissions: u64) -> Self {
        Self {
            mute: permissions & MUTE_MEMBERS != 0,
            deafen: permissions & DEAFEN_MEMBERS != 0,
            move_: permissions & MOVE_MEMBERS != 0,
        }
    }

    pub fn any(self) -> bool {
        self.mute || self.deafen || self.move_
    }
}

impl Snapshot {
    pub fn build(store: &Store) -> Self {
        let Some(me) = &store.me else { return Self::default() };
        let guilds: Vec<GuildView> = store
            .guild_order
            .iter()
            .filter_map(|id| store.guilds.get(id))
            .filter(|guild| guild.available)
            .filter_map(|guild| {
                let channels = voice_channels(store, guild, me.id);
                (!channels.is_empty()).then(|| GuildView {
                    id: guild.id,
                    name: guild.name.clone(),
                    channels,
                })
            })
            .collect();
        let friends_in_voice = friends_in_voice(store, &guilds);
        Self {
            guilds,
            friends_in_voice,
            me: Some((me.id, me.display_name().to_owned())),
            calls: calls(store, me.id),
            private_channels: private_channels(store),
            ..Self::default()
        }
    }
}

fn calls(store: &Store, me: Id) -> Vec<CallView> {
    let mut calls: Vec<CallView> = store
        .calls
        .iter()
        .filter_map(|(&channel_id, call)| {
            let channel = store.private_channels.get(&channel_id);
            let ringing_me = call.ringing.contains(&me);
            let mut participants: Vec<Participant> =
                call.voice_states.values().map(|state| call_participant(store, state, me)).collect();
            participants.sort_by_cached_key(|p| p.name.to_lowercase());
            (ringing_me || !participants.is_empty()).then(|| CallView {
                channel_id,
                name: channel.map(|channel| store.private_channel_name(channel)).unwrap_or_else(|| "Call".to_owned()),
                group: channel.is_some_and(Store::is_group),
                participants,
                ringing_me,
            })
        })
        .collect();
    calls.sort_by_key(|call| call.channel_id);
    calls
}

fn call_participant(store: &Store, state: &VoiceState, me: Id) -> Participant {
    Participant {
        user_id: state.user_id,
        name: store.user_name(state.user_id),
        self_mute: state.self_mute,
        self_deaf: state.self_deaf,
        server_mute: state.mute,
        server_deaf: state.deaf,
        streaming: state.self_stream,
        video: state.self_video,
        is_me: state.user_id == me,
        is_friend: store.friends.contains_key(&state.user_id),
        moderation: Moderation::default(),
    }
}

fn private_channels(store: &Store) -> Vec<PrivateChannelView> {
    let mut channels: Vec<_> = store
        .private_channels
        .values()
        .filter(|channel| matches!(channel.kind, channel_kind::DM | channel_kind::GROUP_DM))
        .filter(|channel| !channel.recipient_ids.is_empty())
        .collect();
    channels.sort_by_key(|channel| std::cmp::Reverse(channel.last_message_id.unwrap_or(channel.id)));
    channels
        .into_iter()
        .map(|channel| PrivateChannelView {
            id: channel.id,
            name: store.private_channel_name(channel),
            group: Store::is_group(channel),
            recipients: channel.recipient_ids.clone(),
        })
        .collect()
}

fn friends_in_voice(store: &Store, guilds: &[GuildView]) -> Vec<FriendInVoice> {
    let mut friends = Vec::new();
    for guild in guilds {
        for channel in &guild.channels {
            for participant in channel.participants.iter().filter(|p| p.is_friend) {
                friends.push(FriendInVoice {
                    user_id: participant.user_id,
                    name: store.friend_name(participant.user_id).unwrap_or_else(|| participant.name.clone()),
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
    friends
}

fn voice_channels(store: &Store, guild: &Guild, me: Id) -> Vec<ChannelView> {
    // If our own member is missing we cannot evaluate overwrites; show everything rather than nothing.
    let can_evaluate = guild.members.contains_key(&me) || guild.owner_id == me;

    let mut channels: Vec<&Channel> = guild
        .channels
        .values()
        .filter(|c| c.kind == channel_kind::GUILD_VOICE)
        .filter(|c| !can_evaluate || permissions::channel_permissions(guild, me, c) & VIEW_CHANNEL != 0)
        .collect();
    channels.sort_by_key(|c| sort_key(guild, c));

    channels
        .into_iter()
        .map(|channel| {
            // Without our own member, assume we may connect but not moderate; Discord has the final word.
            let permissions = if can_evaluate {
                permissions::channel_permissions(guild, me, channel)
            } else {
                VIEW_CHANNEL | CONNECT
            };
            let moderation = Moderation::from_permissions(permissions);
            let mut participants: Vec<Participant> = guild
                .voice_states
                .values()
                .filter(|state| state.channel_id == Some(channel.id))
                .map(|state| Participant {
                    user_id: state.user_id,
                    name: store
                        .display_name(guild, state.user_id)
                        .unwrap_or_else(|| state.user_id.to_string()),
                    self_mute: state.self_mute,
                    self_deaf: state.self_deaf,
                    server_mute: state.mute,
                    server_deaf: state.deaf,
                    streaming: state.self_stream,
                    video: state.self_video,
                    is_me: state.user_id == me,
                    is_friend: store.friends.contains_key(&state.user_id),
                    moderation,
                })
                .collect();
            participants.sort_by_cached_key(|p| p.name.to_lowercase());

            ChannelView {
                id: channel.id,
                name: channel.name.clone().unwrap_or_default(),
                user_limit: channel.user_limit,
                can_connect: permissions & CONNECT != 0,
                participants,
            }
        })
        .collect()
}

/// Discord's sidebar order: uncategorized channels first, then by category, then by position.
fn sort_key(guild: &Guild, channel: &Channel) -> (Option<(i32, Id)>, i32, Id) {
    let category = channel
        .parent_id
        .and_then(|id| guild.channels.get(&id))
        .map(|category| (category.position, category.id));
    (category, channel.position, channel.id)
}

#[cfg(test)]
mod tests {
    use bardo_gateway::{Event, Ready, ReadySupplemental, overwrite_kind};

    use super::*;

    const ME: Id = Id(1);
    const GUILD: Id = Id(10);

    fn store() -> Store {
        let ready: Ready = serde_json::from_str(
            r#"{
                "user": {"id": "1", "username": "me"},
                "users": [],
                "guilds": [{
                    "id": "10",
                    "properties": {"id": "10", "name": "Guild", "owner_id": "99"},
                    "roles": [
                        {"id": "10", "name": "@everyone", "permissions": "1049600"},
                        {"id": "30", "name": "mods", "permissions": "0"}
                    ],
                    "channels": [
                        {"id": "20", "type": 4, "name": "Category B", "position": 1},
                        {"id": "25", "type": 4, "name": "Category A", "position": 0},
                        {"id": "21", "type": 2, "name": "in-b", "position": 0, "parent_id": "20"},
                        {"id": "22", "type": 2, "name": "top", "position": 5},
                        {"id": "23", "type": 2, "name": "hidden", "position": 1, "parent_id": "20",
                         "permission_overwrites": [{"id": "10", "type": 0, "allow": "0", "deny": "1024"}]},
                        {"id": "24", "type": 2, "name": "mods-only", "position": 2, "parent_id": "20",
                         "permission_overwrites": [
                            {"id": "10", "type": 0, "allow": "0", "deny": "1024"},
                            {"id": "30", "type": 0, "allow": "1024", "deny": "0"}
                         ]},
                        {"id": "26", "type": 2, "name": "in-a", "position": 0, "parent_id": "25"},
                        {"id": "27", "type": 0, "name": "text", "position": 0}
                    ]
                }],
                "merged_members": [[{"user_id": "1", "roles": ["30"]}]],
                "session_id": "s",
                "resume_gateway_url": "wss://r"
            }"#,
        )
        .unwrap();
        let supplemental: ReadySupplemental = serde_json::from_str(
            r#"{
                "guilds": [{"id": "10", "voice_states": [
                    {"user_id": "2", "channel_id": "21", "self_mute": true},
                    {"user_id": "3", "channel_id": "21", "mute": true}
                ]}],
                "merged_members": [[{"user": {"id": "2", "username": "zed"}, "nick": "Zed", "roles": []}]]
            }"#,
        )
        .unwrap();

        let mut store = Store::default();
        store.apply(Event::Ready(Box::new(ready)));
        store.apply(Event::ReadySupplemental(Box::new(supplemental)));
        store
    }

    #[test]
    fn lists_visible_voice_channels_in_sidebar_order() {
        let snapshot = Snapshot::build(&store());
        let names: Vec<&str> = snapshot.guilds[0].channels.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["top", "in-a", "in-b", "mods-only"]);
    }

    #[test]
    fn resolves_participants_and_falls_back_to_the_id() {
        let snapshot = Snapshot::build(&store());
        let channel = snapshot.guilds[0].channels.iter().find(|c| c.id == Id(21)).unwrap();
        let names: Vec<&str> = channel.participants.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["3", "Zed"]);
        assert!(channel.participants[0].server_mute);
        assert!(channel.participants[1].self_mute);
    }

    #[test]
    fn reports_voice_users_without_member_data() {
        assert_eq!(store().unresolved_voice_users(|_| true), vec![(GUILD, vec![Id(3)])]);
        assert!(store().unresolved_voice_users(|_| false).is_empty());
    }

    #[test]
    fn leaving_voice_removes_the_participant() {
        let mut store = store();
        let left = serde_json::from_str(r#"{"guild_id": "10", "user_id": "2", "channel_id": null}"#).unwrap();
        store.apply(Event::VoiceStateUpdate(Box::new(left)));
        let snapshot = Snapshot::build(&store);
        let channel = snapshot.guilds[0].channels.iter().find(|c| c.id == Id(21)).unwrap();
        assert_eq!(channel.participants.len(), 1);
    }

    #[test]
    fn lists_calls_that_ring_us_or_have_people() {
        let mut store = store();
        let dm = r#"{"id": "50", "type": 1, "recipients": [{"id": "2", "username": "zed", "global_name": "Zed"}]}"#;
        store.apply(Event::ChannelCreate(serde_json::from_str(dm).unwrap()));
        let group = r#"{"id": "51", "type": 3, "recipient_ids": ["2", "3"], "last_message_id": "900"}"#;
        store.apply(Event::ChannelCreate(serde_json::from_str(group).unwrap()));
        store.apply(Event::CallCreate(serde_json::from_str(r#"{"channel_id": "50", "ringing": ["1"]}"#).unwrap()));
        let joined = r#"{"user_id": "2", "channel_id": "51", "session_id": "x"}"#;
        store.apply(Event::VoiceStateUpdate(Box::new(serde_json::from_str(joined).unwrap())));

        let snapshot = Snapshot::build(&store);
        let calls: Vec<(&str, bool, usize)> =
            snapshot.calls.iter().map(|c| (c.name.as_str(), c.ringing_me, c.participants.len())).collect();
        assert_eq!(calls, [("Zed", true, 0), ("Zed, 3", false, 1)]);
        let channels: Vec<&str> = snapshot.private_channels.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(channels, ["Zed, 3", "Zed"]);

        // Joining the DM call leaves the group call.
        let moved = r#"{"user_id": "2", "channel_id": "50", "session_id": "x"}"#;
        store.apply(Event::VoiceStateUpdate(Box::new(serde_json::from_str(moved).unwrap())));
        store.apply(Event::CallUpdate(serde_json::from_str(r#"{"channel_id": "50", "ringing": []}"#).unwrap()));
        let snapshot = Snapshot::build(&store);
        assert_eq!(snapshot.calls.len(), 1);
        assert_eq!(snapshot.calls[0].participants[0].name, "Zed");
        assert!(!snapshot.calls[0].ringing_me);
    }

    #[test]
    fn lists_friends_in_voice_with_our_nickname() {
        let mut store = store();
        let added = r#"{"id": "2", "type": 1, "nickname": "Bro"}"#;
        store.apply(Event::RelationshipAdd(serde_json::from_str(added).unwrap()));
        let pending = r#"{"id": "3", "type": 3}"#;
        store.apply(Event::RelationshipAdd(serde_json::from_str(pending).unwrap()));

        let snapshot = Snapshot::build(&store);
        let friends: Vec<(&str, &str)> =
            snapshot.friends_in_voice.iter().map(|f| (f.name.as_str(), f.channel_name.as_str())).collect();
        assert_eq!(friends, [("Bro", "in-b")]);
        assert!(participant(&snapshot, Id(21), Id(2)).is_friend);

        store.apply(Event::RelationshipRemove(serde_json::from_str(r#"{"id": "2", "type": 1}"#).unwrap()));
        assert!(Snapshot::build(&store).friends_in_voice.is_empty());
    }

    fn participant(snapshot: &Snapshot, channel: Id, user: Id) -> Participant {
        let channel = snapshot.guilds[0].channels.iter().find(|c| c.id == channel).unwrap();
        channel.participants.iter().find(|p| p.user_id == user).unwrap().clone()
    }

    fn overwrite(id: Id, kind: u8, allow: u64, deny: u64) -> bardo_gateway::Overwrite {
        bardo_gateway::Overwrite { id, kind, allow, deny }
    }

    #[test]
    fn moderation_requires_the_matching_permission() {
        let mut store = store();
        let snapshot = Snapshot::build(&store);
        assert_eq!(participant(&snapshot, Id(21), Id(2)).moderation, Moderation::default());

        let guild = store.guilds.get_mut(&GUILD).unwrap();
        guild.roles.get_mut(&Id(30)).unwrap().permissions = MUTE_MEMBERS;
        let snapshot = Snapshot::build(&store);
        let moderation = participant(&snapshot, Id(21), Id(2)).moderation;
        assert_eq!(moderation, Moderation { mute: true, deafen: false, move_: false });
    }

    #[test]
    fn moderation_follows_channel_overwrites() {
        let mut store = store();
        let guild = store.guilds.get_mut(&GUILD).unwrap();
        guild.roles.get_mut(&Id(30)).unwrap().permissions = DEAFEN_MEMBERS | MOVE_MEMBERS;
        // Our role may not move members in channel 21, and nobody may deafen there.
        guild.channels.get_mut(&Id(21)).unwrap().permission_overwrites = vec![
            overwrite(GUILD, overwrite_kind::ROLE, 0, DEAFEN_MEMBERS),
            overwrite(Id(30), overwrite_kind::ROLE, 0, MOVE_MEMBERS),
        ];
        let state = serde_json::from_str(r#"{"guild_id": "10", "user_id": "2", "channel_id": "22"}"#).unwrap();
        store.apply(Event::VoiceStateUpdate(Box::new(state)));
        let state = serde_json::from_str(r#"{"guild_id": "10", "user_id": "3", "channel_id": "21"}"#).unwrap();
        store.apply(Event::VoiceStateUpdate(Box::new(state)));

        let snapshot = Snapshot::build(&store);
        assert_eq!(participant(&snapshot, Id(21), Id(3)).moderation, Moderation::default());
        assert_eq!(
            participant(&snapshot, Id(22), Id(2)).moderation,
            Moderation { mute: false, deafen: true, move_: true }
        );
    }

    #[test]
    fn member_overwrites_can_grant_moderation() {
        let mut store = store();
        let guild = store.guilds.get_mut(&GUILD).unwrap();
        guild.channels.get_mut(&Id(21)).unwrap().permission_overwrites =
            vec![overwrite(ME, overwrite_kind::MEMBER, MUTE_MEMBERS | MOVE_MEMBERS, 0)];
        let snapshot = Snapshot::build(&store);
        let moderation = participant(&snapshot, Id(21), Id(2)).moderation;
        assert_eq!(moderation, Moderation { mute: true, deafen: false, move_: true });
        assert!(moderation.any());
    }

    #[test]
    fn administrators_may_moderate_anyone() {
        let mut store = store();
        let guild = store.guilds.get_mut(&GUILD).unwrap();
        guild.roles.get_mut(&Id(30)).unwrap().permissions = permissions::ADMINISTRATOR;
        guild.channels.get_mut(&Id(21)).unwrap().permission_overwrites =
            vec![overwrite(Id(30), overwrite_kind::ROLE, 0, MUTE_MEMBERS | DEAFEN_MEMBERS | MOVE_MEMBERS)];
        let snapshot = Snapshot::build(&store);
        let moderation = participant(&snapshot, Id(21), Id(2)).moderation;
        assert_eq!(moderation, Moderation { mute: true, deafen: true, move_: true });
    }

    #[test]
    fn flags_our_own_participant_and_channels_we_cannot_join() {
        let mut store = store();
        let state = serde_json::from_str(r#"{"guild_id": "10", "user_id": "1", "channel_id": "21"}"#).unwrap();
        store.apply(Event::VoiceStateUpdate(Box::new(state)));
        let guild = store.guilds.get_mut(&GUILD).unwrap();
        guild.channels.get_mut(&Id(22)).unwrap().permission_overwrites =
            vec![overwrite(GUILD, overwrite_kind::ROLE, 0, CONNECT)];

        let snapshot = Snapshot::build(&store);
        assert!(participant(&snapshot, Id(21), ME).is_me);
        assert!(!participant(&snapshot, Id(21), Id(2)).is_me);
        let can_connect: Vec<(Id, bool)> =
            snapshot.guilds[0].channels.iter().map(|c| (c.id, c.can_connect)).collect();
        assert_eq!(can_connect, [(Id(22), false), (Id(26), true), (Id(21), true), (Id(24), true)]);
    }

    #[test]
    fn owner_and_administrator_see_everything() {
        let mut store = store();
        let guild = store.guilds.get_mut(&GUILD).unwrap();
        let hidden = guild.channels[&Id(23)].clone();
        assert_eq!(permissions::channel_permissions(guild, ME, &hidden) & VIEW_CHANNEL, 0);

        guild.owner_id = ME;
        assert_eq!(permissions::channel_permissions(guild, ME, &hidden), permissions::ALL);

        guild.owner_id = Id(99);
        guild.roles.get_mut(&Id(30)).unwrap().permissions = permissions::ADMINISTRATOR;
        assert_eq!(permissions::channel_permissions(guild, ME, &hidden), permissions::ALL);
    }
}
