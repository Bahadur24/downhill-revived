//! The state -> Discord payload mapping and the pacing, without Discord.

use super::*;
use std::time::{Duration, Instant};

fn state(kind: Kind, track: &str, place: u32, riders: u32) -> State {
    State {
        kind,
        track: track.into(),
        place,
        riders,
        ..State::default()
    }
}

fn lines(s: &State) -> Option<(String, Option<String>)> {
    payload(s, 7).map(|p| (p.details, p.state))
}

fn pair(details: &str, state: Option<&str>) -> Option<(String, Option<String>)> {
    Some((details.to_string(), state.map(str::to_string)))
}

#[test]
fn the_lines_of_each_state() {
    assert_eq!(lines(&State::default()), None);
    assert_eq!(
        lines(&state(Kind::Menus, "ALPINE", 2, 6)),
        pair("In menus", None)
    );
    assert_eq!(
        lines(&state(Kind::Race, "ALPINE MX", 2, 6)),
        pair("Racing: Alpine MX", Some("2nd of 6"))
    );
    assert_eq!(
        lines(&state(Kind::Career, "PERU TDH", 1, 10)),
        pair("Career: Peru TDH", Some("1st of 10"))
    );
    assert_eq!(
        lines(&state(Kind::OnlineRace, "BC TDX", 3, 4)),
        pair("Online race: BC TDX", Some("3rd of 4"))
    );
    assert_eq!(
        lines(&state(Kind::Movie, "", 0, 0)),
        pair("Watching a movie", None)
    );
}

#[test]
fn public_and_private_rooms() {
    // a public room: its code and the riders
    let public = State {
        kind: Kind::Room,
        room: "K7M2QX".into(),
        members: 3,
        max_members: 6,
        ..State::default()
    };
    assert_eq!(
        lines(&public),
        pair("Online room K7M2QX", Some("3/6 riders"))
    );
    // a private room comes without its code: the riders, and the code nowhere in the activity
    let private = State {
        room: String::new(),
        ..public.clone()
    };
    assert_eq!(lines(&private), pair("Online room", Some("3/6 riders")));
    let json = serde_json::to_value(worker::activity(&payload(&private, 0).unwrap()))
        .unwrap()
        .to_string();
    assert!(!json.contains("K7M2QX"), "{json}");
    // the room's size not known yet: no second line
    assert_eq!(
        lines(&State {
            max_members: 0,
            ..private
        }),
        pair("Online room", None)
    );
}

#[test]
fn a_place_only_when_it_makes_sense() {
    // not known yet, alone on the track, or past the riders: no second line
    assert_eq!(
        lines(&state(Kind::Race, "MOAB", 0, 6)),
        pair("Racing: Moab", None)
    );
    assert_eq!(
        lines(&state(Kind::Race, "MOAB", 1, 1)),
        pair("Racing: Moab", None)
    );
    assert_eq!(
        lines(&state(Kind::Race, "MOAB", 7, 6)),
        pair("Racing: Moab", None)
    );
    // no track name: the kind alone
    assert_eq!(
        lines(&state(Kind::Race, "", 2, 6)),
        pair("Racing", Some("2nd of 6"))
    );
}

#[test]
fn names_and_places() {
    assert_eq!(track_name("ALPINE"), "Alpine");
    assert_eq!(track_name("GLACIER X"), "Glacier X");
    assert_eq!(track_name("AUBER STD"), "Auber STD");
    assert_eq!(track_name("CITY"), "City");
    assert_eq!(track_name("BC"), "BC");
    assert_eq!(track_name("  "), "");
    let words: Vec<_> = [1, 2, 3, 4, 10, 11, 12, 13, 21, 22, 23, 101, 111]
        .iter()
        .map(|&n| ordinal(n))
        .collect();
    assert_eq!(
        words,
        [
            "1st", "2nd", "3rd", "4th", "10th", "11th", "12th", "13th", "21st", "22nd", "23rd",
            "101st", "111th"
        ]
    );
}

#[test]
fn the_elapsed_time() {
    let mut clock = Clock::new(1000);
    assert_eq!(clock.start(&state(Kind::Menus, "", 0, 0), 5000), 1000);
    // a race counts from its start; a new place keeps it, a new track or kind starts again
    assert_eq!(clock.start(&state(Kind::Race, "ALPINE", 0, 6), 6000), 6000);
    assert_eq!(clock.start(&state(Kind::Race, "ALPINE", 3, 6), 9000), 6000);
    assert_eq!(clock.start(&state(Kind::Race, "MOAB", 3, 6), 9500), 9500);
    assert_eq!(clock.start(&state(Kind::Career, "MOAB", 3, 6), 9700), 9700);
    // back in the menus: the session's time; the next race starts again
    assert_eq!(clock.start(&state(Kind::Menus, "", 0, 0), 12000), 1000);
    assert_eq!(
        clock.start(&state(Kind::Career, "MOAB", 0, 6), 13000),
        13000
    );
    assert_eq!(
        payload(&state(Kind::Menus, "", 0, 0), 1000).unwrap().start,
        1000
    );
}

#[test]
fn only_changes_and_at_most_one_update_per_gap() {
    let t0 = Instant::now();
    let at = |s: u64| t0 + Duration::from_secs(s);
    let p = |details: &str| {
        Some(Payload {
            details: details.into(),
            state: None,
            start: 0,
        })
    };
    let mut pacer = Pacer::default();
    assert_eq!(pacer.due(at(0)), Due::Nothing);
    // the first update goes at once
    pacer.want(p("In menus"));
    assert_eq!(pacer.due(at(0)), Due::Send(p("In menus")));
    pacer.sent(p("In menus"), at(0));
    assert_eq!(pacer.due(at(1)), Due::Nothing);
    // the same again: nothing
    pacer.want(p("In menus"));
    assert_eq!(pacer.due(at(2)), Due::Nothing);
    // changes inside the gap wait for it, the last one wins
    pacer.want(p("Racing: Alpine"));
    assert_eq!(pacer.due(at(5)), Due::Wait(Duration::from_secs(10)));
    pacer.want(p("Career: Alpine"));
    assert_eq!(pacer.due(at(14)), Due::Wait(Duration::from_secs(1)));
    assert_eq!(pacer.due(at(15)), Due::Send(p("Career: Alpine")));
    pacer.sent(p("Career: Alpine"), at(15));
    // a change and back inside the gap: nothing to send
    pacer.want(p("In menus"));
    pacer.want(p("Career: Alpine"));
    assert_eq!(pacer.due(at(20)), Due::Nothing);
    // clearing is a change too
    pacer.want(None);
    assert_eq!(pacer.due(at(40)), Due::Send(None));
    pacer.sent(None, at(40));
    assert_eq!(pacer.due(at(41)), Due::Nothing);
    // after a lost connection Discord shows nothing: the wanted state goes again
    pacer.want(p("In menus"));
    pacer.sent(p("In menus"), at(60));
    pacer.lost();
    assert_eq!(pacer.shown(), None);
    assert_eq!(pacer.due(at(80)), Due::Send(p("In menus")));
}

#[test]
fn the_setting() {
    // unset: the built-in value (the tools' runs, without dhd.ini) is off; dhd.ini's default is on
    assert!(!enabled(None));
    assert!(!enabled(Some("")));
    assert_eq!(
        dhd_settings::table::find("discord").map(|s| s.default),
        Some("on")
    );
    for on in ["on", "ON", "1", "yes", "true", "anything"] {
        assert!(enabled(Some(on)), "{on}");
    }
    for off in ["off", "OFF", "0", "no", "false"] {
        assert!(!enabled(Some(off)), "{off}");
    }
}

#[test]
fn the_activity_discord_gets() {
    let p = Payload {
        details: "Racing: Alpine".into(),
        state: Some("2nd of 6".into()),
        start: 1_700_000_000_000,
    };
    let json = serde_json::to_value(worker::activity(&p)).unwrap();
    assert_eq!(json["details"], "Racing: Alpine");
    assert_eq!(json["state"], "2nd of 6");
    assert_eq!(json["timestamps"]["start"], 1_700_000_000_000i64);
    assert_eq!(json["assets"]["large_image"], "logo");
    assert_eq!(json["assets"]["large_text"], "Downhill Revived");
    assert_eq!(
        json["buttons"],
        serde_json::json!([
            {"label": "Get Downhill Revived", "url": "https://downhill-revived.pages.dev/"},
            {"label": "Join the Discord", "url": "https://discord.gg/XrSeTyzfr2"}
        ])
    );
    // no second line: no state key at all (Discord refuses an empty one)
    let menus = Payload {
        details: "In menus".into(),
        state: None,
        start: 0,
    };
    assert!(serde_json::to_value(worker::activity(&menus))
        .unwrap()
        .get("state")
        .is_none());
}
