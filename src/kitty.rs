//! Kitty in the terminal. Plain ASCII so it renders everywhere.
//! Disable with `--no-kitty` or `PREPKITTY_QUIET=1`.

use std::sync::atomic::{AtomicBool, Ordering};

use console::style;

static QUIET: AtomicBool = AtomicBool::new(false);

pub fn set_quiet(q: bool) {
    QUIET.store(q, Ordering::Relaxed);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mood {
    Happy,
    Content,
    Hungry,
    Sleepy,
    Sad,
    Celebrating,
    Thinking,
}

impl Mood {
    pub fn from_api(s: &str) -> Mood {
        match s {
            "happy" => Mood::Happy,
            "hungry" => Mood::Hungry,
            "sleepy" => Mood::Sleepy,
            "sad" => Mood::Sad,
            "celebrating" => Mood::Celebrating,
            "thinking" => Mood::Thinking,
            _ => Mood::Content,
        }
    }
}

/// Three lines of cat, by mood.
pub fn frame(mood: Mood) -> [&'static str; 3] {
    match mood {
        Mood::Happy => [r" /\_/\ ", r"( ^.^ )", r" > ^ < "],
        Mood::Content => [r" /\_/\ ", r"( o.o )", r" > ^ < "],
        Mood::Hungry => [r" /\_/\ ", r"( O.O )", r" > o < "],
        Mood::Sleepy => [r" /\_/\  z", r"( -.- ) Z", r" > ^ < "],
        Mood::Sad => [r" /\_/\ ", r"( ;_; )", r" > ^ < "],
        Mood::Celebrating => [r" /\_/\  *", r"( ^o^ )/", r" > ^ <  *"],
        Mood::Thinking => [r" /\_/\   ?", r"( o.o ) ", r" > ^ < "],
    }
}

/// Prints the kitty with up to three lines of text beside it.
pub fn say(mood: Mood, lines: &[String]) {
    if QUIET.load(Ordering::Relaxed) {
        for l in lines {
            println!("{l}");
        }
        return;
    }
    let art = frame(mood);
    let color = |s: &str| match mood {
        Mood::Celebrating | Mood::Happy => style(s.to_string()).yellow().bold(),
        Mood::Sad | Mood::Thinking => style(s.to_string()).magenta(),
        _ => style(s.to_string()).cyan(),
    };
    let n = lines.len().max(3);
    for i in 0..n {
        let cat = art.get(i).copied().unwrap_or("");
        let text = lines.get(i).map(String::as_str).unwrap_or("");
        // Pad before styling so ANSI codes don't break alignment.
        println!("  {} {}", color(&format!("{cat:<10}")), text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mood_has_three_lines() {
        for m in [
            Mood::Happy,
            Mood::Content,
            Mood::Hungry,
            Mood::Sleepy,
            Mood::Sad,
            Mood::Celebrating,
            Mood::Thinking,
        ] {
            let f = frame(m);
            assert!(f.iter().all(|l| l.is_ascii()), "{m:?} must be ASCII");
            assert!(
                f[1].contains('(') && f[1].contains(')'),
                "{m:?} needs a face"
            );
        }
    }

    #[test]
    fn maps_api_moods() {
        assert_eq!(Mood::from_api("sleepy"), Mood::Sleepy);
        assert_eq!(Mood::from_api("unknown"), Mood::Content);
    }
}
