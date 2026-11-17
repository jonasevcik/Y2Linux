//! The text side of a Hands-Free link, played as the audio gateway: a headset opens it right after
//! it connects and tells the "phone" its battery level there (the Mac got the Niceboy Beans' level
//! 88 ms after this link came up). Only what that conversation needs is answered; nothing here
//! touches a socket, a call or audio, and every input is bounded and range-checked.
// SPDX-License-Identifier: GPL-2.0-only

/// The longest command line read; a longer one is refused (a headset's longest is about 60 bytes).
const MAX_LINE: usize = 256;
/// The logged text of one line.
const LOGGED: usize = 96;
/// What this gateway claims in `+BRSF`: bit 8, hands-free indicators (that is how `AT+BIEV` is
/// allowed). No call, voice or codec feature: nothing here carries audio.
const FEATURES: u16 = 0x100;
const OK: &[u8] = b"\r\nOK\r\n";
const ERROR: &[u8] = b"\r\nERROR\r\n";

/// Which command carried a battery level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// `AT+BIEV=2,<0-100>`: the standard hands-free indicator.
    Biev,
    /// `AT+IPHONEACCEV=n,1,<0-9>`: Apple's, in steps of 10 %.
    Accessory,
    /// `AT+XEVENT=BATTERY,<level>,<top>`: Plantronics'.
    Event,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Biev => "biev",
            Source::Accessory => "iphoneaccev",
            Source::Event => "xevent",
        }
    }
}

/// A headset's battery level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    pub percent: u8,
    pub source: Source,
}

/// One command line and what to answer.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Step {
    /// The line as received, printable and shortened (for the journal).
    pub line: String,
    pub reply: Vec<u8>,
    pub reading: Option<Reading>,
}

/// The state of one link: the bytes of a line not finished yet.
#[derive(Default)]
pub struct Session {
    line: Vec<u8>,
    /// A line that outgrew `MAX_LINE`: skipped to its end.
    skipping: bool,
}

impl Session {
    /// Take what arrived; one step per finished line (a line ends with a carriage return).
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Step> {
        let mut steps = Vec::new();
        for &byte in bytes {
            match byte {
                b'\n' => {}
                b'\r' => {
                    let line = std::mem::take(&mut self.line);
                    if std::mem::take(&mut self.skipping) {
                        steps.push(refused("(too long)"));
                    } else if !line.is_empty() {
                        steps.push(answer(&line));
                    }
                }
                _ if self.skipping => {}
                _ if self.line.len() >= MAX_LINE => {
                    self.line.clear();
                    self.skipping = true;
                }
                _ => self.line.push(byte),
            }
        }
        steps
    }
}

fn refused(line: &str) -> Step {
    Step {
        line: line.to_owned(),
        reply: ERROR.to_vec(),
        reading: None,
    }
}

fn answer(raw: &[u8]) -> Step {
    let printable = raw.iter().all(|b| (0x20..0x7f).contains(b));
    let line: String = String::from_utf8_lossy(raw)
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '?'
            }
        })
        .take(LOGGED)
        .collect();
    let Some(command) = printable.then(|| command(raw)).flatten() else {
        return Step {
            line,
            reply: ERROR.to_vec(),
            reading: None,
        };
    };
    let (reply, reading) = reply_to(&command);
    Step {
        line,
        reply,
        reading,
    }
}

/// The text after `AT`, upper-cased up to its `=` (the arguments keep their case), or none when the
/// line is not a command.
fn command(raw: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(raw).ok()?.trim();
    let rest = text.get(..2).filter(|at| at.eq_ignore_ascii_case("AT"))?;
    let rest = &text[rest.len()..];
    Some(match rest.find(['=', '?']) {
        Some(at) => rest[..at].to_ascii_uppercase() + &rest[at..],
        None => rest.to_ascii_uppercase(),
    })
}

fn info(text: &str) -> Vec<u8> {
    format!("\r\n{text}\r\n").into_bytes()
}

fn then_ok(mut text: Vec<u8>) -> Vec<u8> {
    text.extend_from_slice(OK);
    text
}

fn reply_to(command: &str) -> (Vec<u8>, Option<Reading>) {
    let plain = |reply: Vec<u8>| (reply, None);
    match command {
        "+CIND=?" => plain(then_ok(info(concat!(
            r#"+CIND: ("service",(0,1)),("call",(0,1)),("callsetup",(0,3)),"#,
            r#"("callheld",(0,2)),("signal",(0,5)),("roam",(0,1)),("battchg",(0,5))"#
        )))),
        "+CIND?" => plain(then_ok(info("+CIND: 1,0,0,0,5,0,5"))),
        "+BIND=?" => plain(then_ok(info("+BIND: (1,2)"))),
        "+BIND?" => plain(then_ok(info("+BIND: 1,1\r\n\r\n+BIND: 2,1"))),
        "+CHLD=?" => plain(then_ok(info("+CHLD: (0)"))),
        "+COPS?" => plain(then_ok(info("+COPS: 0"))),
        _ => with_arguments(command).unwrap_or_else(|| plain(ERROR.to_vec())),
    }
}

/// Commands with arguments (`NAME=…`) and the few bare ones a headset sends while setting up.
fn with_arguments(command: &str) -> Option<(Vec<u8>, Option<Reading>)> {
    let (name, arguments) = match command.split_once('=') {
        Some((name, arguments)) => (name, arguments),
        None => (command, ""),
    };
    let done = |reading: Option<Reading>| Some((OK.to_vec(), reading));
    match name {
        "+BRSF" => Some((then_ok(info(&format!("+BRSF: {FEATURES}"))), None)),
        "+XAPL" => Some((then_ok(info("+XAPL=iPhone,2")), None)),
        "+BIEV" => indicator(arguments).and_then(done),
        "+IPHONEACCEV" => accessory(arguments).and_then(done),
        "+XEVENT" => done(event(arguments)),
        // Settings and queries a headset makes while it sets up; none starts a call or a sound.
        "" | "+BAC" | "+CMER" | "+BIND" | "+BIA" | "+NREC" | "+VGS" | "+VGM" | "+CLIP"
        | "+CCWA" | "+CMEE" | "+COPS" | "+CNUM" | "+CLCC" | "+BVRA" | "+CSRBATT" => done(None),
        _ => None,
    }
}

/// `+BIEV=<indicator>,<value>`: indicator 2 is the battery, 0 to 100. Other indicators are
/// accepted and ignored; a battery value out of range is an error. None: the line is malformed.
fn indicator(arguments: &str) -> Option<Option<Reading>> {
    let (id, value) = arguments.split_once(',')?;
    let id: u8 = id.trim().parse().ok()?;
    let value: u8 = value.trim().parse().ok()?;
    if id != 2 {
        return Some(None);
    }
    (value <= 100).then_some(Some(Reading {
        percent: value,
        source: Source::Biev,
    }))
}

/// `+IPHONEACCEV=<n>,<key>,<value>,…`: key 1 is the battery, 0 to 9 (steps of 10 %, 9 is full).
fn accessory(arguments: &str) -> Option<Option<Reading>> {
    let numbers: Vec<u16> = arguments
        .split(',')
        .map(|part| part.trim().parse().ok())
        .collect::<Option<_>>()?;
    let (&count, pairs) = numbers.split_first()?;
    if count == 0 || count > 8 || pairs.len() != 2 * usize::from(count) {
        return None;
    }
    let level = pairs
        .as_chunks::<2>()
        .0
        .iter()
        .find(|pair| pair[0] == 1)
        .map(|pair| pair[1]);
    Some(match level {
        Some(level) if level <= 9 => Some(Reading {
            percent: (level as u8 + 1) * 10,
            source: Source::Accessory,
        }),
        Some(_) => return None,
        None => None,
    })
}

/// `+XEVENT=BATTERY,<level>,<top>,…`: Plantronics' level out of its top level. Anything else (the
/// other events, a level above its top) is answered and not read.
fn event(arguments: &str) -> Option<Reading> {
    let mut parts = arguments.split(',').map(str::trim);
    if !parts.next()?.eq_ignore_ascii_case("BATTERY") {
        return None;
    }
    let level: u16 = parts.next()?.parse().ok()?;
    let top: u16 = parts.next()?.parse().ok()?;
    if top == 0 || top > 100 || level > top {
        return None;
    }
    Some(Reading {
        percent: ((u32::from(level) * 100 + u32::from(top) / 2) / u32::from(top)) as u8,
        source: Source::Event,
    })
}
