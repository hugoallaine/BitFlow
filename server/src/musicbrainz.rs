//! Albums : identifiant MusicBrainz, année et pochette (Cover Art Archive) quand les tags ne suffisent pas.
//! Règles MusicBrainz : 1 requête/s maximum et User-Agent identifiable.
use std::time::Duration;

use serde_json::Value;

use crate::App;

const PAUSE: Duration = Duration::from_millis(1100);

pub async fn enrich(app: &App) -> anyhow::Result<()> {
    let todo: Vec<(i64, String, Option<String>, Option<String>, bool)> = sqlx::query_as(
        "SELECT id, title, artist, mbid, poster IS NOT NULL FROM items WHERE kind = 'album' AND matched = 0",
    )
    .fetch_all(&app.db)
    .await?;
    for (id, title, artist, mbid, has_cover) in todo {
        if let Err(e) = album(app, id, &title, artist.as_deref(), mbid, has_cover).await {
            tracing::warn!("MusicBrainz « {title} » : {e:#}");
        }
    }
    propagate_covers(app).await
}

async fn album(app: &App, id: i64, title: &str, artist: Option<&str>, mbid: Option<String>, has_cover: bool) -> anyhow::Result<()> {
    // Identifiant déjà dans les tags (Picard, beets…) : pas de recherche.
    let (release, group, year) = match mbid {
        Some(m) => (Some(m), None, None),
        None => {
            let clean = |s: &str| s.replace(['"', '\\'], "");
            let mut q = format!("release:\"{}\"", clean(title));
            if let Some(a) = artist.filter(|a| *a != crate::scan::VARIOUS) {
                q += &format!(" AND artist:\"{}\"", clean(a));
            }
            let res: Value = search(app, &q).await?;
            let r = &res["releases"][0];
            // Score MusicBrainz 0-100 : en dessous de 90, trop de faux positifs.
            if r["score"].as_i64().unwrap_or(0) >= 90 {
                let year = r["date"].as_str().and_then(|d| d.get(..4)).and_then(|y| y.parse::<i64>().ok());
                (r["id"].as_str().map(str::to_owned), r["release-group"]["id"].as_str().map(str::to_owned), year)
            } else {
                (None, None, None)
            }
        }
    };
    let cover = match (&release, has_cover) {
        (Some(rel), false) => cover_art(app, id, rel, group.as_deref()).await?,
        _ => None,
    };
    sqlx::query("UPDATE items SET mbid = COALESCE(mbid, ?), year = COALESCE(year, ?), poster = COALESCE(poster, ?), matched = ? WHERE id = ?")
        .bind(&release)
        .bind(year)
        .bind(cover)
        .bind(if release.is_some() { 1 } else { 2 })
        .bind(id)
        .execute(&app.db)
        .await?;
    Ok(())
}

/// Recherche de parution. Pause avant chaque requête (jamais plus d'une par seconde) ;
/// 503 = serveur MusicBrainz surchargé ou limite atteinte : nouvel essai après 2, 4 puis 6 s.
async fn search(app: &App, q: &str) -> anyhow::Result<Value> {
    for attempt in 1..=4u64 {
        tokio::time::sleep(PAUSE).await;
        let res = app.http.get("https://musicbrainz.org/ws/2/release/").query(&[("query", q), ("fmt", "json"), ("limit", "1")]).send().await?;
        if res.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE && attempt < 4 {
            tokio::time::sleep(Duration::from_secs(2 * attempt)).await;
            continue;
        }
        return Ok(res.error_for_status()?.json().await?);
    }
    unreachable!("la dernière tentative retourne toujours")
}

/// Pochette 500 px du groupe de parutions (n'importe quelle édition), sinon de la parution exacte.
async fn cover_art(app: &App, album: i64, release: &str, group: Option<&str>) -> anyhow::Result<Option<String>> {
    let urls = group
        .map(|g| format!("https://coverartarchive.org/release-group/{g}/front-500"))
        .into_iter()
        .chain([format!("https://coverartarchive.org/release/{release}/front-500")]);
    for url in urls {
        // Cover Art Archive redirige vers archive.org : client qui suit les redirections.
        let res = app.http_follow.get(&url).send().await?;
        if res.status().is_success() {
            let png = res.headers().get("content-type").is_some_and(|v| v.as_bytes().ends_with(b"png"));
            return Ok(Some(save_cover(app, album, &res.bytes().await?, if png { "png" } else { "jpg" }).await?));
        }
    }
    Ok(None)
}

/// Écrit `/data/img/cover/c{album}.{ext}` et renvoie le nom à stocker dans `poster`.
pub async fn save_cover(app: &App, album: i64, bytes: &[u8], ext: &str) -> anyhow::Result<String> {
    let dir = app.cfg.data.join("img").join("cover");
    tokio::fs::create_dir_all(&dir).await?;
    let name = format!("c{album}.{ext}");
    tokio::fs::write(dir.join(&name), bytes).await?;
    Ok(name)
}

/// Les pistes et artistes affichent la pochette de leur album.
pub async fn propagate_covers(app: &App) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE items SET poster = (SELECT a.poster FROM items a WHERE a.id = items.parent_id)
         WHERE kind = 'track' AND poster IS NULL",
    )
    .execute(&app.db)
    .await?;
    sqlx::query(
        "UPDATE items SET poster = (SELECT a.poster FROM items a WHERE a.parent_id = items.id AND a.poster IS NOT NULL
         ORDER BY a.year DESC LIMIT 1) WHERE kind = 'artist' AND poster IS NULL",
    )
    .execute(&app.db)
    .await?;
    Ok(())
}
