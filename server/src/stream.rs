//! Lecture : directe (HTTP Range) si le navigateur sait lire le fichier, sinon HLS transcodé par ffmpeg.
use std::{path::PathBuf, process::Stdio, time::Duration};

use axum::{
    Json,
    body::Body,
    extract::{Path, Query, Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use bitflow_shared::{MediaInfo, Prefs, Track};
use serde::Deserialize;
use serde_json::Value;
use tokio::process::{Child, Command};
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::{App, R, auth::User, err};

const SEGMENT: u32 = 6;
const TEXT_SUBS: &[&str] = &["subrip", "ass", "ssa", "mov_text", "webvtt", "text"];

/// Transcodage en cours. Le remplacer ou le supprimer tue ffmpeg et efface ses segments.
pub struct Session {
    pub id: String,
    child: Child,
    dir: PathBuf,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn path_of(app: &App, id: i64) -> R<String> {
    match sqlx::query_scalar("SELECT path FROM items WHERE id = ? AND kind IN ('movie', 'episode', 'track')").bind(id).fetch_optional(&app.db).await? {
        Some(p) => Ok(p),
        None => err(StatusCode::NOT_FOUND, "média introuvable"),
    }
}

async fn probe(path: &str) -> R<Value> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams", path])
        .output()
        .await?;
    Ok(serde_json::from_slice(&out.stdout)?)
}

fn streams<'a>(p: &'a Value, kind: &'a str) -> impl Iterator<Item = &'a Value> {
    p["streams"].as_array().into_iter().flatten().filter(move |s| s["codec_type"] == kind)
}

fn label(s: &Value, i: usize) -> String {
    let t = &s["tags"];
    let parts: Vec<&str> = [t["title"].as_str(), t["language"].as_str(), s["codec_name"].as_str()].into_iter().flatten().collect();
    if parts.is_empty() { format!("Piste {}", i + 1) } else { parts.join(" · ") }
}

/// Code ISO 639-2 (MKV : "fra", "fre") ou 639-1 ("fr") -> 639-1. "und" et vide -> None.
pub fn norm_lang(code: &str) -> Option<String> {
    let c = code.trim().to_ascii_lowercase();
    let two = match c.as_str() {
        "" | "und" | "zxx" | "mul" => return None,
        "fra" | "fre" => "fr", "eng" => "en", "deu" | "ger" => "de", "ita" => "it", "spa" => "es",
        "por" => "pt", "nld" | "dut" => "nl", "jpn" => "ja", "kor" => "ko", "zho" | "chi" => "zh",
        "rus" => "ru", "ara" => "ar", "swe" => "sv", "nor" | "nob" => "no", "dan" => "da", "fin" => "fi",
        "pol" => "pl", "tur" => "tr", "ces" | "cze" => "cs", "ell" | "gre" => "el", "heb" => "he",
        "hin" => "hi", "hun" => "hu", "ron" | "rum" => "ro", "ukr" => "uk",
        c if c.len() == 2 && c.bytes().all(|b| b.is_ascii_lowercase()) => c,
        _ => return Some(c),
    };
    Some(two.into())
}

fn is_forced(text: &str) -> bool {
    let t = text.to_lowercase();
    t.contains("forced") || t.contains("forcé")
}

fn tracks(p: &Value, kind: &str) -> Vec<Track> {
    streams(p, kind)
        .enumerate()
        .map(|(index, s)| Track {
            index,
            label: label(s, index),
            default: s["disposition"]["default"] == 1,
            burn: kind == "subtitle" && !TEXT_SUBS.contains(&s["codec_name"].as_str().unwrap_or_default()),
            lang: s["tags"]["language"].as_str().and_then(norm_lang),
            forced: s["disposition"]["forced"] == 1 || s["tags"]["title"].as_str().is_some_and(is_forced),
        })
        .collect()
}

/// Pistes à activer d'office selon les préférences. `orig` = langue originale de l'œuvre (TMDB).
/// Audio : langue préférée, sinon VO (`orig`), sinon piste par défaut du fichier.
/// Une piste audio sans langue est supposée en VO ; sans VO connue, dans la langue de l'utilisateur.
pub fn choose(audio: &[Track], subs: &[Track], prefs: &Prefs, orig: Option<&str>) -> (usize, Option<usize>) {
    let with_lang = |l: Option<&str>| audio.iter().find(|t| l.is_some() && t.lang.as_deref() == l);
    let a = with_lang(prefs.audio_lang.as_deref())
        .or_else(|| with_lang(orig))
        .or_else(|| audio.iter().find(|t| t.default))
        .or(audio.first());
    let audio_lang = a.and_then(|t| t.lang.as_deref()).or(orig).unwrap_or(&prefs.sub_lang);
    let mine = |forced: bool| {
        let c: Vec<&Track> = subs.iter().filter(|t| t.lang.as_deref() == Some(prefs.sub_lang.as_str()) && t.forced == forced).collect();
        // Texte de préférence : évite l'incrustation, donc le transcodage.
        c.iter().find(|t| !t.burn).or(c.first()).map(|t| t.index)
    };
    let sub = match prefs.sub_mode.as_str() {
        "always" => mine(false).or_else(|| mine(true)),
        "foreign" if audio_lang != prefs.sub_lang => mine(false).or_else(|| mine(true)),
        "foreign" => mine(true),
        _ => None,
    };
    (a.map_or(0, |t| t.index), sub)
}

/// Numéro de piste des sous-titres externes : EXTERNAL + rang, pour les distinguer des pistes intégrées (0:s:N).
const EXTERNAL: usize = 1000;
const SUB_EXT: &[&str] = &["srt", "vtt", "ass", "ssa"];

/// Fichiers de sous-titres à côté de la vidéo : `Film.mkv` -> `Film.srt`, `Film.fr.srt`, `Film.en.forced.srt`.
/// Renvoie (fichier, piste avec libellé et langue tirés du nom), dans un ordre stable.
fn external_subs(video: &str) -> Vec<(PathBuf, Track)> {
    let video = std::path::Path::new(video);
    let (Some(dir), Some(stem)) = (video.parent(), video.file_stem().and_then(|s| s.to_str())) else { return vec![] };
    let mut subs: Vec<(PathBuf, String, Vec<String>)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let (base, ext) = name.rsplit_once('.')?;
            if !SUB_EXT.contains(&ext.to_ascii_lowercase().as_str()) {
                return None;
            }
            let tags = base.strip_prefix(stem)?;
            if !tags.is_empty() && !tags.starts_with('.') {
                return None; // "Film2.srt" n'appartient pas à "Film.mkv"
            }
            let tags: Vec<String> = tags.split('.').filter(|t| !t.is_empty()).map(str::to_owned).collect();
            let label = if tags.is_empty() { "Externe".into() } else { format!("{} · externe", tags.join(" · ")) };
            Some((e.path(), label, tags))
        })
        .collect();
    subs.sort();
    subs.into_iter()
        .enumerate()
        .map(|(i, (path, label, tags))| {
            let lang = tags.iter().find(|t| matches!(t.len(), 2 | 3)).and_then(|t| norm_lang(t));
            let forced = tags.iter().any(|t| is_forced(t));
            (path, Track { index: EXTERNAL + i, label, default: false, burn: false, lang, forced })
        })
        .collect()
}

fn codec<'a>(p: &'a Value, kind: &'a str) -> &'a str {
    streams(p, kind).next().and_then(|s| s["codec_name"].as_str()).unwrap_or_default()
}

/// Lisible nativement par Chrome, Firefox et Safari.
fn is_direct(p: &Value) -> bool {
    let fmt = p["format"]["format_name"].as_str().unwrap_or_default();
    (fmt.contains("mp4") || fmt.contains("webm"))
        && matches!(codec(p, "video"), "h264" | "vp9" | "av1")
        && matches!(codec(p, "audio"), "aac" | "mp3" | "opus" | "flac" | "")
}

pub async fn info(State(app): State<App>, user: User, Path(id): Path<i64>) -> R<Json<MediaInfo>> {
    let path = path_of(&app, id).await?;
    let p = probe(&path).await?;
    let audio = tracks(&p, "audio");
    let mut subs = tracks(&p, "subtitle");
    subs.extend(external_subs(&path).into_iter().map(|(_, t)| t));
    // Langue originale du film, ou de la série pour un épisode (épisode -> saison -> série).
    let orig: Option<String> = sqlx::query_scalar(
        "SELECT COALESCE(NULLIF(i.orig_lang, ''), NULLIF(sh.orig_lang, '')) FROM items i
         LEFT JOIN items se ON se.id = i.parent_id LEFT JOIN items sh ON sh.id = se.parent_id WHERE i.id = ?",
    )
    .bind(id)
    .fetch_one(&app.db)
    .await?;
    let orig = orig.as_deref().and_then(norm_lang);
    let (audio_pick, sub_pick) = choose(&audio, &subs, &crate::api::prefs_of(&app, user.id).await?, orig.as_deref());
    let next: Option<i64> = sqlx::query_scalar(
        "SELECT n.id FROM items e JOIN items s ON s.id = e.parent_id JOIN items n ON n.kind = 'episode'
         JOIN items ns ON ns.id = n.parent_id AND ns.parent_id = s.parent_id
         WHERE e.id = ? AND (n.season, n.episode) > (e.season, e.episode)
         ORDER BY n.season, n.episode LIMIT 1",
    )
    .bind(id)
    .fetch_optional(&app.db)
    .await?;
    Ok(Json(MediaInfo {
        duration: p["format"]["duration"].as_str().and_then(|d| d.parse().ok()).unwrap_or(0.0),
        direct: is_direct(&p),
        audio,
        subs,
        next,
        audio_pick,
        sub_pick,
    }))
}

/// Piste audio : fichier tel quel si le navigateur sait le lire (avec Range), sinon converti en MP3.
/// shortcut: le flux converti ne permet pas de se déplacer dans la piste (formats rares : WMA, APE, ALAC).
pub async fn audio(State(app): State<App>, _: User, Path(id): Path<i64>, req: Request) -> R<Response> {
    let path = path_of(&app, id).await?;
    let p = probe(&path).await?;
    if matches!(codec(&p, "audio"), "mp3" | "aac" | "flac" | "opus" | "vorbis" | "pcm_s16le" | "pcm_s24le") {
        return Ok(ServeFile::new(path).oneshot(req).await?.into_response());
    }
    let mut child = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i", &path, "-map", "0:a:0", "-c:a", "libmp3lame", "-b:a", "320k", "-f", "mp3", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child.stdout.take().expect("stdout");
    // Le processus vit avec le flux : client déconnecté -> flux abandonné -> ffmpeg tué.
    let stream = futures_util::StreamExt::map(tokio_util::io::ReaderStream::new(stdout), move |chunk| {
        let _ = &child;
        chunk
    });
    Ok(([(header::CONTENT_TYPE, "audio/mpeg")], Body::from_stream(stream)).into_response())
}

pub async fn direct(State(app): State<App>, _: User, Path(id): Path<i64>, req: Request) -> R<Response> {
    let path = path_of(&app, id).await?;
    Ok(ServeFile::new(path).oneshot(req).await?.into_response())
}

#[derive(Deserialize)]
pub struct HlsQuery {
    #[serde(default)]
    start: f64,
    #[serde(default)]
    audio: usize,
    /// Hauteur cible (480, 720, 1080…). Absent = qualité d'origine.
    height: Option<u32>,
    /// Sous-titre image à incruster.
    burn: Option<usize>,
}

/// Démarre (ou remplace) la session HLS de l'utilisateur et renvoie l'URL de sa playlist.
pub async fn hls_start(State(app): State<App>, user: User, Path(id): Path<i64>, Query(q): Query<HlsQuery>) -> R<Json<String>> {
    let path = path_of(&app, id).await?;
    let p = probe(&path).await?;
    let sid = format!("{id}-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis());
    let dir = app.cfg.data.join("hls").join(&sid);
    tokio::fs::create_dir_all(&dir).await?;

    let h264 = codec(&p, "video") == "h264" && streams(&p, "video").next().is_some_and(|s| s["pix_fmt"] == "yuv420p");
    // Copie vidéo seulement depuis le début : avec -ss, elle démarre à l'image clé précédente,
    // des secondes avant l'audio, et hls.js ne détecte alors plus la piste audio.
    let copy = h264 && q.height.is_none() && q.burn.is_none() && q.start <= 0.0;
    let args = ffmpeg_args(&app.cfg.hwaccel, &path, &q, copy, &dir);
    let child = Command::new("ffmpeg").args(&args).stdin(Stdio::null()).kill_on_drop(true).spawn()?;

    let session = Session { id: sid.clone(), child, dir };
    app.hls.lock().await.insert(user.id, session); // l'ancienne session est arrêtée ici
    Ok(Json(format!("/api/hls/{sid}/index.m3u8")))
}

fn ffmpeg_args(hw: &str, path: &str, q: &HlsQuery, copy: bool, dir: &std::path::Path) -> Vec<String> {
    let mut a: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into()];
    if hw == "vaapi" {
        a.extend(["-vaapi_device".into(), "/dev/dri/renderD128".into()]);
    }
    a.extend(["-ss".into(), q.start.to_string(), "-i".into(), path.into()]);
    if copy {
        a.extend(["-map".into(), "0:v:0".into(), "-c:v".into(), "copy".into()]);
    } else {
        let mut f: Vec<String> = q.height.map(|h| format!("scale=-2:'min({h},ih)'")).into_iter().collect();
        if hw == "vaapi" {
            f.push("format=nv12,hwupload".into());
        }
        let chain = if f.is_empty() { "null".into() } else { f.join(",") };
        let graph = match q.burn {
            Some(s) => format!("[0:v:0][0:s:{s}]overlay,{chain}[v]"),
            None => format!("[0:v:0]{chain}[v]"),
        };
        a.extend(["-filter_complex".into(), graph, "-map".into(), "[v]".into()]);
        a.extend(encoder(hw, q.height));
        // Une image clé par segment : découpage HLS exact.
        a.extend(["-force_key_frames".into(), format!("expr:gte(t,n_forced*{SEGMENT})")]);
    }
    a.extend(["-map".into(), format!("0:a:{}?", q.audio), "-c:a".into(), "aac".into(), "-ac".into(), "2".into(), "-b:a".into(), "192k".into()]);
    a.extend(
        ["-f", "hls", "-hls_time", &SEGMENT.to_string(), "-hls_list_size", "0", "-hls_playlist_type", "event", "-hls_segment_filename"]
            .iter()
            .map(|s| s.to_string()),
    );
    a.push(dir.join("s%05d.ts").to_string_lossy().into());
    a.push(dir.join("index.m3u8").to_string_lossy().into());
    a
}

/// Encodeur H.264 (matériel si configuré), débit plafonné selon la hauteur cible.
fn encoder(hw: &str, height: Option<u32>) -> Vec<String> {
    let enc: &[&str] = match hw {
        "nvenc" => &["-c:v", "h264_nvenc", "-preset", "p4"],
        "qsv" => &["-c:v", "h264_qsv", "-preset", "veryfast"],
        "vaapi" => &["-c:v", "h264_vaapi"],
        _ => &["-c:v", "libx264", "-preset", "veryfast", "-crf", "21"],
    };
    let mut a: Vec<String> = enc.iter().map(|s| s.to_string()).collect();
    if let Some(h) = height {
        let kbps = match h { 0..=480 => 1500, 481..=720 => 4000, 721..=1080 => 8000, _ => 20000 };
        a.extend(["-maxrate".into(), format!("{kbps}k"), "-bufsize".into(), format!("{}k", kbps * 2)]);
    }
    a
}

// ---------- Téléchargement hors ligne ----------

const DOWNLOAD_HEIGHTS: &[u32] = &[2160, 1080, 720, 480];
/// Les copies converties sont supprimées 24 h après leur création.
const DOWNLOAD_TTL: Duration = Duration::from_secs(24 * 3600);

#[derive(Deserialize)]
pub struct DownloadQuery {
    /// Ne renvoie que l'avancement (1 = prêt), sans le fichier.
    #[serde(default)]
    status: bool,
}

/// `quality` = original | 2160 | 1080 | 720 | 480.
/// Fichier prêt : servi en pièce jointe, avec Range (reprise des téléchargements interrompus).
/// Sinon : la conversion démarre (une à la fois) et la réponse 202 donne l'avancement (0 à 1).
pub async fn download(
    State(app): State<App>,
    _: User,
    Path((id, quality)): Path<(i64, String)>,
    Query(q): Query<DownloadQuery>,
    req: Request,
) -> R<Response> {
    let src = path_of(&app, id).await?;
    let stem: String = std::path::Path::new(&src)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || " .-_()".contains(c) { c } else { '_' })
        .collect();
    if quality == "original" {
        if q.status {
            return Ok(Json(1.0).into_response());
        }
        let ext = std::path::Path::new(&src).extension().unwrap_or_default().to_string_lossy().into_owned();
        return attachment(ServeFile::new(&src).oneshot(req).await?.into_response(), &format!("{stem}.{ext}"));
    }
    let Some(height) = quality.parse().ok().filter(|h| DOWNLOAD_HEIGHTS.contains(h)) else {
        return err(StatusCode::BAD_REQUEST, "qualité invalide");
    };
    let out = app.cfg.data.join("downloads").join(format!("{id}-{height}.mp4"));
    if tokio::fs::try_exists(&out).await.unwrap_or(false) {
        if q.status {
            return Ok(Json(1.0).into_response());
        }
        return attachment(ServeFile::new(&out).oneshot(req).await?.into_response(), &format!("{stem} {height}p.mp4"));
    }
    let key = format!("{id}-{height}");
    let mut jobs = app.downloads.lock().await;
    match jobs.get(&key).copied() {
        Some(p) if p < 0.0 => {
            jobs.remove(&key); // le prochain clic relance
            err(StatusCode::INTERNAL_SERVER_ERROR, "La conversion a échoué.")
        }
        Some(p) => Ok((StatusCode::ACCEPTED, Json(p)).into_response()),
        None => {
            jobs.insert(key.clone(), 0.0);
            tokio::spawn(convert(app.clone(), src, height, out, key));
            Ok((StatusCode::ACCEPTED, Json(0.0)).into_response())
        }
    }
}

fn attachment(mut res: Response, name: &str) -> R<Response> {
    res.headers_mut().insert(header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\"").parse()?);
    Ok(res)
}

async fn convert(app: App, src: String, height: u32, out: PathBuf, key: String) {
    let _slot = app.download_slot.acquire().await;
    let res = convert_inner(&app, &src, height, &out, &key).await;
    let mut jobs = app.downloads.lock().await;
    match res {
        Ok(()) => {
            jobs.remove(&key);
        }
        Err(e) => {
            tracing::error!("conversion {key} : {e:#}");
            jobs.insert(key, -1.0);
        }
    }
}

async fn convert_inner(app: &App, src: &str, height: u32, out: &std::path::Path, key: &str) -> anyhow::Result<()> {
    let dir = out.parent().unwrap_or(out);
    tokio::fs::create_dir_all(dir).await?;
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(e) = entries.next_entry().await? {
        if e.metadata().await?.modified()?.elapsed().unwrap_or_default() > DOWNLOAD_TTL {
            let _ = tokio::fs::remove_file(e.path()).await;
        }
    }

    let p = probe(src).await.map_err(|e| anyhow::anyhow!("ffprobe : {}", e.1))?;
    let duration: f64 = p["format"]["duration"].as_str().and_then(|d| d.parse().ok()).unwrap_or(0.0);
    let v = streams(&p, "video").next();
    // Source déjà en H.264 8 bits et pas plus haute que demandé : simple remux, quasi instantané.
    let copy = codec(&p, "video") == "h264"
        && v.is_some_and(|s| s["pix_fmt"] == "yuv420p" && s["height"].as_u64().is_some_and(|h| h <= height as u64));

    let hw = app.cfg.hwaccel.as_str();
    let tmp = out.with_extension("part");
    let mut a: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into(), "-nostats".into(), "-y".into()];
    if hw == "vaapi" && !copy {
        a.extend(["-vaapi_device".into(), "/dev/dri/renderD128".into()]);
    }
    a.extend(["-i".into(), src.into(), "-map".into(), "0:v:0".into(), "-map".into(), "0:a?".into()]);
    if copy {
        a.extend(["-c:v".into(), "copy".into()]);
    } else {
        let mut vf = format!("scale=-2:'min({height},ih)'");
        if hw == "vaapi" {
            vf += ",format=nv12,hwupload";
        }
        a.extend(["-vf".into(), vf]);
        a.extend(encoder(hw, Some(height)));
    }
    a.extend(["-c:a", "aac", "-ac", "2", "-b:a", "192k", "-movflags", "+faststart", "-f", "mp4", "-progress", "pipe:1"].map(String::from));
    a.push(tmp.to_string_lossy().into());

    let mut child = Command::new("ffmpeg").args(&a).stdin(Stdio::null()).stdout(Stdio::piped()).kill_on_drop(true).spawn()?;
    let mut lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(child.stdout.take().expect("stdout")));
    while let Some(line) = lines.next_line().await? {
        if let Some(us) = line.strip_prefix("out_time_us=").and_then(|v| v.parse::<f64>().ok())
            && duration > 0.0
        {
            app.downloads.lock().await.insert(key.into(), (us / 1e6 / duration).clamp(0.0, 0.99));
        }
    }
    if !child.wait().await?.success() {
        let _ = tokio::fs::remove_file(&tmp).await;
        anyhow::bail!("ffmpeg a échoué");
    }
    tokio::fs::rename(&tmp, out).await?;
    Ok(())
}

/// Seuls `index.m3u8` et `sNNNNN.ts` sont servis : aucun chemin arbitraire.
fn valid_file(f: &str) -> bool {
    f == "index.m3u8" || (f.len() == 9 && f.starts_with('s') && f.ends_with(".ts") && f[1..6].bytes().all(|b| b.is_ascii_digit()))
}

pub async fn hls_file(State(app): State<App>, user: User, Path((sid, file)): Path<(String, String)>) -> R<Response> {
    if !valid_file(&file) {
        return err(StatusCode::NOT_FOUND, "fichier inconnu");
    }
    let dir = match app.hls.lock().await.get(&user.id) {
        Some(s) if s.id == sid => s.dir.clone(),
        _ => return err(StatusCode::NOT_FOUND, "session terminée"),
    };
    let path = dir.join(&file);
    // ffmpeg met quelques secondes à produire la playlist au démarrage.
    for _ in 0..150 {
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let body = tokio::fs::read(&path).await.map_err(|_| crate::E(StatusCode::NOT_FOUND, "segment indisponible".into()))?;
    let ctype = if file.ends_with(".ts") { "video/mp2t" } else { "application/vnd.apple.mpegurl" };
    Ok(([(header::CONTENT_TYPE, ctype), (header::CACHE_CONTROL, "no-cache")], Body::from(body)).into_response())
}

pub async fn hls_stop(State(app): State<App>, user: User) -> StatusCode {
    app.hls.lock().await.remove(&user.id);
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
pub struct SubQuery {
    #[serde(default)]
    start: f64,
}

/// Sous-titre texte converti en WebVTT, décalé de `start` pour s'aligner sur une session HLS.
pub async fn subtitle(State(app): State<App>, _: User, Path((id, n)): Path<(i64, usize)>, Query(q): Query<SubQuery>) -> R<Response> {
    let path = path_of(&app, id).await?;
    let mut args: Vec<String> = ["-hide_banner", "-loglevel", "error", "-ss", &q.start.to_string()].map(String::from).to_vec();
    if n >= EXTERNAL {
        let Some((file, _)) = external_subs(&path).into_iter().nth(n - EXTERNAL) else { return err(StatusCode::NOT_FOUND, "sous-titre introuvable") };
        // SRT français souvent en Windows-1252 : ffmpeg suppose UTF-8 et échouerait sur les accents.
        if std::str::from_utf8(&tokio::fs::read(&file).await?).is_err() {
            args.extend(["-sub_charenc".into(), "CP1252".into()]);
        }
        args.extend(["-i".into(), file.to_string_lossy().into(), "-map".into(), "0:s:0".into()]);
    } else {
        args.extend(["-i".into(), path, "-map".into(), format!("0:s:{n}")]);
    }
    args.extend(["-f", "webvtt", "-"].map(String::from));
    let out = Command::new("ffmpeg").args(&args).output().await?;
    if !out.status.success() {
        return err(StatusCode::UNPROCESSABLE_ENTITY, "sous-titre illisible");
    }
    Ok(([(header::CONTENT_TYPE, "text/vtt; charset=utf-8")], out.stdout).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_subtitles() {
        let dir = std::env::temp_dir().join(format!("bitflow-subs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["Film.mkv", "Film.srt", "Film.fr.srt", "Film.en.forced.SRT", "Film2.srt", "Film.fr.txt"] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        let subs: Vec<Track> = external_subs(dir.join("Film.mkv").to_str().unwrap()).into_iter().map(|(_, t)| t).collect();
        std::fs::remove_dir_all(&dir).unwrap();
        let labels: Vec<&str> = subs.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, ["en · forced · externe", "fr · externe", "Externe"]);
        assert_eq!((subs[0].lang.as_deref(), subs[0].forced), (Some("en"), true));
        assert_eq!((subs[1].lang.as_deref(), subs[1].forced), (Some("fr"), false));
        assert_eq!(subs[2].lang, None);
    }

    fn t(index: usize, lang: &str, forced: bool, burn: bool, default: bool) -> Track {
        Track { index, label: String::new(), default, burn, lang: norm_lang(lang), forced }
    }

    #[test]
    fn track_choice() {
        let prefs = |audio: Option<&str>, mode: &str| Prefs { audio_lang: audio.map(Into::into), sub_lang: "fr".into(), sub_mode: mode.into() };
        let audio = [t(0, "eng", false, false, true), t(1, "fre", false, false, false)];
        let subs = [t(0, "fre", false, true, false), t(1, "fra", false, false, false), t(2, "fr", true, false, false), t(3, "eng", false, false, false)];
        // VO anglaise, mode « autre langue » : sous-titres français complets, texte plutôt qu'image.
        assert_eq!(choose(&audio, &subs, &prefs(None, "foreign"), None), (0, Some(1)));
        // Audio français préféré : seulement les forcés.
        assert_eq!(choose(&audio, &subs, &prefs(Some("fr"), "foreign"), None), (1, Some(2)));
        assert_eq!(choose(&audio, &subs, &prefs(Some("fr"), "always"), None), (1, Some(1)));
        assert_eq!(choose(&audio, &subs, &prefs(None, "off"), None), (0, None));
        // Langue audio absente du fichier : piste par défaut.
        assert_eq!(choose(&audio, &subs, &prefs(Some("de"), "off"), None), (0, None));
        // VO française d'après TMDB, doublage anglais par défaut dans le fichier : la VO l'emporte.
        assert_eq!(choose(&audio, &subs, &prefs(None, "foreign"), Some("fr")), (1, Some(2)));
        // Pistes sans étiquette de langue : supposées en VO. Film anglais -> sous-titres français.
        let untagged = [t(0, "", false, false, true)];
        assert_eq!(choose(&untagged, &subs, &prefs(None, "foreign"), Some("en")), (0, Some(1)));
        assert_eq!(choose(&untagged, &subs, &prefs(None, "foreign"), None), (0, Some(2)));
        assert_eq!(norm_lang("und"), None);
    }

    #[test]
    fn hls_file_names() {
        assert!(valid_file("index.m3u8"));
        assert!(valid_file("s00042.ts"));
        assert!(!valid_file(".."));
        assert!(!valid_file("../../x.ts"));
        assert!(!valid_file("s0004a.ts"));
    }

    #[test]
    fn direct_detection() {
        let p: Value = serde_json::json!({"format": {"format_name": "mov,mp4,m4a"}, "streams": [
            {"codec_type": "video", "codec_name": "h264"}, {"codec_type": "audio", "codec_name": "aac"}]});
        assert!(is_direct(&p));
        let p: Value = serde_json::json!({"format": {"format_name": "matroska,webm"}, "streams": [
            {"codec_type": "video", "codec_name": "hevc"}, {"codec_type": "audio", "codec_name": "dts"}]});
        assert!(!is_direct(&p));
    }
}
