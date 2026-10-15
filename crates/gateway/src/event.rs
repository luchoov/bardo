use serde::de::DeserializeOwned;
use serde_json::value::RawValue;

use crate::model::*;

/// A dispatch (op 0) the gateway sent us. Unhandled event names are reported as `Other`.
#[derive(Debug)]
pub enum Event {
    Ready(Box<Ready>),
    ReadySupplemental(Box<ReadySupplemental>),
    Resumed,
    AuthTokenUpdate(AuthTokenUpdate),
    GuildCreate(Box<GatewayGuild>),
    GuildUpdate(GuildProperties),
    GuildDelete(GuildDelete),
    ChannelCreate(Channel),
    ChannelUpdate(Channel),
    ChannelDelete(Channel),
    GuildRoleCreate(GuildRoleUpdate),
    GuildRoleUpdate(GuildRoleUpdate),
    GuildRoleDelete(GuildRoleDelete),
    GuildMemberUpdate(Box<GuildMemberUpdate>),
    GuildMembersChunk(GuildMembersChunk),
    PassiveUpdate(PassiveUpdate),
    VoiceStateUpdate(Box<VoiceState>),
    VoiceServerUpdate(VoiceServerUpdate),
    /// Added or changed (a nickname, say).
    RelationshipAdd(Relationship),
    RelationshipRemove(Relationship),
    Other(String),
}

impl Event {
    pub(crate) fn parse(name: &str, data: &RawValue) -> serde_json::Result<Self> {
        fn de<T: DeserializeOwned>(data: &RawValue) -> serde_json::Result<T> {
            serde_json::from_str(data.get())
        }

        Ok(match name {
            "READY" => Event::Ready(de(data)?),
            "READY_SUPPLEMENTAL" => Event::ReadySupplemental(de(data)?),
            "RESUMED" => Event::Resumed,
            "AUTH_TOKEN_UPDATE" => Event::AuthTokenUpdate(de(data)?),
            "GUILD_CREATE" => Event::GuildCreate(de(data)?),
            "GUILD_UPDATE" => Event::GuildUpdate(de(data)?),
            "GUILD_DELETE" => Event::GuildDelete(de(data)?),
            "CHANNEL_CREATE" => Event::ChannelCreate(de(data)?),
            "CHANNEL_UPDATE" => Event::ChannelUpdate(de(data)?),
            "CHANNEL_DELETE" => Event::ChannelDelete(de(data)?),
            "GUILD_ROLE_CREATE" => Event::GuildRoleCreate(de(data)?),
            "GUILD_ROLE_UPDATE" => Event::GuildRoleUpdate(de(data)?),
            "GUILD_ROLE_DELETE" => Event::GuildRoleDelete(de(data)?),
            "GUILD_MEMBER_UPDATE" => Event::GuildMemberUpdate(de(data)?),
            "GUILD_MEMBERS_CHUNK" => Event::GuildMembersChunk(de(data)?),
            "PASSIVE_UPDATE_V2" => Event::PassiveUpdate(de(data)?),
            "VOICE_STATE_UPDATE" => Event::VoiceStateUpdate(de(data)?),
            "VOICE_SERVER_UPDATE" => Event::VoiceServerUpdate(de(data)?),
            "RELATIONSHIP_ADD" | "RELATIONSHIP_UPDATE" => Event::RelationshipAdd(de(data)?),
            "RELATIONSHIP_REMOVE" => Event::RelationshipRemove(de(data)?),
            other => Event::Other(other.to_owned()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::Id;

    fn parse(name: &str, json: &str) -> Event {
        let raw: Box<RawValue> = serde_json::from_str(json).unwrap();
        Event::parse(name, &raw).unwrap()
    }

    #[test]
    fn ready_accepts_client_state_v2_guilds() {
        let event = parse(
            "READY",
            r#"{
                "user": {"id": "1", "username": "me"},
                "users": [{"id": "2", "username": "friend", "global_name": "Friend"}],
                "guilds": [{
                    "id": "10",
                    "properties": {"id": "10", "name": "Guild", "owner_id": "1", "icon": null},
                    "channels": [{"id": "11", "type": 2, "name": "Voice", "position": 0,
                                  "permission_overwrites": [{"id": "10", "type": 0, "allow": "0", "deny": "1024"}]}],
                    "roles": [{"id": "10", "name": "@everyone", "permissions": "1024", "position": 0}],
                    "data_mode": "full"
                }],
                "merged_members": [[{"user_id": "1", "roles": [], "nick": null}]],
                "private_channels": [],
                "session_id": "abc",
                "resume_gateway_url": "wss://resume.example"
            }"#,
        );
        let Event::Ready(mut ready) = event else { panic!("expected READY") };
        let guild = &mut ready.guilds[0];
        assert_eq!(guild.take_properties().unwrap().name, "Guild");
        assert_eq!(guild.channels[0].permission_overwrites[0].deny, 1024);
        assert_eq!(ready.merged_members[0][0].id(), Some(Id(1)));
    }

    #[test]
    fn guild_accepts_flattened_properties() {
        let Event::GuildCreate(mut guild) = parse(
            "GUILD_CREATE",
            r#"{"id": "10", "name": "Flat", "owner_id": "1", "channels": [], "roles": []}"#,
        ) else {
            panic!("expected GUILD_CREATE")
        };
        assert_eq!(guild.take_properties().unwrap().name, "Flat");
    }

    #[test]
    fn unavailable_guild_has_no_properties() {
        let Event::GuildCreate(mut guild) =
            parse("GUILD_CREATE", r#"{"id": "10", "unavailable": true}"#)
        else {
            panic!("expected GUILD_CREATE")
        };
        assert!(guild.take_properties().is_none());
    }

    #[test]
    fn relationships_parse_with_or_without_the_user() {
        let Event::RelationshipAdd(added) = parse(
            "RELATIONSHIP_ADD",
            r#"{"id": "2", "type": 1, "nickname": "Bro", "user": {"id": "2", "username": "friend"}}"#,
        ) else {
            panic!("expected RELATIONSHIP_ADD")
        };
        assert_eq!((added.id, added.kind, added.nickname.as_deref()), (Id(2), 1, Some("Bro")));
        let Event::RelationshipRemove(removed) = parse("RELATIONSHIP_REMOVE", r#"{"id": "2", "type": 1}"#) else {
            panic!("expected RELATIONSHIP_REMOVE")
        };
        assert!(removed.user.is_none());
    }

    #[test]
    fn unknown_events_are_reported_by_name() {
        assert!(matches!(parse("TYPING_START", "{}"), Event::Other(name) if name == "TYPING_START"));
    }
}
