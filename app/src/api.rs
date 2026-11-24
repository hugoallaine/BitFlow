//! Client HTTP de l'API BitFlow. Le cookie de session est envoyé automatiquement (même origine).
use serde::{Serialize, de::DeserializeOwned};

pub fn origin() -> String {
    web_sys::window().and_then(|w| w.location().origin().ok()).unwrap_or_default()
}

/// Navigation complète (recharge l'état de l'application).
pub fn go(path: &str) {
    if let Some(w) = web_sys::window() {
        let _ = w.location().set_href(path);
    }
}

pub fn enc(s: &str) -> String {
    js_sys::encode_uri_component(s).into()
}

/// `kind` = "poster" ou "backdrop" : le serveur choisit et stocke une seule taille par type.
/// Les chemins TMDB commencent par « / » ; les pochettes d'albums locales (« c12.jpg ») non.
pub fn img(path: &Option<String>, kind: &str) -> Option<String> {
    path.as_ref().map(|p| if p.starts_with('/') { format!("/api/img/{kind}{p}") } else { format!("/api/img/cover/{p}") })
}

/// Préférence locale à l'appareil (ex. qualité de téléchargement).
pub fn setting(key: &str) -> Option<String> {
    web_sys::window()?.local_storage().ok()??.get_item(key).ok()?
}

pub fn set_setting(key: &str, value: &str) {
    if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = s.set_item(key, value);
    }
}

async fn parse<T: DeserializeOwned>(path: &str, r: Result<reqwest::Response, reqwest::Error>) -> Result<T, String> {
    let r = r.map_err(|e| e.to_string())?;
    let status = r.status().as_u16();
    let text = r.text().await.map_err(|e| e.to_string())?;
    if status == 401 && !path.starts_with("/api/auth/") {
        go("/login");
    }
    if status >= 400 {
        return Err(if text.is_empty() { format!("Erreur {status}") } else { text });
    }
    serde_json::from_str(if text.is_empty() { "null" } else { &text }).map_err(|e| e.to_string())
}

pub async fn get<T: DeserializeOwned>(path: &str) -> Result<T, String> {
    parse(path, reqwest::get(format!("{}{path}", origin())).await).await
}

pub async fn post<T: DeserializeOwned>(path: &str, body: &impl Serialize) -> Result<T, String> {
    parse(path, reqwest::Client::new().post(format!("{}{path}", origin())).json(body).send().await).await
}

pub async fn put<T: DeserializeOwned>(path: &str, body: &impl Serialize) -> Result<T, String> {
    parse(path, reqwest::Client::new().put(format!("{}{path}", origin())).json(body).send().await).await
}

pub async fn delete(path: &str) -> Result<(), String> {
    parse(path, reqwest::Client::new().delete(format!("{}{path}", origin())).send().await).await
}
