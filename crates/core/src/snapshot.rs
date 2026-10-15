//! UI-agnostic view of the state, rebuilt whenever the store changes.

use bardo_gateway::{Channel, Id, channel_kind};

use crate::{
    permissions::{self, VIEW_CHANNEL},
    store::{Guild, Store},
};

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub guilds: Vec<GuildView>,
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
    pub participants: Vec<Participant>,
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
}

impl Snapshot {
    pub fn build(store: &Store) -> Self {
        let Some(me) = &store.me else { return Self::default() };
        let guilds = store
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
        Self { guilds }
    }
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
                })
                .collect();
            participants.sort_by_cached_key(|p| p.name.to_lowercase());

            ChannelView {
                id: channel.id,
                name: channel.name.clone().unwrap_or_default(),
                user_limit: channel.user_limit,
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
    use bardo_gateway::{Event, Ready, ReadySupplemental};

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
        assert_eq!(store().unresolved_voice_users(), vec![(GUILD, vec![Id(3)])]);
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
