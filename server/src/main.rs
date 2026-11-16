mod api;
mod auth;
mod db;
mod loudness;
mod musicbrainz;
mod scan;
mod stream;
mod tmdb;

use std::{collections::HashMap, env, path::PathBuf, sync::Arc, time::Instant};

use axum::{
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use tokio::sync::Mutex;
use tower_http::{
    compression::{CompressionLayer, Predicate, predicate::{DefaultPredicate, NotForContentType}},
    services::{ServeDir, ServeFile},
    set_header::SetResponseHeaderLayer,
};

/// Configuration lue une seule fois depuis l'environnement.
pub struct Config {
    pub data: PathBuf,
    pub movies: Vec<PathBuf>,
    pub shows: Vec<PathBuf>,
    pub music: Vec<PathBuf>,
    /// Contact ajouté au User-Agent MusicBrainz (URL ou e-mail de l'administrateur), recommandé par leurs règles.
    pub contact: Option<String>,
    /// Clé personnelle Fanart.tv (gratuite) : images d'artistes.
    pub fanart_key: Option<String>,
    pub url: String,
    pub tmdb_token: Option<String>,
    pub lang: String,
    pub hwaccel: String,
    pub oidc: Option<(String, String, Option<String>)>,
    /// Taille unique conservée pour les affiches et pour les fonds/vignettes d'épisodes.
    pub img_poster: String,
    pub img_backdrop: String,
}

impl Config {
    fn from_env() -> Self {
        let var = |k: &str| env::var(k).ok().filter(|v| !v.is_empty());
        let dirs = |k: &str| var(k).map(|v| v.split(',').map(PathBuf::from).collect()).unwrap_or_default();
        Config {
            data: var("BITFLOW_DATA").unwrap_or("./data".into()).into(),
            movies: dirs("BITFLOW_MOVIES"),
            shows: dirs("BITFLOW_SHOWS"),
            music: dirs("BITFLOW_MUSIC"),
            contact: var("BITFLOW_CONTACT"),
            fanart_key: var("FANART_KEY"),
            url: var("BITFLOW_URL").unwrap_or("http://localhost:8080".into()).trim_end_matches('/').into(),
            tmdb_token: var("TMDB_TOKEN"),
            lang: var("BITFLOW_LANG").unwrap_or("fr-FR".into()),
            hwaccel: var("BITFLOW_HWACCEL").unwrap_or("none".into()),
            img_poster: img_size(var("BITFLOW_IMG_POSTER"), "w342"),
            img_backdrop: img_size(var("BITFLOW_IMG_BACKDROP"), "w780"),
            oidc: var("OIDC_ISSUER").zip(var("OIDC_CLIENT_ID")).map(|(i, c)| (i, c, var("OIDC_CLIENT_SECRET"))),
        }
    }
}

/// Tailles proposées par TMDB ; une valeur inconnue retombe sur la valeur par défaut.
fn img_size(v: Option<String>, default: &str) -> String {
    const SIZES: &[&str] = &["w92", "w154", "w185", "w300", "w342", "w500", "w780", "w1280", "original"];
    match v {
        Some(v) if SIZES.contains(&v.as_str()) => v,
        Some(v) => {
            tracing::warn!("taille d'image inconnue « {v} », utilisation de {default}");
            default.into()
        }
        None => default.into(),
    }
}

#[derive(Clone)]
pub struct App {
    pub db: sqlx::SqlitePool,
    pub http: reqwest::Client,
    /// Suit les redirections : réservé aux hôtes d'images fixes (Cover Art Archive -> archive.org).
    pub http_follow: reqwest::Client,
    pub cfg: Arc<Config>,
    /// Une session HLS (ffmpeg) par utilisateur.
    pub hls: Arc<Mutex<HashMap<i64, stream::Session>>>,
    /// Échecs de connexion par nom d'utilisateur (anti brute-force).
    pub fails: Arc<Mutex<HashMap<String, (u32, Instant)>>>,
    /// state OIDC -> (vérificateur PKCE, nonce, création).
    pub oidc_pending: Arc<Mutex<HashMap<String, (String, String, Instant)>>>,
    /// Conversions pour le téléchargement : "id-hauteur" -> avancement (0..1, -1 = échec).
    pub downloads: Arc<Mutex<HashMap<String, f64>>>,
    /// Une seule conversion à la fois : elles sont lourdes et ne doivent pas étouffer le streaming.
    pub download_slot: Arc<tokio::sync::Semaphore>,
}

/// Erreur HTTP : code + message. Toute erreur anyhow devient un 500 journalisé.
pub struct E(pub StatusCode, pub String);
pub type R<T> = Result<T, E>;

pub fn err<T>(code: StatusCode, msg: &str) -> R<T> {
    Err(E(code, msg.into()))
}

impl<T: Into<anyhow::Error>> From<T> for E {
    fn from(e: T) -> Self {
        let e = e.into();
        tracing::error!("{e:#}");
        E(StatusCode::INTERNAL_SERVER_ERROR, "erreur interne".into())
    }
}

impl IntoResponse for E {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

/// User-Agent identifiable exigé par MusicBrainz : « Appli/version ( contact ) ».
fn user_agent(cfg: &Config) -> String {
    format!("BitFlow/{} ( {} )", env!("CARGO_PKG_VERSION"), cfg.contact.as_deref().unwrap_or("self-hosted media server"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(env::var("RUST_LOG").unwrap_or("info".into()).as_str()).init();
    let cfg = Config::from_env();
    let cache = cfg.data.join("hls");
    let _ = std::fs::remove_dir_all(&cache);
    std::fs::create_dir_all(&cache)?;

    let app = App {
        db: db::open(&cfg.data.join("bitflow.db")).await?,
        // Pas de redirections : recommandé par openidconnect contre les SSRF.
        http: reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).user_agent(user_agent(&cfg)).build()?,
        http_follow: reqwest::Client::builder().redirect(reqwest::redirect::Policy::limited(5)).user_agent(user_agent(&cfg)).build()?,
        cfg: Arc::new(cfg),
        hls: Default::default(),
        fails: Default::default(),
        oidc_pending: Default::default(),
        downloads: Default::default(),
        download_slot: Arc::new(tokio::sync::Semaphore::new(1)),
    };

    let bg = app.clone();
    tokio::spawn(async move { scan::run(&bg).await });

    let web = PathBuf::from(env::var("BITFLOW_WEB").unwrap_or("./web".into()));
    let compress = DefaultPredicate::new().and(NotForContentType::new("video/"));
    let router = axum::Router::new()
        .nest("/api", api::routes())
        .fallback_service(ServeDir::new(&web).fallback(ServeFile::new(web.join("index.html"))))
        .layer(CompressionLayer::new().compress_when(compress))
        .layer(SetResponseHeaderLayer::overriding(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")))
        .layer(SetResponseHeaderLayer::overriding(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY")))
        .layer(SetResponseHeaderLayer::overriding(header::REFERRER_POLICY, HeaderValue::from_static("same-origin")))
        .with_state(app);

    let addr = env::var("BITFLOW_LISTEN").unwrap_or("0.0.0.0:8080".into());
    tracing::info!("BitFlow écoute sur {addr}");
    axum::serve(tokio::net::TcpListener::bind(addr).await?, router).await?;
    Ok(())
}
