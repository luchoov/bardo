//! Which sounds other people cause in our channel or call, from two consecutive snapshots. Our
//! own comings and goings are decided by [`VoiceSession`](crate::voice::VoiceSession), which knows
//! why they happened.

use std::collections::HashSet;

use bardo_audio::Sound;
use bardo_gateway::Id;

use crate::snapshot::{Participant, Snapshot, VoiceStatus};

/// The sounds for what changed between `prev` and `next` in the channel or call we are connected
/// to. Nothing plays until we have been connected there for one snapshot, so the people already
/// in it when we arrive make no noise.
pub fn between(prev: &Snapshot, next: &Snapshot) -> Vec<Sound> {
    let (Some(before), Some(after)) = (Room::of(prev), Room::of(next)) else {
        return Vec::new();
    };
    if before.channel_id != after.channel_id {
        return Vec::new();
    }
    let others = |room: &Room| -> HashSet<Id> { room.others.iter().copied().collect() };
    let (was, is) = (others(&before), others(&after));
    let joined = is.difference(&was).next().is_some();
    let left = was.difference(&is).next().is_some();

    let mut sounds = Vec::new();
    if joined {
        sounds.push(Sound::UserJoin);
    }
    if after.private && is.is_empty() && (!was.is_empty() || before.ringing && !after.ringing) {
        // The last one left, or everyone we rang declined (or Discord gave up ringing them).
        sounds.push(Sound::CallEnded);
    } else if left {
        sounds.push(Sound::UserLeave);
    }
    sounds
}

/// The DM or group call whose others are being rung while we wait alone in it.
pub fn ringback(snapshot: &Snapshot) -> Option<Id> {
    let room = Room::of(snapshot)?;
    (room.private && room.ringing && room.others.is_empty()).then_some(room.channel_id)
}

/// The channel or call we are connected to.
struct Room {
    channel_id: Id,
    private: bool,
    /// Everyone in it but us.
    others: Vec<Id>,
    /// Someone besides us is being rung.
    ringing: bool,
}

impl Room {
    fn of(snapshot: &Snapshot) -> Option<Self> {
        let voice = snapshot.voice.as_ref().filter(|voice| voice.status == VoiceStatus::Connected)?;
        let others =
            |participants: &[Participant]| participants.iter().filter(|p| !p.is_me).map(|p| p.user_id).collect();
        match voice.guild_id {
            Some(guild_id) => {
                let channel = snapshot
                    .guilds
                    .iter()
                    .find(|guild| guild.id == guild_id)?
                    .channels
                    .iter()
                    .find(|channel| channel.id == voice.channel_id)?;
                Some(Self {
                    channel_id: channel.id,
                    private: false,
                    others: others(&channel.participants),
                    ringing: false,
                })
            }
            None => {
                // A call with nobody in it and nobody rung has no view at all.
                let call = snapshot.calls.iter().find(|call| call.channel_id == voice.channel_id);
                Some(Self {
                    channel_id: voice.channel_id,
                    private: true,
                    others: call.map(|call| others(&call.participants)).unwrap_or_default(),
                    ringing: call.is_some_and(|call| call.ringing_others),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{CallView, ChannelView, GuildView, Moderation, VoiceView};

    const ME: Id = Id(1);
    const GUILD: Id = Id(10);
    const CHANNEL: Id = Id(20);
    const OTHER_CHANNEL: Id = Id(21);
    const DM: Id = Id(50);

    fn participant(user_id: Id) -> Participant {
        Participant {
            user_id,
            name: user_id.to_string(),
            self_mute: false,
            self_deaf: false,
            server_mute: false,
            server_deaf: false,
            streaming: false,
            video: false,
            is_me: user_id == ME,
            is_friend: false,
            moderation: Moderation::default(),
        }
    }

    fn voice(guild_id: Option<Id>, channel_id: Id, status: VoiceStatus) -> Option<VoiceView> {
        Some(VoiceView {
            guild_id,
            channel_id,
            guild_name: String::new(),
            channel_name: String::new(),
            status,
            self_mute: false,
            self_deaf: false,
        })
    }

    /// A guild with our channel and another one, with `users` in ours and `elsewhere` in the other.
    fn guild(status: Option<VoiceStatus>, users: &[u64], elsewhere: &[u64]) -> Snapshot {
        let channel = |id, users: &[u64]| ChannelView {
            id,
            name: String::new(),
            user_limit: 0,
            can_connect: true,
            participants: users.iter().map(|&id| participant(Id(id))).collect(),
        };
        Snapshot {
            guilds: vec![GuildView {
                id: GUILD,
                name: String::new(),
                channels: vec![channel(CHANNEL, users), channel(OTHER_CHANNEL, elsewhere)],
            }],
            voice: status.and_then(|status| voice(Some(GUILD), CHANNEL, status)),
            ..Snapshot::default()
        }
    }

    fn call(users: &[u64], ringing_others: bool) -> Snapshot {
        Snapshot {
            calls: vec![CallView {
                channel_id: DM,
                name: String::new(),
                group: false,
                participants: users.iter().map(|&id| participant(Id(id))).collect(),
                ringing_me: false,
                ringing_others,
            }],
            voice: voice(None, DM, VoiceStatus::Connected),
            ..Snapshot::default()
        }
    }

    const CONNECTED: Option<VoiceStatus> = Some(VoiceStatus::Connected);

    #[test]
    fn people_already_there_when_we_connect_are_silent() {
        let connecting = guild(Some(VoiceStatus::Connecting), &[2, 3], &[]);
        assert!(between(&connecting, &guild(CONNECTED, &[1, 2, 3], &[])).is_empty());
        let outside = guild(None, &[2, 3], &[]);
        assert!(between(&outside, &guild(CONNECTED, &[1, 2, 3], &[])).is_empty());
    }

    #[test]
    fn people_joining_and_leaving_our_channel() {
        let before = guild(CONNECTED, &[1, 2], &[]);
        assert_eq!(between(&before, &guild(CONNECTED, &[1, 2, 3, 4], &[])), [Sound::UserJoin]);
        assert_eq!(between(&before, &guild(CONNECTED, &[1], &[])), [Sound::UserLeave]);
        // Someone moved out while someone else came in.
        assert_eq!(between(&before, &guild(CONNECTED, &[1, 3], &[2])), [Sound::UserJoin, Sound::UserLeave]);
        // Muting, deafening or anything else about the same people makes no sound.
        let mut muted = before.clone();
        muted.guilds[0].channels[0].participants[1].self_mute = true;
        assert!(between(&before, &muted).is_empty());
    }

    #[test]
    fn other_channels_are_none_of_our_business() {
        let before = guild(CONNECTED, &[1], &[2]);
        assert!(between(&before, &guild(CONNECTED, &[1], &[2, 3])).is_empty());
        assert!(between(&before, &guild(CONNECTED, &[1], &[])).is_empty());
        assert!(between(&guild(None, &[], &[2]), &guild(None, &[3], &[])).is_empty());
    }

    #[test]
    fn our_own_moves_and_departures_are_not_other_people() {
        let before = guild(CONNECTED, &[1, 2], &[3]);
        // We were moved: we are reconnecting in the other channel.
        let mut moved = guild(Some(VoiceStatus::Connecting), &[2], &[1, 3]);
        moved.voice.as_mut().unwrap().channel_id = OTHER_CHANNEL;
        assert!(between(&before, &moved).is_empty());
        let mut arrived = moved.clone();
        arrived.voice.as_mut().unwrap().status = VoiceStatus::Connected;
        assert!(between(&moved, &arrived).is_empty());
        // We left.
        assert!(between(&before, &guild(None, &[2], &[3])).is_empty());
    }

    #[test]
    fn being_left_alone_in_a_call_ends_it() {
        let group = call(&[1, 2, 3], false);
        assert_eq!(between(&group, &call(&[1, 2], false)), [Sound::UserLeave]);
        assert_eq!(between(&call(&[1, 2], false), &call(&[1], false)), [Sound::CallEnded]);
        // A call nobody else is in may lose its view altogether.
        let mut gone = call(&[], false);
        gone.calls.clear();
        assert_eq!(between(&call(&[1, 2], false), &gone), [Sound::CallEnded]);
    }

    #[test]
    fn ringback_lasts_until_someone_answers_or_declines() {
        let ringing = call(&[1], true);
        assert_eq!(ringback(&ringing), Some(DM));
        assert_eq!(ringback(&call(&[1, 2], false)), None);

        // Answered.
        assert_eq!(between(&ringing, &call(&[1, 2], false)), [Sound::UserJoin]);
        // Declined, or nobody answered.
        assert_eq!(between(&ringing, &call(&[1], false)), [Sound::CallEnded]);
        // Still ringing.
        assert!(between(&ringing, &ringing).is_empty());

        // Not before we are connected, and never in a guild channel.
        let mut connecting = ringing.clone();
        connecting.voice.as_mut().unwrap().status = VoiceStatus::Connecting;
        assert_eq!(ringback(&connecting), None);
        assert_eq!(ringback(&guild(CONNECTED, &[1], &[])), None);
    }
}
