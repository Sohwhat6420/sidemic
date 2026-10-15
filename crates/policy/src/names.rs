//! Friendly names for well-known app ids, for decision text.

/// "com.zhiliaoapp.musically" -> "TikTok", "Chrome" -> "Chrome", anything unknown -> the raw id.
pub fn friendly_name(id: &str) -> String {
    let l = id.to_ascii_lowercase();
    let known: &[(&[&str], &str)] = &[
        (&["musically", "ugc.trill", "tiktok"], "TikTok"),
        (&["spotify"], "Spotify"),
        (&["youtube"], "YouTube"),
        (&["chrome"], "Chrome"),
        (&["msedge", "microsoftedge"], "Edge"),
        (&["discord"], "Discord"),
        (&["vlc"], "VLC"),
    ];
    for (needles, name) in known {
        if needles.iter().any(|n| l.contains(n)) {
            return (*name).to_string();
        }
    }
    id.to_string()
}

/// "2 minutes", "1 second", "3 hours".
pub fn human_duration(ms: u64) -> String {
    let s = ms / 1000;
    let (n, unit) = if s < 60 {
        (s, "second")
    } else if s < 3600 {
        (s / 60, "minute")
    } else {
        (s / 3600, "hour")
    };
    if n == 1 {
        format!("1 {unit}")
    } else {
        format!("{n} {unit}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_durations() {
        let cases = [
            ("com.zhiliaoapp.musically", "TikTok"),
            ("com.spotify.music", "Spotify"),
            ("Spotify.exe", "Spotify"),
            ("com.google.android.youtube", "YouTube"),
            ("Chrome", "Chrome"),
            ("discord.exe", "Discord"),
            ("org.example.radio", "org.example.radio"),
        ];
        for (id, want) in cases {
            assert_eq!(friendly_name(id), want, "{id}");
        }
        let durations = [(0, "0 seconds"), (1_000, "1 second"), (2_500, "2 seconds"), (120_000, "2 minutes"), (3_600_000, "1 hour")];
        for (ms, want) in durations {
            assert_eq!(human_duration(ms), want, "{ms}");
        }
    }
}
