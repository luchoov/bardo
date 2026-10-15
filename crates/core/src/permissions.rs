//! Effective permission computation, following Discord's documented algorithm.

use bardo_gateway::{Channel, Id, overwrite_kind};

use crate::store::Guild;

pub const ADMINISTRATOR: u64 = 1 << 3;
pub const VIEW_CHANNEL: u64 = 1 << 10;
pub const CONNECT: u64 = 1 << 20;
pub const SPEAK: u64 = 1 << 21;
pub const MUTE_MEMBERS: u64 = 1 << 22;
pub const DEAFEN_MEMBERS: u64 = 1 << 23;
pub const MOVE_MEMBERS: u64 = 1 << 24;

pub const ALL: u64 = u64::MAX;

/// Guild-level permissions of a user: @everyone plus all of their roles.
pub fn guild_permissions(guild: &Guild, user_id: Id) -> u64 {
    if guild.owner_id == user_id {
        return ALL;
    }
    // The @everyone role shares the guild's id.
    let mut permissions = guild.roles.get(&guild.id).map_or(0, |role| role.permissions);
    if let Some(member) = guild.members.get(&user_id) {
        for role_id in &member.roles {
            if let Some(role) = guild.roles.get(role_id) {
                permissions |= role.permissions;
            }
        }
    }
    if permissions & ADMINISTRATOR != 0 { ALL } else { permissions }
}

/// Permissions of a user in a channel, applying @everyone, role and member overwrites in order.
pub fn channel_permissions(guild: &Guild, user_id: Id, channel: &Channel) -> u64 {
    let mut permissions = guild_permissions(guild, user_id);
    if permissions == ALL {
        return ALL;
    }

    let overwrites = &channel.permission_overwrites;
    if let Some(everyone) = overwrites.iter().find(|o| o.id == guild.id) {
        permissions = (permissions & !everyone.deny) | everyone.allow;
    }

    let roles = guild.members.get(&user_id).map(|m| m.roles.as_slice()).unwrap_or_default();
    let (mut allow, mut deny) = (0, 0);
    for overwrite in overwrites
        .iter()
        .filter(|o| o.kind == overwrite_kind::ROLE && roles.contains(&o.id))
    {
        allow |= overwrite.allow;
        deny |= overwrite.deny;
    }
    permissions = (permissions & !deny) | allow;

    if let Some(member) = overwrites.iter().find(|o| o.kind == overwrite_kind::MEMBER && o.id == user_id) {
        permissions = (permissions & !member.deny) | member.allow;
    }
    permissions
}
