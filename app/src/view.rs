//! Turns the snapshot and the list's state into rows.
//!
//! The list shows where people are rather than everything that exists: friends in voice first,
//! then the pinned servers (and the one we are in voice in), then every other server, folded.
//! Empty channels are folded away too.

use std::collections::HashSet;

use bardo_core::{CallView, ChannelView, FriendInVoice, GuildView, Id, Participant, Snapshot};
use slint::{ModelRc, SharedString, VecModel};

use crate::{
    Row, RowKind, Target,
    settings::Settings,
};

pub const SECTION_FRIENDS: &str = "friends";
pub const SECTION_OTHER: &str = "other";
/// Search results are capped; more would not help anyone find a channel.
const MAX_RESULTS: usize = 40;

/// How the list is folded and filtered. Lives for the session.
#[derive(Debug, Default)]
pub struct ListState {
    /// Pinned servers (and the one we are in voice in) are expanded unless listed here.
    pub collapsed: HashSet<Id>,
    /// Other servers are collapsed unless listed here.
    pub expanded: HashSet<Id>,
    /// Whether "Other servers" is open; by default only when nothing is pinned.
    pub other_open: Option<bool>,
    pub friends_collapsed: bool,
    pub query: String,
}

impl ListState {
    pub fn other_open(&self, settings: &Settings) -> bool {
        self.other_open.unwrap_or(settings.servers.pinned.is_empty())
    }
}

pub struct Context<'a> {
    pub snapshot: &'a Snapshot,
    pub settings: &'a Settings,
    pub speaking: &'a HashSet<Id>,
    pub list: &'a ListState,
}

impl Context<'_> {
    fn current_channel(&self) -> Option<Id> {
        self.snapshot.voice.as_ref().map(|voice| voice.channel_id)
    }

    fn current_guild(&self) -> Option<Id> {
        self.snapshot.voice.as_ref().and_then(|voice| voice.guild_id)
    }

    /// Servers listed in full: the pinned ones, then the one we are in voice in.
    fn main_guilds(&self) -> Vec<&GuildView> {
        let guild = |id: &Id| self.snapshot.guilds.iter().find(|guild| guild.id == *id);
        let mut guilds: Vec<&GuildView> = self.settings.servers.pinned.iter().filter_map(guild).collect();
        if let Some(current) = self.current_guild()
            && !self.settings.servers.is_pinned(current)
            && let Some(current) = guild(&current)
        {
            guilds.push(current);
        }
        guilds
    }
}

pub fn rows(cx: &Context) -> Vec<Row> {
    let query = cx.list.query.trim().to_lowercase();
    if query.is_empty() { browse(cx) } else { search(cx, &query) }
}

/// Guilds whose members should be fetched: the ones whose participants are on screen.
pub fn watched_guilds(cx: &Context) -> HashSet<Id> {
    let mut watched: HashSet<Id> = cx.main_guilds().iter().map(|guild| guild.id).collect();
    if cx.list.other_open(cx.settings) {
        watched.extend(cx.list.expanded.iter().copied());
    }
    watched
}

/// The channel the first search result leads to; the guild is empty for DMs and groups.
pub fn first_result(rows: &[Row]) -> Option<(String, String)> {
    rows.iter()
        .find(|row| matches!(row.kind, RowKind::Person | RowKind::Place) && row.can_connect && !row.current)
        .map(|row| (row.guild_id.to_string(), row.channel_id.to_string()))
}

fn browse(cx: &Context) -> Vec<Row> {
    let mut rows = Vec::new();

    if !cx.snapshot.calls.is_empty() {
        rows.push(section("calls", "CALLS", cx.snapshot.calls.len(), None));
        for call in &cx.snapshot.calls {
            call_rows(cx, call, &mut rows);
        }
    }

    let friends: Vec<&FriendInVoice> = cx
        .snapshot
        .friends_in_voice
        .iter()
        .filter(|friend| Some(friend.channel_id) != cx.current_channel())
        .collect();
    if !friends.is_empty() {
        let expanded = !cx.list.friends_collapsed;
        rows.push(section(SECTION_FRIENDS, "FRIENDS IN VOICE", friends.len(), Some(expanded)));
        if expanded {
            rows.extend(friends.into_iter().map(|friend| friend_row(cx, friend)));
        }
    }

    let main = cx.main_guilds();
    rows.push(section("servers", "SERVERS", 0, None));
    if main.is_empty() {
        rows.push(Row {
            kind: RowKind::Hint,
            label: "Pin the servers you use to keep them here: hover one below and click the pin.".into(),
            ..Default::default()
        });
    }
    for guild in &main {
        guild_rows(cx, guild, !cx.list.collapsed.contains(&guild.id), &mut rows);
    }

    let main_ids: HashSet<Id> = main.iter().map(|guild| guild.id).collect();
    let others: Vec<&GuildView> = cx.snapshot.guilds.iter().filter(|guild| !main_ids.contains(&guild.id)).collect();
    if !others.is_empty() {
        let open = cx.list.other_open(cx.settings);
        rows.push(section(SECTION_OTHER, "OTHER SERVERS", others.len(), Some(open)));
        if open {
            for guild in others {
                guild_rows(cx, guild, cx.list.expanded.contains(&guild.id), &mut rows);
            }
        }
    }
    rows
}

fn search(cx: &Context, query: &str) -> Vec<Row> {
    let current = cx.current_channel();
    let mut places = Vec::new();
    let mut people = Vec::new();
    for guild in &cx.snapshot.guilds {
        let guild_matches = guild.name.to_lowercase().contains(query);
        for channel in &guild.channels {
            if guild_matches || channel.name.to_lowercase().contains(query) {
                places.push(Row {
                    kind: RowKind::Place,
                    id: id(channel.id),
                    label: channel.name.as_str().into(),
                    sublabel: guild.name.as_str().into(),
                    guild_id: id(guild.id),
                    channel_id: id(channel.id),
                    count: channel.participants.len() as i32,
                    can_connect: channel.can_connect,
                    current: current == Some(channel.id),
                    ..Default::default()
                });
            }
            for participant in &channel.participants {
                if participant.name.to_lowercase().contains(query) {
                    people.push(person_row(cx, guild, channel, participant));
                }
            }
        }
    }

    // Channels with people in them first; the sort is stable, so sidebar order breaks ties.
    places.sort_by_key(|place| std::cmp::Reverse(place.count));
    places.truncate(MAX_RESULTS);
    people.truncate(MAX_RESULTS);

    let calls: Vec<Row> = cx
        .snapshot
        .private_channels
        .iter()
        .filter(|channel| channel.name.to_lowercase().contains(query))
        .take(MAX_RESULTS)
        .map(|channel| Row {
            kind: RowKind::Place,
            id: id(channel.id),
            label: channel.name.as_str().into(),
            sublabel: if channel.group { "Group · call everyone" } else { "Direct message · call" }.into(),
            channel_id: id(channel.id),
            can_connect: true,
            current: current == Some(channel.id),
            ..Default::default()
        })
        .collect();

    let mut rows = Vec::new();
    if !places.is_empty() {
        rows.push(section("channels", "CHANNELS", places.len(), None));
        rows.extend(places);
    }
    if !calls.is_empty() {
        rows.push(section("direct", "DIRECT MESSAGES", calls.len(), None));
        rows.extend(calls);
    }
    if !people.is_empty() {
        rows.push(section("people", "PEOPLE IN VOICE", people.len(), None));
        rows.extend(people);
    }
    rows
}

/// `expanded`: `Some` for collapsible sections.
fn section(key: &str, label: &str, count: usize, expanded: Option<bool>) -> Row {
    Row {
        kind: RowKind::Section,
        id: key.into(),
        label: label.into(),
        count: count as i32,
        collapsible: expanded.is_some(),
        expanded: expanded.unwrap_or(true),
        ..Default::default()
    }
}

fn call_rows(cx: &Context, call: &CallView, rows: &mut Vec<Row>) {
    let current = cx.current_channel() == Some(call.channel_id);
    let others = call.participants.iter().filter(|p| !p.is_me).count();
    rows.push(Row {
        kind: RowKind::Call,
        id: id(call.channel_id),
        label: call.name.as_str().into(),
        sublabel: if call.ringing_me {
            "Calling you".into()
        } else if current && others == 0 {
            "Ringing…".into()
        } else if call.participants.len() == 1 {
            "1 in the call".into()
        } else {
            format!("{} in the call", call.participants.len()).into()
        },
        channel_id: id(call.channel_id),
        count: call.participants.len() as i32,
        can_connect: true,
        current,
        pinned: call.group,
        ..Default::default()
    });
    for participant in &call.participants {
        let audio = cx.settings.user(participant.user_id);
        rows.push(Row {
            kind: RowKind::Participant,
            id: id(participant.user_id),
            label: participant.name.as_str().into(),
            channel_id: id(call.channel_id),
            current,
            initial: initial(&participant.name).into(),
            self_mute: participant.self_mute,
            self_deaf: participant.self_deaf,
            live: participant.streaming,
            speaking: current && cx.speaking.contains(&participant.user_id),
            is_me: participant.is_me,
            local_mute: audio.muted && !participant.is_me,
            volume: i32::from(audio.volume),
            ..Default::default()
        });
    }
}

/// The DM channel to call a user in, if we have one with them.
fn dm_with(cx: &Context, user_id: Id) -> SharedString {
    cx.snapshot
        .private_channels
        .iter()
        .find(|channel| !channel.group && channel.recipients.contains(&user_id))
        .map(|channel| id(channel.id))
        .unwrap_or_default()
}

fn guild_rows(cx: &Context, guild: &GuildView, expanded: bool, rows: &mut Vec<Row>) {
    let show_empty = cx.settings.servers.show_empty.contains(&guild.id);
    let current = cx.current_channel();
    rows.push(Row {
        kind: RowKind::Guild,
        id: id(guild.id),
        label: guild.name.as_str().into(),
        expanded,
        pinned: cx.settings.servers.is_pinned(guild.id),
        show_empty,
        count: guild.voice_count() as i32,
        ..Default::default()
    });
    if !expanded {
        return;
    }

    let is_empty = |channel: &ChannelView| channel.participants.is_empty() && current != Some(channel.id);
    for channel in &guild.channels {
        if is_empty(channel) && !show_empty {
            continue;
        }
        rows.push(Row {
            kind: RowKind::Channel,
            id: id(channel.id),
            label: channel.name.as_str().into(),
            guild_id: id(guild.id),
            count: channel.participants.len() as i32,
            limit: channel.user_limit as i32,
            can_connect: channel.can_connect,
            current: current == Some(channel.id),
            ..Default::default()
        });
        if channel.participants.is_empty() {
            continue;
        }
        // Every participant of a channel can be moved to the same channels.
        let targets: Vec<Target> = guild
            .channels
            .iter()
            .filter(|target| target.can_connect && target.id != channel.id)
            .map(|target| Target { id: id(target.id), name: target.name.as_str().into() })
            .collect();
        let targets = ModelRc::new(VecModel::from(targets));
        for participant in &channel.participants {
            rows.push(Row {
                kind: RowKind::Participant,
                targets: targets.clone(),
                ..participant_fields(cx, guild, channel, participant)
            });
        }
    }

    let empty = guild.channels.iter().filter(|channel| is_empty(channel)).count();
    if empty > 0 {
        rows.push(Row {
            kind: RowKind::More,
            guild_id: id(guild.id),
            count: empty as i32,
            expanded: show_empty,
            ..Default::default()
        });
    }
}

/// What participant and person rows share.
fn participant_fields(cx: &Context, guild: &GuildView, channel: &ChannelView, participant: &Participant) -> Row {
    let audio = cx.settings.user(participant.user_id);
    let current = cx.current_channel() == Some(channel.id);
    Row {
        id: id(participant.user_id),
        label: participant.name.as_str().into(),
        guild_id: id(guild.id),
        channel_id: id(channel.id),
        can_connect: channel.can_connect,
        current,
        initial: initial(&participant.name).into(),
        self_mute: participant.self_mute,
        self_deaf: participant.self_deaf,
        server_mute: participant.server_mute,
        server_deaf: participant.server_deaf,
        live: participant.streaming,
        speaking: current && cx.speaking.contains(&participant.user_id),
        is_me: participant.is_me,
        local_mute: audio.muted && !participant.is_me,
        volume: i32::from(audio.volume),
        can_mute: participant.moderation.mute,
        can_deafen: participant.moderation.deafen,
        can_move: participant.moderation.move_,
        call_id: if participant.is_me { SharedString::new() } else { dm_with(cx, participant.user_id) },
        ..Default::default()
    }
}

/// Someone found by search.
fn person_row(cx: &Context, guild: &GuildView, channel: &ChannelView, participant: &Participant) -> Row {
    let fields = participant_fields(cx, guild, channel, participant);
    Row {
        kind: RowKind::Person,
        sublabel: if fields.current {
            "In your channel".into()
        } else {
            format!("{} · {}", channel.name, guild.name).into()
        },
        // Moderation needs the "Move to" targets; it stays in the server list.
        can_mute: false,
        can_deafen: false,
        can_move: false,
        ..fields
    }
}

fn friend_row(cx: &Context, friend: &FriendInVoice) -> Row {
    let audio = cx.settings.user(friend.user_id);
    Row {
        kind: RowKind::Person,
        id: id(friend.user_id),
        label: friend.name.as_str().into(),
        sublabel: format!("{} · {}", friend.channel_name, friend.guild_name).into(),
        guild_id: id(friend.guild_id),
        channel_id: id(friend.channel_id),
        can_connect: friend.can_connect,
        initial: initial(&friend.name).into(),
        self_mute: friend.self_mute,
        self_deaf: friend.self_deaf,
        live: friend.streaming,
        local_mute: audio.muted,
        volume: i32::from(audio.volume),
        call_id: dm_with(cx, friend.user_id),
        ..Default::default()
    }
}

fn id(id: Id) -> SharedString {
    id.to_string().into()
}

pub fn initial(name: &str) -> String {
    name.chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_else(|| "?".to_owned())
}

#[cfg(test)]
mod tests {
    use bardo_core::{Moderation, VoiceStatus, VoiceView};

    use super::*;

    fn participant(id: u64, name: &str) -> Participant {
        Participant {
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
            moderation: Moderation::default(),
        }
    }

    fn channel(id: u64, name: &str, participants: Vec<Participant>) -> ChannelView {
        ChannelView { id: Id(id), name: name.to_owned(), user_limit: 0, can_connect: true, participants }
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            guilds: vec![
                GuildView {
                    id: Id(1),
                    name: "Home".to_owned(),
                    channels: vec![
                        channel(10, "General", vec![participant(100, "Ana")]),
                        channel(11, "Empty", Vec::new()),
                        channel(12, "Also empty", Vec::new()),
                    ],
                },
                GuildView {
                    id: Id(2),
                    name: "Big public server".to_owned(),
                    channels: vec![channel(20, "Lobby", vec![participant(200, "Bob")])],
                },
            ],
            friends_in_voice: vec![FriendInVoice {
                user_id: Id(200),
                name: "Bobby".to_owned(),
                guild_id: Id(2),
                guild_name: "Big public server".to_owned(),
                channel_id: Id(20),
                channel_name: "Lobby".to_owned(),
                can_connect: true,
                self_mute: false,
                self_deaf: false,
                streaming: false,
            }],
            ..Snapshot::default()
        }
    }

    fn labels(snapshot: &Snapshot, settings: &Settings, list: &ListState) -> Vec<String> {
        let speaking = HashSet::new();
        let cx = Context { snapshot, settings, speaking: &speaking, list };
        rows(&cx)
            .iter()
            .map(|row| match row.kind {
                RowKind::More => format!("more:{}", row.count),
                _ => row.label.to_string(),
            })
            .collect()
    }

    #[test]
    fn without_pins_all_servers_are_listed_folded() {
        let labels = labels(&snapshot(), &Settings::default(), &ListState::default());
        assert_eq!(labels, [
            "FRIENDS IN VOICE",
            "Bobby",
            "SERVERS",
            "Pin the servers you use to keep them here: hover one below and click the pin.",
            "OTHER SERVERS",
            "Home",
            "Big public server"
        ]);
    }

    #[test]
    fn pinned_servers_are_expanded_with_empty_channels_folded() {
        let mut settings = Settings::default();
        settings.servers.set_pinned(Id(1), true);
        let list = ListState::default();
        assert_eq!(labels(&snapshot(), &settings, &list), [
            "FRIENDS IN VOICE",
            "Bobby",
            "SERVERS",
            "Home",
            "General",
            "Ana",
            "more:2",
            "OTHER SERVERS"
        ]);

        settings.servers.toggle_show_empty(Id(1));
        let labels = labels(&snapshot(), &settings, &list);
        assert!(labels.contains(&"Empty".to_owned()) && labels.contains(&"more:2".to_owned()));
    }

    #[test]
    fn the_server_we_are_in_is_always_listed() {
        let mut snapshot = snapshot();
        snapshot.voice = Some(VoiceView {
            guild_id: Some(Id(2)),
            channel_id: Id(20),
            guild_name: "Big public server".to_owned(),
            channel_name: "Lobby".to_owned(),
            status: VoiceStatus::Connected,
            self_mute: false,
            self_deaf: false,
        });
        let labels = labels(&snapshot, &Settings::default(), &ListState::default());
        // Bob is a friend but already with us, so he is not listed twice.
        assert_eq!(labels[..4], ["SERVERS", "Big public server", "Lobby", "Bob"]);
        let speaking = HashSet::new();
        let list = ListState::default();
        let settings = Settings::default();
        let cx = Context { snapshot: &snapshot, settings: &settings, speaking: &speaking, list: &list };
        assert_eq!(watched_guilds(&cx), HashSet::from([Id(2)]));
    }

    #[test]
    fn calls_come_first_and_search_finds_dms() {
        let mut snapshot = snapshot();
        snapshot.calls = vec![CallView {
            channel_id: Id(50),
            name: "Bobby".to_owned(),
            group: false,
            participants: vec![participant(200, "Bobby")],
            ringing_me: true,
            ringing_others: false,
        }];
        snapshot.private_channels = vec![bardo_core::PrivateChannelView {
            id: Id(50),
            name: "Bobby".to_owned(),
            group: false,
            recipients: vec![Id(200)],
        }];
        let labels = labels(&snapshot, &Settings::default(), &ListState::default());
        assert_eq!(labels[..3], ["CALLS", "Bobby", "Bobby"]);

        let list = ListState { query: "bob".to_owned(), ..Default::default() };
        let speaking = HashSet::new();
        let settings = Settings::default();
        let cx = Context { snapshot: &snapshot, settings: &settings, speaking: &speaking, list: &list };
        let rows = rows(&cx);
        let dm = rows.iter().find(|row| row.kind == RowKind::Place && row.guild_id.is_empty()).unwrap();
        assert_eq!(dm.channel_id, "50");
        // Bob, in voice in a server, can be called in his DM.
        let bob = rows.iter().find(|row| row.kind == RowKind::Person).unwrap();
        assert_eq!(bob.call_id, "50");
    }

    #[test]
    fn search_finds_channels_and_people() {
        let list = ListState { query: "e".to_owned(), ..Default::default() };
        // Lobby has someone in it, so it comes before Home's empty channels.
        assert_eq!(labels(&snapshot(), &Settings::default(), &list)[..3], ["CHANNELS", "General", "Lobby"]);
        let list = ListState { query: "lob".to_owned(), ..Default::default() };
        assert_eq!(labels(&snapshot(), &Settings::default(), &list), ["CHANNELS", "Lobby"]);
        let list = ListState { query: "ANA".to_owned(), ..Default::default() };
        assert_eq!(labels(&snapshot(), &Settings::default(), &list), ["PEOPLE IN VOICE", "Ana"]);

        let speaking = HashSet::new();
        let settings = Settings::default();
        let snapshot = snapshot();
        let cx = Context { snapshot: &snapshot, settings: &settings, speaking: &speaking, list: &list };
        assert_eq!(first_result(&rows(&cx)), Some(("1".to_owned(), "10".to_owned())));
    }
}
