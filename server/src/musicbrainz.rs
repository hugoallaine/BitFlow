//! Albums : identifiant MusicBrainz, année et pochette (Cover Art Archive) quand les tags ne suffisent pas.
//! Règles MusicBrainz : 1 requête/s maximum et User-Agent identifiable.
use std::time::Duration;

use serde_json::Value;

use crate::App;

const PAUSE: Duration = Duration::from_millis(1100);

pub async fn enrich(app: &App) -> anyhow::Result<()> {
    let todo: Vec<(i64, String, Option<String>, Option<String>, bool, Option<i64>)> = sqlx::query_as(
        "SELECT id, title, artist, mbid, poster IS NOT NULL, parent_id FROM items WHERE kind = 'album' AND matched = 0",
    )
    .fetch_all(&app.db)
    .await?;
    for (id, title, artist, mbid, has_cover, artist_id) in todo {
        if let Err(e) = album(app, id, &title, artist.as_deref(), mbid, has_cover, artist_id).await {
            tracing::warn!("MusicBrainz « {title} » : {e:#}");
        }
    }
    artists(app).await?;
    propagate_covers(app).await
}

const ARTIST_ART: &[&str] = &["artist.jpg", "artist-poster.jpg", "artist.png", "artist-poster.png"];

/// Image d'artiste : fichier `artist.jpg` / `artist-poster.jpg` dans le dossier de l'artiste (convention Plex),
/// sinon Fanart.tv (clé gratuite FANART_KEY, recherche par identifiant MusicBrainz).
async fn artists(app: &App) -> anyhow::Result<()> {
    let todo: Vec<(i64, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT a.id, a.mbid, (SELECT t.path FROM items al JOIN items t ON t.parent_id = al.id WHERE al.parent_id = a.id LIMIT 1)
         FROM items a WHERE a.kind = 'artist' AND a.matched = 0",
    )
    .fetch_all(&app.db)
    .await?;
    for (id, mbid, track) in todo {
        // Piste -> dossier album -> dossier artiste.
        let dir = track.as_deref().map(std::path::Path::new).and_then(|p| p.parent()?.parent());
        let local = dir.and_then(|d| {
            ARTIST_ART.iter().find_map(|f| Some((std::fs::read(d.join(f)).ok()?, if f.ends_with("png") { "png" } else { "jpg" })))
        });
        let image = match (local, &app.cfg.fanart_key, &mbid) {
            (Some(img), _, _) => Some(img),
            (None, Some(key), Some(mbid)) => fanart(app, key, mbid).await.unwrap_or_else(|e| {
                tracing::warn!("Fanart.tv {mbid} : {e:#}");
                None
            }),
            _ => None,
        };
        let state = match (&image, &app.cfg.fanart_key, &mbid) {
            (Some(_), _, _) => 1,
            (None, Some(_), Some(_)) => 2, // cherché, introuvable
            _ => 0,                         // sans clé ni identifiant : on revérifiera le dossier au prochain scan
        };
        let poster = match image {
            Some((bytes, ext)) => Some(save_image(app, format!("a{id}.{ext}"), &bytes).await?),
            None => None,
        };
        sqlx::query("UPDATE items SET poster = COALESCE(?, poster), matched = ? WHERE id = ?").bind(poster).bind(state).bind(id).execute(&app.db).await?;
    }
    Ok(())
}

async fn fanart(app: &App, key: &str, mbid: &str) -> anyhow::Result<Option<(Vec<u8>, &'static str)>> {
    let res = app.http.get(format!("https://webservice.fanart.tv/v3/music/{mbid}")).query(&[("api_key", key)]).send().await?;
    if res.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let v: Value = res.error_for_status()?.json().await?;
    let Some(url) = v["artistthumb"][0]["url"].as_str() else { return Ok(None) };
    let bytes = app.http_follow.get(url).send().await?.error_for_status()?.bytes().await?;
    Ok(Some((bytes.to_vec(), if url.ends_with(".png") { "png" } else { "jpg" })))
}

async fn album(app: &App, id: i64, title: &str, artist: Option<&str>, mbid: Option<String>, has_cover: bool, artist_id: Option<i64>) -> anyhow::Result<()> {
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
                // Identifiant MusicBrainz de l'artiste : nécessaire pour son image (Fanart.tv).
                if let (Some(a), Some(aid)) = (r["artist-credit"][0]["artist"]["id"].as_str(), artist_id)
                    && artist.is_some_and(|n| n != crate::scan::VARIOUS)
                {
                    sqlx::query("UPDATE items SET mbid = COALESCE(mbid, ?) WHERE id = ?").bind(a).bind(aid).execute(&app.db).await?;
                }
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
            return Ok(Some(save_image(app, format!("c{album}.{}", if png { "png" } else { "jpg" }), &res.bytes().await?).await?));
        }
    }
    Ok(None)
}

/// Écrit `/data/img/cover/{name}` (c12.jpg = album, a3.jpg = artiste) et renvoie le nom à stocker dans `poster`.
pub async fn save_image(app: &App, name: String, bytes: &[u8]) -> anyhow::Result<String> {
    let dir = app.cfg.data.join("img").join("cover");
    tokio::fs::create_dir_all(&dir).await?;
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
