use std::collections::BTreeMap;

pub const AUTHOR_HALF_LIFE_DAYS: u32 = 180;
pub const AUTHOR_HISTORY_DAYS: u32 = 730;

pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub(crate) fn risk_author_concentration(
    db: &codesage_storage::Database,
    path: &str,
    now: i64,
    anchor_name: &str,
) -> anyhow::Result<(Option<codesage_protocol::AuthorConcentration>, String)> {
    if !db.git_authors_complete()? {
        return Ok((None, "author concentration unavailable: run codesage git-index --full to populate author history".into()));
    }
    let events = db.git_author_events(path)?;
    if events.is_empty() {
        return Ok((
            None,
            "author concentration unavailable: no indexed author history for this path".into(),
        ));
    }
    let cutoff = now.saturating_sub(i64::from(AUTHOR_HISTORY_DAYS) * 86_400);
    if events
        .iter()
        .any(|(author, timestamp)| author.is_empty() && *timestamp >= cutoff)
    {
        return Ok((
            None,
            "author concentration unavailable: commit author identities are missing".into(),
        ));
    }
    let Some(value) = author_concentration(
        events.iter().map(|(author, time)| (author.as_str(), *time)),
        now,
    ) else {
        return Ok((
            None,
            format!(
                "author concentration unavailable: no qualifying commits in the {AUTHOR_HISTORY_DAYS}-day window anchored at {anchor_name} (Unix timestamp {now})"
            ),
        ));
    };
    let note = format!(
        "author concentration: {} identities, largest {:.0}% of weighted commits, bus factor {} (identities covering at least 50%; 180-day half-life, 730-day history; informational only)",
        value.author_count,
        value.dominant_share * 100.0,
        value.bus_factor,
    );
    Ok((
        Some(codesage_protocol::AuthorConcentration {
            author_count: value.author_count,
            dominant_share: value.dominant_share,
            effective_authors: value.effective_authors,
            bus_factor: value.bus_factor,
            half_life_days: AUTHOR_HALF_LIFE_DAYS,
            history_days: AUTHOR_HISTORY_DAYS,
            as_of: now,
        }),
        note,
    ))
}

#[derive(Debug, Clone, PartialEq)]
pub struct AuthorConcentration {
    pub author_count: usize,
    pub dominant_share: f64,
    pub effective_authors: f64,
    pub bus_factor: usize,
}

pub fn normalized_author(email: &str, name: &str) -> Option<String> {
    let email = email.trim().to_lowercase();
    if !email.is_empty() {
        return Some(format!("email:{email}"));
    }
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    (!name.is_empty()).then(|| format!("name:{}", name.to_lowercase()))
}

pub fn author_concentration<'a>(
    contributions: impl IntoIterator<Item = (&'a str, i64)>,
    now: i64,
) -> Option<AuthorConcentration> {
    let cutoff = now.saturating_sub(i64::from(AUTHOR_HISTORY_DAYS) * 86_400);
    let mut authors = BTreeMap::<&str, f64>::new();
    for (author, timestamp) in contributions {
        if author.is_empty() || timestamp < cutoff {
            continue;
        }
        let age = now.saturating_sub(timestamp).max(0) as f64;
        let weight = (-age / (f64::from(AUTHOR_HALF_LIFE_DAYS) * 86_400.0)).exp2();
        *authors.entry(author).or_default() += weight;
    }
    let mut weights: Vec<f64> = authors.into_values().collect();
    if weights.is_empty() {
        return None;
    }
    weights.sort_by(|a, b| b.total_cmp(a));
    let total: f64 = weights.iter().sum();
    let shares: Vec<f64> = weights.iter().map(|weight| weight / total).collect();
    let mut cumulative = 0.0;
    let mut bus_factor = 0;
    for weight in &weights {
        bus_factor += 1;
        cumulative += weight;
        if cumulative >= total * 0.5 {
            break;
        }
    }
    Some(AuthorConcentration {
        author_count: shares.len(),
        dominant_share: shares[0],
        effective_authors: 1.0 / shares.iter().map(|share| share * share).sum::<f64>(),
        bus_factor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;

    #[test]
    fn half_life_is_six_months_and_concentration_counts_author_identities() {
        let now = 2_000_000_000;
        let report = author_concentration([("old", now - 180 * DAY), ("new", now)], now).unwrap();
        assert_eq!(report.author_count, 2);
        assert!((report.dominant_share - 2.0 / 3.0).abs() < 1e-12);
        assert!((report.effective_authors - 1.8).abs() < 1e-12);
        assert_eq!(report.bus_factor, 1);
        let balanced = author_concentration([("a", now), ("b", now), ("c", now)], now).unwrap();
        assert_eq!(balanced.bus_factor, 2);
        assert!((balanced.effective_authors - 3.0).abs() < 1e-12);
        let identities: Vec<String> = (0..14).map(|i| i.to_string()).collect();
        let balanced =
            author_concentration(identities.iter().map(|name| (name.as_str(), now)), now).unwrap();
        assert_eq!(balanced.bus_factor, 7);
    }

    #[test]
    fn missing_identity_and_aged_history_do_not_fabricate_authors() {
        assert_eq!(
            normalized_author(" User@Example.COM ", "Alias"),
            Some("email:user@example.com".into())
        );
        assert_eq!(
            normalized_author("", "  Some   Name "),
            Some("name:some name".into())
        );
        assert_eq!(normalized_author(" ", " \t "), None);
        let now = 2_000_000_000;
        assert!(author_concentration([("old", now - 731 * DAY)], now).is_none());
        assert!(author_concentration([], now).is_none());
        assert!(author_concentration([("", now)], now).is_none());
        assert_eq!(
            author_concentration([("future", now + DAY)], now)
                .unwrap()
                .dominant_share,
            1.0
        );
    }
}
