//! Original RFC 5545 text for known catalog dates; all-day events with no guessed time.
use crate::{Result, date, integrations::report_text};

/// Largest calendar file Mynou will produce.
pub const MAX_BYTES: usize = 2 * 1024 * 1024;
/// Largest number of events in one file; matches the calendar query row limit.
pub const MAX_EVENTS: usize = 200;
const LINE_OCTETS: usize = 75;
const TOO_LARGE: &str =
    "The calendar file would exceed 2 MiB; narrow the dates or choose one series";
const NO_EVENTS: &str = "No known episode dates match these filters, so there is nothing to export";

/// One known dated episode, already restricted to the requested window and scope.
pub struct Event<'a> {
    pub series_id: &'a str,
    pub series_title: &'a str,
    pub season: u32,
    pub episode: u32,
    pub episode_title: &'a str,
    pub air_date: &'a str,
}

/// Escape an RFC 5545 TEXT value; control characters become spaces except newlines.
pub fn escape_text(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\\' => result.push_str("\\\\"),
            ';' => result.push_str("\\;"),
            ',' => result.push_str("\\,"),
            '\n' => result.push_str("\\n"),
            '\r' => {}
            character if character.is_control() => result.push(' '),
            character => result.push(character),
        }
    }
    result
}

/// Fold one content line at 75 octets without splitting UTF-8 characters.
pub fn fold(line: &str) -> String {
    let mut result = String::with_capacity(line.len() + line.len() / 70 * 3 + 2);
    let mut width = 0;
    for character in line.chars() {
        let size = character.len_utf8();
        if width + size > LINE_OCTETS {
            result.push_str("\r\n ");
            width = 1;
        }
        result.push(character);
        width += size;
    }
    result.push_str("\r\n");
    result
}

fn compact(date_text: &str) -> Result<String> {
    date::day(date_text)?;
    Ok(date_text.replace('-', ""))
}

fn push(output: &mut String, line: &str) -> Result<()> {
    output.push_str(&fold(line));
    if output.len() > MAX_BYTES {
        return Err(TOO_LARGE.into());
    }
    Ok(())
}

/// Render events as a calendar. Fails for no events, too many events or oversized output.
pub fn render(events: &[Event<'_>]) -> Result<String> {
    if events.is_empty() {
        return Err(NO_EVENTS.into());
    }
    if events.len() > MAX_EVENTS {
        return Err(format!(
            "This window has more than {MAX_EVENTS} known episode dates; narrow the dates or choose one series"
        ));
    }
    let mut output = String::new();
    for line in [
        "BEGIN:VCALENDAR",
        "VERSION:2.0",
        "PRODID:-//Mynou//Episode dates//EN",
        "CALSCALE:GREGORIAN",
        "METHOD:PUBLISH",
        "X-WR-CALNAME:Mynou episode dates",
    ] {
        push(&mut output, line)?;
    }
    for event in events {
        let start = compact(event.air_date)?;
        let end = compact(&date::add(event.air_date, 1)?)?;
        let series = report_text(event.series_title, 512);
        let episode = report_text(event.episode_title, 512);
        let label = format!("S{:02}E{:02}", event.season, event.episode);
        let summary = if episode.is_empty() {
            format!("{series} {label}")
        } else {
            format!("{series} {label}: {episode}")
        };
        for line in [
            "BEGIN:VEVENT".to_owned(),
            format!(
                "UID:{}-s{:04}e{:04}@mynou.invalid",
                event.series_id, event.season, event.episode
            ),
            format!("DTSTAMP:{start}T000000Z"),
            format!("DTSTART;VALUE=DATE:{start}"),
            format!("DTEND;VALUE=DATE:{end}"),
            format!("SUMMARY:{}", escape_text(&summary)),
            format!(
                "DESCRIPTION:{}",
                escape_text(
                    "Known catalog air date (UTC day, not a premiere time). It may change."
                )
            ),
            "TRANSP:TRANSPARENT".to_owned(),
            "END:VEVENT".to_owned(),
        ] {
            push(&mut output, &line)?;
        }
    }
    push(&mut output, "END:VCALENDAR")?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event<'a>(title: &'a str, date: &'a str) -> Event<'a> {
        Event {
            series_id: "0123456789abcdef0123456789abcdef",
            series_title: title,
            season: 1,
            episode: 2,
            episode_title: "Pilot",
            air_date: date,
        }
    }

    #[test]
    fn escapes_separators_and_newlines() {
        assert_eq!(escape_text("a,b;c\\d\r\ne\u{7}"), "a\\,b\\;c\\\\d\\ne ");
    }

    #[test]
    fn folds_by_octets_without_splitting_characters() {
        let text = format!("SUMMARY:{}", "é".repeat(100));
        let folded = fold(&text);
        for line in folded.trim_end_matches("\r\n").split("\r\n") {
            assert!(line.len() <= LINE_OCTETS);
        }
        let joined = folded.replace("\r\n ", "");
        assert_eq!(joined, format!("{text}\r\n"));
    }

    #[test]
    fn renders_leap_day_and_year_end_all_day_events() {
        let leap = render(&[event("Show", "2024-02-29")]).unwrap();
        assert!(leap.contains("DTSTART;VALUE=DATE:20240229\r\n"));
        assert!(leap.contains("DTEND;VALUE=DATE:20240301\r\n"));
        let end = render(&[event("Show", "2025-12-31")]).unwrap();
        assert!(end.contains("DTEND;VALUE=DATE:20260101\r\n"));
        assert!(leap.starts_with("BEGIN:VCALENDAR\r\n") && leap.ends_with("END:VCALENDAR\r\n"));
        assert!(!leap.contains("VALARM") && !leap.contains("URL") && !leap.contains("ATTACH"));
    }

    #[test]
    fn injection_like_titles_cannot_add_properties() {
        let text = render(&[event("X\r\nEND:VEVENT\r\nATTENDEE:evil;,", "2024-01-01")]).unwrap();
        assert_eq!(text.matches("BEGIN:VEVENT").count(), 1);
        assert!(!text.contains("\r\nATTENDEE"));
    }

    #[test]
    fn rejects_empty_oversized_and_invalid_input() {
        assert!(render(&[]).is_err());
        let many: Vec<_> = (0..=MAX_EVENTS)
            .map(|_| event("Show", "2024-01-01"))
            .collect();
        assert!(render(&many).is_err());
        assert!(render(&[event("Show", "2024-02-30")]).is_err());
        assert!(render(&[event("Show", "9999-12-31")]).is_err());
    }
}
