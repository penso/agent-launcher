use chrono::{DateTime, Local, Utc};

pub(crate) fn age_label(created_at: Option<DateTime<Utc>>) -> String {
    let Some(created_at) = created_at else {
        return "—".to_owned();
    };
    let age = Utc::now().signed_duration_since(created_at);
    if age.num_days() > 0 {
        format!("{}d", age.num_days())
    } else if age.num_hours() > 0 {
        format!("{}h", age.num_hours())
    } else if age.num_minutes() > 0 {
        format!("{}m", age.num_minutes())
    } else {
        "now".to_owned()
    }
}

pub(crate) fn timestamp_label(timestamp: DateTime<Utc>) -> String {
    let local: DateTime<Local> = timestamp.into();
    local.format("%Y-%m-%d %H:%M:%S").to_string()
}

pub(crate) fn truncate(value: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let mut output = value
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    output.push('…');
    output
}

pub(crate) fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_by_characters() {
        assert_eq!(truncate("agent", 8), "agent");
        assert_eq!(truncate("launch", 4), "lau…");
        assert_eq!(truncate("x", 0), "");
    }
}
