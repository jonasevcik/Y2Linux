//! The Hands-Free conversation as headsets hold it: a normal set-up, each battery command, and
//! lines that must never do harm.
// SPDX-License-Identifier: GPL-2.0-only
use super::hfp_at::{Reading, Session, Source, Step};

fn say(session: &mut Session, text: &str) -> Vec<Step> {
    session.feed(text.as_bytes())
}

fn one(text: &str) -> Step {
    let mut steps = say(&mut Session::default(), text);
    assert_eq!(steps.len(), 1, "{text:?}");
    steps.remove(0)
}

fn reply(step: &Step) -> String {
    String::from_utf8_lossy(&step.reply).into_owned()
}

fn level(percent: u8, source: Source) -> Option<Reading> {
    Some(Reading { percent, source })
}

#[test]
fn a_service_level_set_up_is_answered_to_its_end() {
    let mut session = Session::default();
    let features = say(&mut session, "AT+BRSF=1008\r");
    assert_eq!(reply(&features[0]), "\r\n+BRSF: 256\r\n\r\nOK\r\n");
    for (line, answer) in [
        ("AT+BAC=1\r", "\r\nOK\r\n"),
        ("AT+CMER=3,0,0,1\r", "\r\nOK\r\n"),
        ("AT+BIND=1,2\r", "\r\nOK\r\n"),
        ("AT+BIND=?\r", "\r\n+BIND: (1,2)\r\n\r\nOK\r\n"),
        (
            "AT+BIND?\r",
            "\r\n+BIND: 1,1\r\n\r\n+BIND: 2,1\r\n\r\nOK\r\n",
        ),
        ("AT+CIND?\r", "\r\n+CIND: 1,0,0,0,5,0,5\r\n\r\nOK\r\n"),
    ] {
        assert_eq!(reply(&say(&mut session, line)[0]), answer, "{line}");
    }
    let indicators = reply(&say(&mut session, "AT+CIND=?\r")[0]);
    assert!(indicators.contains(r#"("battchg",(0,5))"#) && indicators.ends_with("OK\r\n"));
}

#[test]
fn the_standard_battery_indicator_is_read() {
    let step = one("AT+BIEV=2,85\r");
    assert_eq!(step.reading, level(85, Source::Biev));
    assert_eq!(reply(&step), "\r\nOK\r\n");
    assert_eq!(one("AT+BIEV=2,0\r").reading, level(0, Source::Biev));
    assert_eq!(one("AT+BIEV=2,100\r").reading, level(100, Source::Biev));
    // Another indicator is accepted and not read.
    let other = one("AT+BIEV=1,1\r");
    assert_eq!(
        (reply(&other).as_str(), other.reading),
        ("\r\nOK\r\n", None)
    );
}

#[test]
fn a_battery_indicator_out_of_range_or_malformed_is_an_error() {
    for line in [
        "AT+BIEV=2,101\r",
        "AT+BIEV=2,-1\r",
        "AT+BIEV=2\r",
        "AT+BIEV=2,x\r",
        "AT+BIEV=,\r",
        "AT+BIEV=2,300\r",
    ] {
        let step = one(line);
        assert_eq!(reply(&step), "\r\nERROR\r\n", "{line}");
        assert_eq!(step.reading, None, "{line}");
    }
}

#[test]
fn apples_accessory_battery_is_ten_percent_steps() {
    assert_eq!(
        one("AT+IPHONEACCEV=1,1,9\r").reading,
        level(100, Source::Accessory)
    );
    assert_eq!(
        one("AT+IPHONEACCEV=2,1,4,2,0\r").reading,
        level(50, Source::Accessory)
    );
    // The battery need not come first; a dock-only report carries no level.
    assert_eq!(
        one("AT+IPHONEACCEV=2,2,1,1,0\r").reading,
        level(10, Source::Accessory)
    );
    let dock = one("AT+IPHONEACCEV=1,2,1\r");
    assert_eq!((reply(&dock).as_str(), dock.reading), ("\r\nOK\r\n", None));
}

#[test]
fn a_malformed_accessory_report_is_an_error() {
    for line in [
        "AT+IPHONEACCEV=1,1,10\r",
        "AT+IPHONEACCEV=2,1,5\r",
        "AT+IPHONEACCEV=0\r",
        "AT+IPHONEACCEV=9,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1\r",
        "AT+IPHONEACCEV=a,b,c\r",
        "AT+IPHONEACCEV=\r",
    ] {
        let step = one(line);
        assert_eq!(reply(&step), "\r\nERROR\r\n", "{line}");
        assert_eq!(step.reading, None, "{line}");
    }
}

#[test]
fn apple_headsets_are_told_the_player_reads_batteries() {
    assert_eq!(
        reply(&one("AT+XAPL=05AC-1200-0100,10\r")),
        "\r\n+XAPL=iPhone,2\r\n\r\nOK\r\n"
    );
}

#[test]
fn plantronics_battery_is_a_level_out_of_its_top() {
    assert_eq!(
        one("AT+XEVENT=BATTERY,6,9,0,0\r").reading,
        level(67, Source::Event)
    );
    assert_eq!(
        one("at+xevent=battery,9,9\r").reading,
        level(100, Source::Event)
    );
    for line in [
        "AT+XEVENT=BATTERY,10,9\r",
        "AT+XEVENT=BATTERY,1,0\r",
        "AT+XEVENT=BATTERY\r",
        "AT+XEVENT=USAGE,1,2\r",
    ] {
        let step = one(line);
        assert_eq!(
            (reply(&step).as_str(), step.reading),
            ("\r\nOK\r\n", None),
            "{line}"
        );
    }
}

#[test]
fn nothing_starts_a_call_or_a_sound() {
    for line in [
        "ATA\r",
        "ATD123;\r",
        "AT+BCC\r",
        "AT+BCS=1\r",
        "AT+CHUP\r",
        "ATH\r",
        "AT+BLDN\r",
    ] {
        let step = one(line);
        assert_eq!(reply(&step), "\r\nERROR\r\n", "{line}");
        assert_eq!(step.reading, None);
    }
}

#[test]
fn garbage_and_oversized_lines_are_refused_and_the_next_line_still_works() {
    let mut session = Session::default();
    assert_eq!(reply(&say(&mut session, "HELLO\r")[0]), "\r\nERROR\r\n");
    assert_eq!(
        reply(&say(&mut session, "\u{1}\u{2}AT+BIEV=2,50\r")[0]),
        "\r\nERROR\r\n"
    );
    let long = format!("AT+{}\r", "A".repeat(5000));
    let steps = say(&mut session, &long);
    assert_eq!(steps.len(), 1);
    assert_eq!(reply(&steps[0]), "\r\nERROR\r\n");
    assert_eq!(steps[0].reading, None);
    assert_eq!(
        say(&mut session, "AT+BIEV=2,33\r")[0].reading,
        level(33, Source::Biev)
    );
    let binary = session.feed(&[0xff, 0xfe, b'\r']);
    assert_eq!(reply(&binary[0]), "\r\nERROR\r\n");
}

#[test]
fn lines_arrive_in_pieces_and_in_batches() {
    let mut session = Session::default();
    assert!(say(&mut session, "AT+BI").is_empty());
    assert!(say(&mut session, "EV=2,7").is_empty());
    let first = say(&mut session, "1\r\nAT+BIEV=2,72\r");
    let seen: Vec<_> = first.iter().map(|s| s.reading).collect();
    assert_eq!(seen, [level(71, Source::Biev), level(72, Source::Biev)]);
}

#[test]
fn the_logged_line_is_printable_and_short() {
    let step = one(&format!("AT+X{}\r", "é".repeat(50)));
    assert!(step.line.len() <= 96 && step.line.chars().all(|c| c.is_ascii_graphic() || c == ' '));
    assert_eq!(one("at+biev=2,5\r").line, "at+biev=2,5");
}
