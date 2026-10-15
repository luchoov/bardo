//! In-memory cache of the gateway state bardo cares about.

use std::collections::HashMap;

use bardo_gateway::{
    Channel, Event, GatewayGuild, Id, Member, Ready, ReadySupplemental, Relationship, Role, User, VoiceState,
    relationship_kind,
};

#[derive(Default)]
pub struct Store {
    pub me: Option<User>,
    pub users: HashMap<Id, User>,
    pub guilds: HashMap<Id, Guild>,
    /// Guild ids in the order the gateway sent them.
    pub guild_order: Vec<Id>,
    pub private_channels: HashMap<Id, Channel>,
    /// Our friends, with the nickname we gave them.
    pub friends: HashMap<Id, Option<String>>,
}

pub struct Guild {
    pub id: Id,
    pub name: String,
    pub icon: Option<String>,
    pub owner_id: Id,
    pub available: bool,
    pub channels: HashMap<Id, Channel>,
    pub roles: HashMap<Id, Role>,
    pub members: HashMap<Id, Member>,
    /// Keyed by user id.
    pub voice_states: HashMap<Id, VoiceState>,
}

impl Guild {
    fn unavailable(id: Id) -> Self {
        Self {
            id,
            name: String::new(),
            icon: None,
            owner_id: Id::default(),
            available: false,
            channels: HashMap::new(),
            roles: HashMap::new(),
            members: HashMap::new(),
            voice_states: HashMap::new(),
        }
    }
}

impl Store {
    /// Applies an event. Returns whether anything visible may have changed.
    pub fn apply(&mut self, event: Event) -> bool {
        match event {
            Event::Ready(ready) => self.ready(*ready),
            Event::ReadySupplemental(supplemental) => self.ready_supplemental(*supplemental),
            Event::GuildCreate(guild) => self.insert_guild(*guild),
            Event::GuildUpdate(properties) => {
                let Some(guild) = self.guilds.get_mut(&properties.id) else { return false };
                guild.name = properties.name;
                guild.icon = properties.icon;
                guild.owner_id = properties.owner_id;
            }
            Event::GuildDelete(deleted) => {
                if deleted.unavailable {
                    if let Some(guild) = self.guilds.get_mut(&deleted.id) {
                        guild.available = false;
                    }
                } else {
                    self.guilds.remove(&deleted.id);
                    self.guild_order.retain(|&id| id != deleted.id);
                }
            }
            Event::ChannelCreate(channel) | Event::ChannelUpdate(channel) => self.upsert_channel(channel),
            Event::ChannelDelete(channel) => match channel.guild_id {
                Some(guild_id) => {
                    if let Some(guild) = self.guilds.get_mut(&guild_id) {
                        guild.channels.remove(&channel.id);
                    }
                }
                None => {
                    self.private_channels.remove(&channel.id);
                }
            },
            Event::GuildRoleCreate(update) | Event::GuildRoleUpdate(update) => {
                let Some(guild) = self.guilds.get_mut(&update.guild_id) else { return false };
                guild.roles.insert(update.role.id, update.role);
            }
            Event::GuildRoleDelete(deleted) => {
                let Some(guild) = self.guilds.get_mut(&deleted.guild_id) else { return false };
                guild.roles.remove(&deleted.role_id);
            }
            Event::GuildMemberUpdate(update) => self.upsert_member(update.guild_id, update.member),
            Event::GuildMembersChunk(chunk) => {
                for member in chunk.members {
                    self.upsert_member(chunk.guild_id, member);
                }
            }
            Event::PassiveUpdate(update) => {
                for member in update.updated_members {
                    self.upsert_member(update.guild_id, member);
                }
                for state in update.updated_voice_states {
                    self.upsert_voice_state(update.guild_id, state);
                }
                if let Some(guild) = self.guilds.get_mut(&update.guild_id) {
                    for user_id in update.removed_voice_states {
                        guild.voice_states.remove(&user_id);
                    }
                }
            }
            Event::VoiceStateUpdate(state) => match state.guild_id {
                Some(guild_id) => self.upsert_voice_state(guild_id, *state),
                // DM and group calls are not tracked yet.
                None => return false,
            },
            Event::RelationshipAdd(relationship) => self.upsert_relationship(relationship),
            Event::RelationshipRemove(relationship) => {
                self.friends.remove(&relationship.id);
            }
            Event::Resumed
            | Event::AuthTokenUpdate(_)
            | Event::VoiceServerUpdate(_)
            | Event::Other(_) => return false,
        }
        true
    }

    /// Display name of a user in a guild: nickname, then global name, then username.
    pub fn display_name(&self, guild: &Guild, user_id: Id) -> Option<String> {
        let member = guild.members.get(&user_id);
        if let Some(nick) = member.and_then(|m| m.nick.clone()) {
            return Some(nick);
        }
        member
            .and_then(|m| m.user.as_ref())
            .or_else(|| self.users.get(&user_id))
            .map(|user| user.display_name().to_owned())
    }

    /// Users in voice channels whose name we cannot resolve, grouped by guild, for the guilds
    /// that `wanted` accepts.
    pub fn unresolved_voice_users(&self, wanted: impl Fn(Id) -> bool) -> Vec<(Id, Vec<Id>)> {
        self.guilds
            .values()
            .filter(|guild| wanted(guild.id))
            .filter_map(|guild| {
                let missing: Vec<Id> = guild
                    .voice_states
                    .keys()
                    .copied()
                    .filter(|user_id| self.display_name(guild, *user_id).is_none())
                    .collect();
                (!missing.is_empty()).then_some((guild.id, missing))
            })
            .collect()
    }

    fn ready(&mut self, ready: Ready) {
        *self = Store::default();
        self.users = ready.users.into_iter().map(|u| (u.id, u)).collect();
        self.users.insert(ready.user.id, ready.user.clone());
        self.me = Some(ready.user);
        self.private_channels = ready.private_channels.into_iter().map(|c| (c.id, c)).collect();
        for relationship in ready.relationships {
            self.upsert_relationship(relationship);
        }

        let mut merged_members = ready.merged_members.into_iter();
        for guild in ready.guilds {
            let guild_id = guild.id;
            self.insert_guild(guild);
            for member in merged_members.next().unwrap_or_default() {
                self.upsert_member(guild_id, member);
            }
        }
    }

    fn ready_supplemental(&mut self, supplemental: ReadySupplemental) {
        let mut merged_members = supplemental.merged_members.into_iter();
        for guild in supplemental.guilds {
            for member in merged_members.next().unwrap_or_default() {
                self.upsert_member(guild.id, member);
            }
            for state in guild.voice_states {
                self.upsert_voice_state(guild.id, state);
            }
        }
    }

    fn insert_guild(&mut self, mut gateway_guild: GatewayGuild) {
        let id = gateway_guild.id;
        if !self.guild_order.contains(&id) {
            self.guild_order.push(id);
        }

        let Some(properties) = gateway_guild.take_properties().filter(|_| !gateway_guild.unavailable) else {
            self.guilds.entry(id).or_insert_with(|| Guild::unavailable(id)).available = false;
            return;
        };

        let guild = Guild {
            id,
            name: properties.name,
            icon: properties.icon,
            owner_id: properties.owner_id,
            available: true,
            channels: gateway_guild.channels.into_iter().map(|c| (c.id, c)).collect(),
            roles: gateway_guild.roles.into_iter().map(|r| (r.id, r)).collect(),
            members: HashMap::new(),
            voice_states: HashMap::new(),
        };
        self.guilds.insert(id, guild);

        for member in gateway_guild.members {
            self.upsert_member(id, member);
        }
        for state in gateway_guild.voice_states {
            self.upsert_voice_state(id, state);
        }
    }

    fn upsert_relationship(&mut self, mut relationship: Relationship) {
        if let Some(user) = relationship.user.take() {
            self.users.insert(user.id, user);
        }
        if relationship.kind == relationship_kind::FRIEND {
            self.friends.insert(relationship.id, relationship.nickname);
        } else {
            // A friend became blocked, say.
            self.friends.remove(&relationship.id);
        }
    }

    /// How we call a friend: our nickname for them, then their global name, then their username.
    pub fn friend_name(&self, user_id: Id) -> Option<String> {
        let nickname = self.friends.get(&user_id)?;
        nickname.clone().or_else(|| self.users.get(&user_id).map(|user| user.display_name().to_owned()))
    }

    fn upsert_channel(&mut self, channel: Channel) {
        match channel.guild_id {
            Some(guild_id) => {
                if let Some(guild) = self.guilds.get_mut(&guild_id) {
                    guild.channels.insert(channel.id, channel);
                }
            }
            None => {
                self.private_channels.insert(channel.id, channel);
            }
        }
    }

    fn upsert_member(&mut self, guild_id: Id, mut member: Member) {
        let Some(user_id) = member.id() else { return };
        if let Some(user) = member.user.take() {
            self.users.insert(user.id, user);
        }
        member.user_id = Some(user_id);
        if let Some(guild) = self.guilds.get_mut(&guild_id) {
            guild.members.insert(user_id, member);
        }
    }

    fn upsert_voice_state(&mut self, guild_id: Id, mut state: VoiceState) {
        if let Some(member) = state.member.take() {
            self.upsert_member(guild_id, member);
        }
        let Some(guild) = self.guilds.get_mut(&guild_id) else { return };
        if state.channel_id.is_some() {
            guild.voice_states.insert(state.user_id, state);
        } else {
            guild.voice_states.remove(&state.user_id);
        }
    }
}
