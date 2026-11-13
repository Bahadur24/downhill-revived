//! Discord Rich Presence (finding 106): what the player is doing, on their Discord profile.
//!
//! The game core works out the state (port/hle/presence.c) and sends it through
//! `host_service_presence` when it changes; port/hle/host.c hands it here (`c.rs`). A thread of its
//! own (`worker.rs`) talks to Discord's local IPC through the discord-rich-presence crate: a named
//! pipe on Windows, a Unix socket on macOS and Linux. No Discord SDK. Discord running or not, the
//! player sees nothing of it but their profile: no error box, one log line at most while Discord
//! isn't there, a quiet retry every 30 s.
//!
//! This file is the part without Discord: the state, the two lines and the elapsed time Discord
//! shows for it (`payload`), and the pacing (`Pacer`: only a change is sent, at most one update per
//! 15 s, which is about what Discord allows).

use std::time::{Duration, Instant};

mod c;
mod worker;

pub use worker::{start, Worker};

/// The Discord application (public: every Rich Presence client names its own).
pub const APP_ID: &str = "1564742442857799934";
/// The large image (the art asset uploaded to the application) and its hover text.
pub const LOGO: &str = "logo";
pub const LOGO_TEXT: &str = "Downhill Revived";
/// The two buttons other people see on the profile.
pub const BUTTONS: [(&str, &str); 2] = [
    (
        "Get Downhill Revived",
        "https://downhill-revived.pages.dev/",
    ),
    ("Join the Discord", "https://discord.gg/XrSeTyzfr2"),
];
/// The shortest time between two updates, and between two tries to reach Discord.
pub const MIN_GAP: Duration = Duration::from_secs(15);
pub const RETRY: Duration = Duration::from_secs(30);

/// What the player is doing: port/host/host_api.h's PRESENCE_*, in that order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kind {
    /// Nothing to show (the presence is cleared).
    #[default]
    None,
    Menus,
    /// A race outside the career (single event and the other modes).
    Race,
    Career,
    /// In an online room, before its race.
    Room,
    OnlineRace,
    Movie,
}

impl Kind {
    pub fn from_c(kind: i32) -> Kind {
        match kind {
            1 => Kind::Menus,
            2 => Kind::Race,
            3 => Kind::Career,
            4 => Kind::Room,
            5 => Kind::OnlineRace,
            6 => Kind::Movie,
            _ => Kind::None,
        }
    }

    fn racing(self) -> bool {
        matches!(self, Kind::Race | Kind::Career | Kind::OnlineRace)
    }
}

/// The state as the core sends it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct State {
    pub kind: Kind,
    /// The track as the game names it ("ALPINE MX"); empty when not known.
    pub track: String,
    /// A race: the player's place (1 first; 0 not known) and the riders in it.
    pub place: u32,
    pub riders: u32,
    /// An online room: its join code (empty for a private room), the players in it and how many it
    /// takes.
    pub room: String,
    pub members: u32,
    pub max_members: u32,
}

/// What Discord shows: the first line, the second, and the start of the elapsed time (Unix ms).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Payload {
    pub details: String,
    pub state: Option<String>,
    pub start: i64,
}

/// "ALPINE MX" as "Alpine MX": a word of four letters or more in title case, a shorter one (MX,
/// TDH, BC) as it is.
pub fn track_name(name: &str) -> String {
    name.split_whitespace()
        .map(|w| {
            if w.chars().count() < 4 {
                return w.to_string();
            }
            let mut c = w.chars();
            let first = c.next().map(|f| f.to_ascii_uppercase()).into_iter();
            first.chain(c.map(|x| x.to_ascii_lowercase())).collect()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// 1st, 2nd, 3rd, 4th ... 11th, 12th, 13th ... 21st.
pub fn ordinal(n: u32) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// The two lines for `s`, its elapsed time counted from `start`. None: nothing to show.
pub fn payload(s: &State, start: i64) -> Option<Payload> {
    let track = track_name(&s.track);
    let on = |what: &str| {
        if track.is_empty() {
            what.to_string()
        } else {
            format!("{what}: {track}")
        }
    };
    let place = (s.place > 0 && s.riders > 1 && s.place <= s.riders)
        .then(|| format!("{} of {}", ordinal(s.place), s.riders));
    let (details, state) = match s.kind {
        Kind::None => return None,
        Kind::Menus => ("In menus".to_string(), None),
        Kind::Race => (on("Racing"), place),
        Kind::Career => (on("Career"), place),
        Kind::OnlineRace => (on("Online race"), place),
        // a private room comes without its code (port/hle/presence.c keeps it): no code shown
        Kind::Room => (
            if s.room.is_empty() {
                "Online room".to_string()
            } else {
                format!("Online room {}", s.room)
            },
            (s.max_members > 0).then(|| format!("{}/{} riders", s.members, s.max_members)),
        ),
        Kind::Movie => ("Watching a movie".to_string(), None),
    };
    Some(Payload {
        details,
        state,
        start,
    })
}

/// Where the elapsed time starts: the session's start, and for a race the moment it began (a new
/// race kind or track; a place changing keeps it).
#[derive(Debug)]
pub struct Clock {
    session: i64,
    race: Option<(Kind, String, i64)>,
}

impl Clock {
    pub fn new(session: i64) -> Clock {
        Clock {
            session,
            race: None,
        }
    }

    pub fn start(&mut self, s: &State, now: i64) -> i64 {
        if !s.kind.racing() {
            self.race = None;
            return self.session;
        }
        match &self.race {
            Some((kind, track, at)) if *kind == s.kind && *track == s.track => *at,
            _ => {
                self.race = Some((s.kind, s.track.clone(), now));
                now
            }
        }
    }
}

/// What the worker does next.
#[derive(Debug, PartialEq, Eq)]
pub enum Due {
    /// Discord shows what it should.
    Nothing,
    /// Send this (None: clear).
    Send(Option<Payload>),
    /// A change waits for the gap since the last update.
    Wait(Duration),
}

/// Only a change is sent, at most one per MIN_GAP; changes in between are merged (the last wins).
#[derive(Debug, Default)]
pub struct Pacer {
    wanted: Option<Payload>,
    shown: Option<Payload>,
    last: Option<Instant>,
}

impl Pacer {
    pub fn want(&mut self, p: Option<Payload>) {
        self.wanted = p;
    }

    pub fn due(&self, now: Instant) -> Due {
        if self.wanted == self.shown {
            return Due::Nothing;
        }
        match self
            .last
            .map(|t| (t + MIN_GAP).saturating_duration_since(now))
        {
            Some(wait) if !wait.is_zero() => Due::Wait(wait),
            _ => Due::Send(self.wanted.clone()),
        }
    }

    pub fn sent(&mut self, p: Option<Payload>, now: Instant) {
        self.shown = p;
        self.last = Some(now);
    }

    /// What Discord shows (None: nothing).
    pub fn shown(&self) -> Option<&Payload> {
        self.shown.as_ref()
    }

    /// The connection is gone: Discord shows nothing from this game any more.
    pub fn lost(&mut self) {
        self.shown = None;
    }
}

/// Whether DHD_DISCORD's value (None: not set) turns the presence on, as the game reads the
/// switch (dhd_settings: an off word is off, anything else on; unset, the built-in value).
pub fn enabled(setting: Option<&str>) -> bool {
    let Some(s) = dhd_settings::table::find("discord") else {
        return false;
    };
    let value = setting
        .filter(|v| !v.trim().is_empty())
        .unwrap_or(s.builtin);
    s.canonical(value) == Some("on")
}

#[cfg(test)]
mod tests;
