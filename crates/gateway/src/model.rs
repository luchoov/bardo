//! Gateway payload types. Only the fields bardo needs are modeled; everything else is ignored.

use serde::{Deserialize, Serialize};

use crate::id::{Id, u64_string};

pub mod channel_kind {
    pub const GUILD_TEXT: u8 = 0;
    pub const DM: u8 = 1;
    pub const GUILD_VOICE: u8 = 2;
    pub const GROUP_DM: u8 = 3;
    pub const GUILD_CATEGORY: u8 = 4;
    pub const GUILD_STAGE_VOICE: u8 = 13;
}

pub mod overwrite_kind {
    pub const ROLE: u8 = 0;
    pub const MEMBER: u8 = 1;
}

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: Id,
    pub username: String,
    #[serde(default)]
    pub global_name: Option<String>,
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub bot: bool,
}

impl User {
    pub fn display_name(&self) -> &str {
        self.global_name.as_deref().unwrap_or(&self.username)
    }
}

/// A guild member. Depending on the payload, the user is either embedded or referenced by `user_id`.
#[derive(Debug, Clone, Deserialize)]
pub struct Member {
    #[serde(default)]
    pub user: Option<User>,
    #[serde(default)]
    pub user_id: Option<Id>,
    #[serde(default)]
    pub nick: Option<String>,
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub roles: Vec<Id>,
    #[serde(default)]
    pub mute: bool,
    #[serde(default)]
    pub deaf: bool,
}

impl Member {
    pub fn id(&self) -> Option<Id> {
        self.user.as_ref().map(|u| u.id).or(self.user_id)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Role {
    pub id: Id,
    pub name: String,
    #[serde(deserialize_with = "u64_string")]
    pub permissions: u64,
    #[serde(default)]
    pub position: i32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Overwrite {
    pub id: Id,
    #[serde(rename = "type")]
    pub kind: u8,
    #[serde(deserialize_with = "u64_string")]
    pub allow: u64,
    #[serde(deserialize_with = "u64_string")]
    pub deny: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Channel {
    pub id: Id,
    #[serde(rename = "type")]
    pub kind: u8,
    #[serde(default)]
    pub guild_id: Option<Id>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub position: i32,
    #[serde(default)]
    pub parent_id: Option<Id>,
    #[serde(default)]
    pub permission_overwrites: Vec<Overwrite>,
    #[serde(default)]
    pub user_limit: u32,
    #[serde(default)]
    pub recipient_ids: Vec<Id>,
    #[serde(default)]
    pub recipients: Vec<User>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VoiceState {
    #[serde(default)]
    pub guild_id: Option<Id>,
    #[serde(default)]
    pub channel_id: Option<Id>,
    pub user_id: Id,
    #[serde(default)]
    pub member: Option<Member>,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub mute: bool,
    #[serde(default)]
    pub deaf: bool,
    #[serde(default)]
    pub self_mute: bool,
    #[serde(default)]
    pub self_deaf: bool,
    #[serde(default)]
    pub self_video: bool,
    #[serde(default)]
    pub self_stream: bool,
    #[serde(default)]
    pub suppress: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuildProperties {
    pub id: Id,
    pub name: String,
    #[serde(default)]
    pub icon: Option<String>,
    pub owner_id: Id,
}

/// A guild as sent in READY and GUILD_CREATE.
///
/// With the `CLIENT_STATE_V2` capability the guild's own fields live under `properties`;
/// without it they are flattened into the object. Both shapes are accepted.
#[derive(Debug, Clone, Deserialize)]
pub struct GatewayGuild {
    pub id: Id,
    #[serde(default)]
    pub properties: Option<GuildProperties>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    owner_id: Option<Id>,
    #[serde(default)]
    pub unavailable: bool,
    #[serde(default)]
    pub channels: Vec<Channel>,
    #[serde(default)]
    pub roles: Vec<Role>,
    #[serde(default)]
    pub voice_states: Vec<VoiceState>,
    #[serde(default)]
    pub members: Vec<Member>,
}

impl GatewayGuild {
    /// Returns the guild's own fields regardless of payload shape, or `None` if the guild is unavailable.
    pub fn take_properties(&mut self) -> Option<GuildProperties> {
        if let Some(properties) = self.properties.take() {
            return Some(properties);
        }
        Some(GuildProperties {
            id: self.id,
            name: self.name.take()?,
            icon: self.icon.take(),
            owner_id: self.owner_id?,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Ready {
    pub user: User,
    #[serde(default)]
    pub guilds: Vec<GatewayGuild>,
    /// Users referenced elsewhere in the payload (`DEDUPE_USER_OBJECTS`).
    #[serde(default)]
    pub users: Vec<User>,
    /// Per guild, in the same order as `guilds`. With `PRIORITIZED_READY_PAYLOAD` only our own member.
    #[serde(default)]
    pub merged_members: Vec<Vec<Member>>,
    #[serde(default)]
    pub private_channels: Vec<Channel>,
    pub session_id: String,
    pub resume_gateway_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReadySupplemental {
    #[serde(default)]
    pub guilds: Vec<SupplementalGuild>,
    /// Per guild, in the same order as `guilds`: members referenced by voice states and presences.
    #[serde(default)]
    pub merged_members: Vec<Vec<Member>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SupplementalGuild {
    pub id: Id,
    #[serde(default)]
    pub voice_states: Vec<VoiceState>,
}

/// Sent for large guilds we are not subscribed to, instead of individual events.
#[derive(Debug, Clone, Deserialize)]
pub struct PassiveUpdate {
    pub guild_id: Id,
    #[serde(default)]
    pub updated_voice_states: Vec<VoiceState>,
    #[serde(default)]
    pub removed_voice_states: Vec<Id>,
    #[serde(default)]
    pub updated_members: Vec<Member>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VoiceServerUpdate {
    pub token: String,
    #[serde(default)]
    pub guild_id: Option<Id>,
    #[serde(default)]
    pub channel_id: Option<Id>,
    #[serde(default)]
    pub endpoint: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuildDelete {
    pub id: Id,
    #[serde(default)]
    pub unavailable: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuildRoleUpdate {
    pub guild_id: Id,
    pub role: Role,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuildRoleDelete {
    pub guild_id: Id,
    pub role_id: Id,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuildMemberUpdate {
    pub guild_id: Id,
    #[serde(flatten)]
    pub member: Member,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuildMembersChunk {
    pub guild_id: Id,
    #[serde(default)]
    pub members: Vec<Member>,
    #[serde(default)]
    pub not_found: Vec<Id>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthTokenUpdate {
    pub token: String,
}

/// Op 4: join, move between or leave voice channels (`channel_id: None` leaves).
#[derive(Debug, Clone, Serialize)]
pub struct UpdateVoiceState {
    pub guild_id: Option<Id>,
    pub channel_id: Option<Id>,
    pub self_mute: bool,
    pub self_deaf: bool,
    pub self_video: bool,
}

/// Op 8: fetch specific members of a guild.
#[derive(Debug, Clone, Serialize)]
pub struct RequestGuildMembers {
    pub guild_id: Id,
    pub user_ids: Vec<Id>,
    pub presences: bool,
}
