use std::path::Path;

#[derive(Debug, PartialEq, Eq)]
pub struct FilenameInfo {
    pub title: String,
    pub year: Option<u16>,
    pub season: Option<u16>,
    pub episode: Option<u16>,
    pub resolution: Option<String>,
    pub codec: Option<String>,
    pub ext: String,
}

fn number(s: &str) -> Option<(u16, &str)> {
    let end = s.bytes().take_while(u8::is_ascii_digit).count();
    Some((s[..end].parse().ok()?, &s[end..]))
}

fn chinese_number(s: &str) -> Option<u16> {
    if s.is_empty() {
        return None;
    }
    if let Ok(n) = s.parse() {
        return Some(n);
    }
    let value = |c| match c {
        '两' => Some(2u16),
        '〇' => Some(0),
        c => "零一二三四五六七八九"
            .chars()
            .position(|n| n == c)
            .map(|n| n as u16),
    };
    if !s.contains(['十', '百', '千']) {
        return s
            .chars()
            .try_fold(0u16, |n, c| n.checked_mul(10)?.checked_add(value(c)?));
    }
    let (mut total, mut digit) = (0u16, 0u16);
    for c in s.chars() {
        match c {
            '十' | '百' | '千' => {
                let unit = match c {
                    '十' => 10,
                    '百' => 100,
                    _ => 1000,
                };
                total = total.checked_add(digit.max(1).checked_mul(unit)?)?;
                digit = 0;
            }
            c => digit = value(c)?,
        }
    }
    total.checked_add(digit)
}

fn season_folder(path: &Path) -> Option<u16> {
    let folder = path.file_name()?.to_str()?.to_ascii_lowercase();
    folder
        .strip_prefix("season")
        .or_else(|| folder.strip_prefix('s'))?
        .trim_start_matches([' ', '_', '.', '-'])
        .parse()
        .ok()
}

fn clean(raw: &str) -> String {
    raw.trim_matches(|c: char| c.is_whitespace() || ".-_[]()【】（）".contains(c))
        .replace(['.', '_'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn detect_filename(path: &Path) -> FilenameInfo {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let lower = stem.to_ascii_lowercase(); // ASCII folding preserves UTF-8 byte offsets.
    let mut info = FilenameInfo {
        title: String::new(),
        year: None,
        season: None,
        episode: None,
        resolution: None,
        codec: None,
        ext: path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase(),
    };
    let mut end = stem.len();
    let mut previous_ascii_word = false;
    for (i, c) in lower.char_indices() {
        let boundary = !previous_ascii_word;
        previous_ascii_word = c.is_ascii_alphanumeric();
        let rest = &lower[i..];
        let marker = if let Some(rest) = rest.strip_prefix('第') {
            rest.split_once('集')
                .and_then(|(n, _)| chinese_number(n))
                .map(|ep| (None, ep))
        } else if boundary && c == 's' {
            rest[1..].split_once('e').and_then(|(s, e)| {
                let season = s.trim_end_matches([' ', '.', '_', '-']).parse().ok()?;
                let (episode, _) = number(e)?;
                Some((Some(season), episode))
            })
        } else if boundary && rest.starts_with("ep") {
            number(rest[2..].trim_start_matches([' ', '.', '_', '-'])).map(|(ep, _)| (None, ep))
        } else {
            None
        };
        if let Some((season, episode)) = marker
            && info.episode.is_none()
        {
            info.season = season;
            info.episode = Some(episode);
            end = end.min(i);
        }
        if !boundary {
            continue;
        }
        if c.is_ascii_digit()
            && let Some((year, tail)) = number(rest)
            && rest.len() - tail.len() == 4
            && (1888..=2100).contains(&year)
            && !tail.starts_with(|v: char| v.is_ascii_alphanumeric())
            && i > 0
            && !clean(&stem[..i]).is_empty()
            && info.year.is_none()
        {
            info.year = Some(year);
            end = end.min(i);
        }
        // Release tags can appear in any order, including bracketed scene names.
        for (aliases, value, resolution) in [
            (&["4320p", "8k"][..], "4320p", true),
            (&["2160p", "4k", "uhd"], "2160p", true),
            (&["1080p"], "1080p", true),
            (&["1080i"], "1080i", true),
            (&["720p"], "720p", true),
            (&["576p"], "576p", true),
            (&["480p"], "480p", true),
            (&["h.265", "h265", "x265", "hevc"], "HEVC", false),
            (&["h.264", "h264", "x264", "avc"], "H.264", false),
            (&["av1"], "AV1", false),
            (&["xvid"], "XviD", false),
        ] {
            if aliases.iter().any(|tag| {
                rest.strip_prefix(tag)
                    .is_some_and(|tail| !tail.starts_with(|v: char| v.is_ascii_alphanumeric()))
            }) {
                let slot = if resolution {
                    &mut info.resolution
                } else {
                    &mut info.codec
                };
                slot.get_or_insert_with(|| value.to_owned());
                end = end.min(i);
            }
        }
        if [
            "web-dl", "webrip", "web.dl", "bluray", "blu-ray", "remux", "hdtv",
        ]
        .iter()
        .any(|tag| {
            rest.strip_prefix(tag)
                .is_some_and(|tail| !tail.starts_with(|v: char| v.is_ascii_alphanumeric()))
        }) {
            end = end.min(i);
        }
    }
    let mut raw = stem[..end].trim();
    while let Some((_, tail)) = raw.strip_prefix('[').and_then(|s| s.split_once(']')) {
        if clean(tail).is_empty() {
            break;
        }
        raw = tail.trim_start_matches([' ', '.', '_', '-']);
    }
    info.title = clean(raw);
    if let Some(parent) = path.parent() {
        let folder_season = season_folder(parent);
        info.season = info.season.or(folder_season);
        if info.title.is_empty() || (folder_season.is_some() && info.episode.is_none()) {
            let series_dir = if folder_season.is_some() {
                parent.parent()
            } else {
                Some(parent)
            };
            if let Some(name) = series_dir
                .and_then(Path::file_name)
                .and_then(|s| s.to_str())
            {
                info.title = clean(name);
            }
        }
    }
    if info.episode.is_some() {
        info.season.get_or_insert(1);
    }
    if info.title.is_empty() {
        info.title = if info.season.is_some() {
            "Unsorted series".into()
        } else {
            clean(stem)
        };
    }
    info
}
