use crate::types::{Playlist, PlaylistEntry};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum M3uError {
    #[error("Empty playlist")]
    Empty,
    #[error("No valid entries found")]
    NoEntries,
    #[error("Parse error: {0}")]
    Parse(String),
}

/// Parse an M3U playlist from text content.
///
/// Supports both simple M3U (one URL per line) and extended M3U (#EXTM3U header
/// with #EXTINF directives).
pub fn parse(content: &str) -> Result<Playlist, M3uError> {
    let content = content.trim();
    if content.is_empty() {
        return Err(M3uError::Empty);
    }

    let lines: Vec<&str> = content.lines().collect();

    let is_extended = lines
        .first()
        .map(|l| l.trim().starts_with("#EXTM3U"))
        .unwrap_or(false);

    let entries = if is_extended {
        parse_extended(&lines)
    } else {
        parse_simple(&lines)
    };

    if entries.is_empty() {
        return Err(M3uError::NoEntries);
    }

    Ok(Playlist { entries })
}

/// Parse simple M3U: each non-empty, non-comment line is a URL.
fn parse_simple(lines: &[&str]) -> Vec<PlaylistEntry> {
    lines
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|url| PlaylistEntry {
            url: url.to_string(),
            title: None,
            duration_secs: None,
        })
        .collect()
}

/// Parse extended M3U: #EXTINF:duration,title followed by URL.
fn parse_extended(lines: &[&str]) -> Vec<PlaylistEntry> {
    let mut entries = Vec::new();
    let mut pending_title: Option<String> = None;
    let mut pending_duration: Option<f64> = None;

    for line in lines {
        let line = line.trim();

        if line.is_empty() || line == "#EXTM3U" {
            continue;
        }

        if let Some(extinf) = line.strip_prefix("#EXTINF:") {
            // Parse #EXTINF:duration,title
            let (duration, title) = parse_extinf(extinf);
            pending_duration = duration;
            pending_title = title;
        } else if line.starts_with('#') {
            // Skip other directives
            continue;
        } else {
            // This is a URL line
            entries.push(PlaylistEntry {
                url: line.to_string(),
                title: pending_title.take(),
                duration_secs: pending_duration.take(),
            });
        }
    }

    entries
}

/// Parse the content after `#EXTINF:` — e.g. "123,Song Title" or "-1,Title" or just "123".
fn parse_extinf(extinf: &str) -> (Option<f64>, Option<String>) {
    let extinf = extinf.trim();

    if let Some(comma_pos) = extinf.find(',') {
        let duration_str = extinf[..comma_pos].trim();
        let title_str = extinf[comma_pos + 1..].trim();

        let duration = duration_str.parse::<f64>().ok().filter(|d| *d >= 0.0);
        let title = if title_str.is_empty() {
            None
        } else {
            Some(title_str.to_string())
        };

        (duration, title)
    } else {
        // No comma — just duration
        let duration = extinf.parse::<f64>().ok().filter(|d| *d >= 0.0);
        (duration, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_content() {
        assert!(parse("").is_err());
        assert!(parse("   \n  \n  ").is_err());
    }

    #[test]
    fn test_simple_m3u() {
        let content = "http://example.com/video1.mp4\nhttp://example.com/video2.mkv\n";
        let playlist = parse(content).unwrap();
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(playlist.entries[0].url, "http://example.com/video1.mp4");
        assert_eq!(playlist.entries[1].url, "http://example.com/video2.mkv");
        assert!(playlist.entries[0].title.is_none());
        assert!(playlist.entries[0].duration_secs.is_none());
    }

    #[test]
    fn test_simple_m3u_with_comments() {
        let content = "# This is a comment\nhttp://example.com/video1.mp4\n# Another comment\nhttp://example.com/video2.mp4\n";
        let playlist = parse(content).unwrap();
        assert_eq!(playlist.entries.len(), 2);
    }

    #[test]
    fn test_extended_m3u() {
        let content = "#EXTM3U\n#EXTINF:120,First Video\nhttp://example.com/video1.mp4\n#EXTINF:300,Second Video\nhttp://example.com/video2.mp4\n";
        let playlist = parse(content).unwrap();
        assert_eq!(playlist.entries.len(), 2);

        assert_eq!(playlist.entries[0].url, "http://example.com/video1.mp4");
        assert_eq!(playlist.entries[0].title.as_deref(), Some("First Video"));
        assert_eq!(playlist.entries[0].duration_secs, Some(120.0));

        assert_eq!(playlist.entries[1].url, "http://example.com/video2.mp4");
        assert_eq!(playlist.entries[1].title.as_deref(), Some("Second Video"));
        assert_eq!(playlist.entries[1].duration_secs, Some(300.0));
    }

    #[test]
    fn test_extended_m3u_negative_duration() {
        let content = "#EXTM3U\n#EXTINF:-1,Live Stream\nhttp://example.com/live\n";
        let playlist = parse(content).unwrap();
        assert_eq!(playlist.entries.len(), 1);
        assert!(playlist.entries[0].duration_secs.is_none()); // -1 is filtered out
        assert_eq!(playlist.entries[0].title.as_deref(), Some("Live Stream"));
    }

    #[test]
    fn test_extended_m3u_no_title() {
        let content = "#EXTM3U\n#EXTINF:60\nhttp://example.com/video.mp4\n";
        let playlist = parse(content).unwrap();
        assert_eq!(playlist.entries.len(), 1);
        assert_eq!(playlist.entries[0].duration_secs, Some(60.0));
        assert!(playlist.entries[0].title.is_none());
    }

    #[test]
    fn test_extended_m3u_url_without_extinf() {
        let content = "#EXTM3U\nhttp://example.com/video1.mp4\n#EXTINF:30,Titled\nhttp://example.com/video2.mp4\n";
        let playlist = parse(content).unwrap();
        assert_eq!(playlist.entries.len(), 2);
        assert!(playlist.entries[0].title.is_none());
        assert_eq!(playlist.entries[1].title.as_deref(), Some("Titled"));
    }

    #[test]
    fn test_no_valid_entries() {
        let content = "# just comments\n# nothing else\n";
        assert!(parse(content).is_err());
    }

    #[test]
    fn test_whitespace_handling() {
        let content = "  http://example.com/video1.mp4  \n  \n  http://example.com/video2.mp4  \n";
        let playlist = parse(content).unwrap();
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(playlist.entries[0].url, "http://example.com/video1.mp4");
    }
}
