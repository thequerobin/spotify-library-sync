//! Human-readable rate limits. Deadlines remain UTC; only presentation uses local time.
use chrono::{DateTime, Local, TimeZone, Utc};
use std::fmt;

/// A persisted cooldown blocked the operation before any request was sent.
#[derive(Debug)]
pub struct CooldownActive {
    pub retry_at: DateTime<Utc>,
}

impl fmt::Display for CooldownActive {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}\nNenhuma requisição foi enviada.",
            local_notice(Some(self.retry_at))
        )
    }
}

impl std::error::Error for CooldownActive {}

pub(crate) fn local_notice(deadline: Option<DateTime<Utc>>) -> String {
    notice(deadline, Utc::now(), &Local)
}

// Accept the clock and timezone explicitly so tests never depend on the host timezone.
fn notice<Tz: TimeZone>(
    deadline: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    timezone: &Tz,
) -> String {
    let Some(deadline) = deadline else {
        return "Limite de requisições informado pelo Spotify.\nDisponível em: prazo não informado.\nTempo restante: não informado.".into();
    };
    let heading = if deadline > now {
        "Limite de requisições ativo."
    } else {
        "Prazo do limite de requisições encerrado."
    };
    let local = deadline.with_timezone(timezone).fixed_offset();
    format!(
        "{heading}\nDisponível em: {} às {} (UTC{})\nTempo restante: {}.",
        local.format("%d/%m/%Y"),
        local.format("%H:%M:%S"),
        local.format("%:z"),
        remaining(deadline, now)
    )
}

fn remaining(deadline: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let duration = deadline.signed_duration_since(now);
    // Round positive fractions up: an active cooldown must never say zero seconds.
    let seconds = if duration > chrono::Duration::zero() {
        duration.num_seconds() + i64::from(duration.subsec_nanos() > 0)
    } else {
        0
    };
    let units = [
        (seconds / 86400, "dia", "dias"),
        (seconds % 86400 / 3600, "hora", "horas"),
        (seconds % 3600 / 60, "minuto", "minutos"),
        (seconds % 60, "segundo", "segundos"),
    ];
    let mut parts: Vec<String> = units
        .into_iter()
        .filter(|(n, _, _)| *n > 0)
        .map(|(n, singular, plural)| format!("{n} {}", if n == 1 { singular } else { plural }))
        .collect();
    match parts.pop() {
        None => "0 segundos".into(),
        Some(last) if parts.is_empty() => last,
        Some(last) => format!("{} e {last}", parts.join(", ")),
    }
}

/// Use the same presentation at every CLI/menu error boundary, including wrapped errors.
pub fn error_message(error: &anyhow::Error) -> String {
    if let Some(cooldown) = error.downcast_ref::<CooldownActive>() {
        cooldown.to_string()
    } else if let Some(paused) = error.downcast_ref::<crate::spotify::Paused>() {
        paused.to_string()
    } else {
        format!("Erro: {error:#}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, FixedOffset};

    fn utc(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn formats_example_without_fractional_seconds() {
        let deadline = utc("2026-11-05T16:30:11.212575700Z");
        let now = deadline - Duration::seconds(754);
        let text = notice(Some(deadline), now, &FixedOffset::west_opt(10800).unwrap());
        assert_eq!(
            text,
            "Limite de requisições ativo.\nDisponível em: 05/11/2026 às 13:30:11 (UTC-03:00)\nTempo restante: 12 minutos e 34 segundos."
        );
        assert_eq!(deadline, utc("2026-11-05T16:30:11.212575700Z"));
    }

    #[test]
    fn offsets_can_change_the_day_month_and_year() {
        let now = utc("2026-12-31T23:59:00Z");
        let deadline = utc("2027-01-01T00:01:00Z");
        let west = notice(Some(deadline), now, &FixedOffset::west_opt(10800).unwrap());
        assert!(west.contains("31/12/2026 às 21:01:00 (UTC-03:00)"));
        let east = notice(Some(deadline), now, &FixedOffset::east_opt(20700).unwrap());
        assert!(east.contains("01/01/2027 às 05:46:00 (UTC+05:45)"));
        assert!(east.ends_with("Tempo restante: 2 minutos."));
        assert!(notice(Some(deadline), now, &Utc).contains("(UTC+00:00)"));
    }

    #[test]
    fn expired_equal_and_subsecond_deadlines_are_clear() {
        let now = utc("2026-11-05T16:30:11Z");
        for deadline in [now, now - Duration::seconds(1)] {
            let text = notice(Some(deadline), now, &Utc);
            assert!(text.starts_with("Prazo do limite de requisições encerrado."));
            assert!(text.ends_with("Tempo restante: 0 segundos."));
        }
        assert_eq!(remaining(now + Duration::nanoseconds(1), now), "1 segundo");
        assert_eq!(remaining(now + Duration::milliseconds(1001), now), "2 segundos");
    }

    #[test]
    fn remaining_is_recomputed_and_handles_long_durations_and_plurals() {
        let now = utc("2026-11-05T16:30:11Z");
        let deadline = now + Duration::seconds(90061);
        assert_eq!(
            remaining(deadline, now),
            "1 dia, 1 hora, 1 minuto e 1 segundo"
        );
        assert_eq!(remaining(deadline, now + Duration::seconds(90001)), "1 minuto");
        assert_eq!(remaining(now + Duration::days(2), now), "2 dias");
        assert_eq!(remaining(now + Duration::hours(2), now), "2 horas");
    }

    #[test]
    fn system_timezone_and_wrapped_errors_use_shared_presentation() {
        let deadline = Utc::now() + Duration::hours(1);
        let local = deadline.with_timezone(&Local);
        let error = anyhow::Error::new(CooldownActive { retry_at: deadline })
            .context("outer context");
        let text = error_message(&error);
        assert!(text.contains(&local.format("%d/%m/%Y às %H:%M:%S (UTC%:z)").to_string()));
        assert!(text.ends_with("Nenhuma requisição foi enviada."));
        assert!(!text.contains('\u{1b}'));
        let paused = anyhow::Error::new(crate::spotify::Paused {
            message: "HTTP 429: execução pausada.".into(),
            retry_at: Some(deadline),
        })
        .context(crate::spotify::WriteRejected);
        let text = error_message(&paused);
        assert!(text.starts_with("Limite de requisições ativo."));
        assert!(text.contains("HTTP 429"));
        assert!(!text.contains("Nenhuma requisição foi enviada"));
        assert!(notice(None, Utc::now(), &Local).contains("prazo não informado"));
        assert_eq!(error_message(&anyhow::anyhow!("other")), "Erro: other");
    }
}
