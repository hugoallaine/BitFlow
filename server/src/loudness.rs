//! Volume perçu (EBU R128) des pistes et des albums, pour la normalisation côté lecteur.
//! Tags ReplayGain lus au scan si présents ; sinon mesure ffmpeg en arrière-plan.
//! Pistes : matched 0 = à mesurer, 1 = mesurée, 2 = échec.
use futures_util::{StreamExt, stream};
use tokio::process::Command;

use crate::App;

pub async fn analyze(app: &App) -> anyhow::Result<()> {
    let todo: Vec<(i64, String)> = sqlx::query_as("SELECT id, path FROM items WHERE kind = 'track' AND matched = 0").fetch_all(&app.db).await?;
    if !todo.is_empty() {
        tracing::info!("analyse du volume : {} pistes", todo.len());
    }
    // ~500x le temps réel par piste : 2 en parallèle laissent le processeur au streaming.
    stream::iter(todo)
        .for_each_concurrent(2, |(id, path)| async move {
            let res = measure(&path).await;
            let (loudness, peak, state) = match res {
                Some((l, p)) => (Some(l), Some(p), 1),
                None => (None, None, 2),
            };
            if let Err(e) = sqlx::query("UPDATE items SET loudness = ?, peak = ?, matched = ? WHERE id = ?")
                .bind(loudness).bind(peak).bind(state).bind(id).execute(&app.db).await
            {
                tracing::warn!("volume {path} : {e}");
            }
        })
        .await;
    albums(app).await
}

/// Volume d'album = moyenne énergétique des pistes pondérée par leur durée ; pic = pic max.
async fn albums(app: &App) -> anyhow::Result<()> {
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT DISTINCT parent_id FROM items WHERE kind = 'track' AND album_loudness IS NULL
         AND parent_id NOT IN (SELECT parent_id FROM items WHERE kind = 'track' AND matched = 0)",
    )
    .fetch_all(&app.db)
    .await?;
    for album in ids {
        let rows: Vec<(f64, f64, f64)> = sqlx::query_as(
            "SELECT loudness, COALESCE(peak, 1.0), COALESCE(NULLIF(duration, 0), 1.0) FROM items WHERE parent_id = ? AND loudness IS NOT NULL",
        )
        .bind(album)
        .fetch_all(&app.db)
        .await?;
        let Some((loudness, peak)) = album_loudness(&rows) else { continue };
        sqlx::query("UPDATE items SET album_loudness = ?, album_peak = ? WHERE parent_id = ? AND album_loudness IS NULL")
            .bind(loudness).bind(peak).bind(album).execute(&app.db).await?;
    }
    Ok(())
}

fn album_loudness(rows: &[(f64, f64, f64)]) -> Option<(f64, f64)> {
    let total: f64 = rows.iter().map(|r| r.2).sum();
    if rows.is_empty() || total <= 0.0 {
        return None;
    }
    let energy: f64 = rows.iter().map(|(l, _, d)| d * 10f64.powf(l / 10.0)).sum::<f64>() / total;
    Some((10.0 * energy.log10(), rows.iter().map(|r| r.1).fold(0.0, f64::max)))
}

/// (volume intégré en LUFS, pic vrai linéaire).
async fn measure(path: &str) -> Option<(f64, f64)> {
    let out = Command::new("ffmpeg")
        .args(["-nostats", "-hide_banner", "-i", path, "-map", "0:a:0", "-af", "ebur128=peak=true", "-f", "null", "-"])
        .output()
        .await
        .ok()?;
    parse_ebur128(&String::from_utf8_lossy(&out.stderr))
}

fn parse_ebur128(log: &str) -> Option<(f64, f64)> {
    let summary = log.rsplit("Summary:").next()?;
    let value = |label: &str| -> Option<f64> {
        summary.lines().find_map(|l| l.trim().strip_prefix(label)?.split_whitespace().next()?.parse().ok())
    };
    let loudness = value("I:")?;
    // Silence complet : -70 LUFS, inutilisable pour normaliser.
    (loudness > -70.0).then(|| (loudness, 10f64.powf(value("Peak:").unwrap_or(0.0) / 20.0)))
}

/// ReplayGain 2.0 : gain = -18 LUFS - volume. Tag « -6.50 dB » -> volume -11.5 LUFS.
pub fn loudness_from_gain(tag: &str) -> Option<f64> {
    let gain: f64 = tag.trim().split_whitespace().next()?.parse().ok()?;
    Some(-18.0 - gain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ebur128_summary() {
        let log = "[Parsed_ebur128_0] Summary:\n\n  Integrated loudness:\n    I:         -22.2 LUFS\n    Threshold: -32.2 LUFS\n\n  True peak:\n    Peak:      -18.5 dBFS\n";
        let (l, p) = parse_ebur128(log).unwrap();
        assert_eq!(l, -22.2);
        assert!((p - 0.1189).abs() < 1e-3);
        assert_eq!(parse_ebur128("Summary:\n    I:         -70.0 LUFS\n"), None);
    }

    #[test]
    fn album_and_tags() {
        // Deux pistes de même durée à -10 et -20 LUFS : la plus forte domine (moyenne d'énergie).
        let (l, p) = album_loudness(&[(-10.0, 0.9, 100.0), (-20.0, 0.5, 100.0)]).unwrap();
        assert!((l - -12.6).abs() < 0.1);
        assert_eq!(p, 0.9);
        assert_eq!(loudness_from_gain("-6.50 dB"), Some(-11.5));
    }
}
