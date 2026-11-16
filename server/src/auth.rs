//! Authentification : mot de passe (Argon2id, NIST SP 800-63B-4) et OIDC (Authorization Code + PKCE).
use std::{
    sync::OnceLock,
    time::{Duration, Instant},
};

use argon2::{
    Argon2, PasswordHash, PasswordHasher, PasswordVerifier,
    password_hash::{SaltString, rand_core::{OsRng, RngCore}},
};
use axum::{
    extract::{FromRequestParts, OptionalFromRequestParts, Query, State},
    http::{HeaderMap, StatusCode, header, request::Parts},
    response::{IntoResponse, Redirect, Response},
};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet, EndpointNotSet, EndpointSet,
    IssuerUrl, Nonce, OAuth2TokenResponse, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope,
    TokenResponse,
    core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata},
};
use sha2::{Digest, Sha256};

use crate::{App, E, R, err};

const SESSION_DAYS: i64 = 30;
const MAX_FAILS: u32 = 10;
const LOCKOUT: Duration = Duration::from_secs(15 * 60);

/// Utilisateur authentifié, extrait du cookie de session ou d'un en-tête `Authorization: Bearer`.
pub struct User {
    pub id: i64,
    pub admin: bool,
}

impl FromRequestParts<App> for User {
    type Rejection = E;
    async fn from_request_parts(parts: &mut Parts, app: &App) -> R<Self> {
        let token = bearer(&parts.headers).or_else(|| cookie(&parts.headers, &cookie_name(app)));
        let Some(token) = token else { return err(StatusCode::UNAUTHORIZED, "non connecté") };
        let row: Option<(i64, bool)> = sqlx::query_as(
            "SELECT u.id, u.admin FROM sessions s JOIN users u ON u.id = s.user_id WHERE s.hash = ? AND s.expires > unixepoch()",
        )
        .bind(sha256(&token))
        .fetch_optional(&app.db)
        .await?;
        match row {
            Some((id, admin)) => Ok(User { id, admin }),
            None => err(StatusCode::UNAUTHORIZED, "session expirée"),
        }
    }
}

impl OptionalFromRequestParts<App> for User {
    type Rejection = E;
    async fn from_request_parts(parts: &mut Parts, app: &App) -> R<Option<Self>> {
        Ok(<User as FromRequestParts<App>>::from_request_parts(parts, app).await.ok())
    }
}

fn bearer(h: &HeaderMap) -> Option<String> {
    h.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ").map(str::to_owned)
}

fn cookie(h: &HeaderMap, name: &str) -> Option<String> {
    h.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';')).find_map(|kv| {
        let (k, v) = kv.trim().split_once('=')?;
        (k == name).then(|| v.to_owned())
    })
}

/// Préfixe `__Host-` (Secure, Path=/, sans Domain) dès que l'URL publique est en HTTPS.
fn cookie_name(app: &App) -> String {
    if secure(app) { "__Host-bitflow".into() } else { "bitflow".into() }
}

fn secure(app: &App) -> bool {
    app.cfg.url.starts_with("https://")
}

fn sha256(s: &str) -> Vec<u8> {
    Sha256::digest(s.as_bytes()).to_vec()
}

fn random_token() -> String {
    let mut b = [0u8; 32];
    OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Crée une session et renvoie le jeton (en clair une seule fois ; seul son SHA-256 est stocké).
pub async fn new_session(app: &App, user_id: i64) -> R<String> {
    let token = random_token();
    sqlx::query("DELETE FROM sessions WHERE expires <= unixepoch()").execute(&app.db).await?;
    sqlx::query("INSERT INTO sessions(hash, user_id, expires) VALUES (?, ?, unixepoch() + ?)")
        .bind(sha256(&token))
        .bind(user_id)
        .bind(SESSION_DAYS * 86400)
        .execute(&app.db)
        .await?;
    Ok(token)
}

/// `SameSite=Lax` + API JSON uniquement (pas de formulaire cross-site possible) = protection CSRF.
pub fn set_cookie(app: &App, token: &str, max_age: i64) -> (header::HeaderName, String) {
    let sec = if secure(app) { "; Secure" } else { "" };
    (header::SET_COOKIE, format!("{}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{sec}", cookie_name(app)))
}

pub async fn logout(State(app): State<App>, headers: HeaderMap) -> R<impl IntoResponse> {
    if let Some(t) = bearer(&headers).or_else(|| cookie(&headers, &cookie_name(&app))) {
        sqlx::query("DELETE FROM sessions WHERE hash = ?").bind(sha256(&t)).execute(&app.db).await?;
    }
    Ok([set_cookie(&app, "", 0)])
}

// ---------- Mot de passe ----------

pub fn hash(pw: &str) -> String {
    // Argon2::default() = Argon2id v19, m=19 MiB, t=2, p=1 : minimum recommandé par l'OWASP.
    Argon2::default().hash_password(pw.as_bytes(), &SaltString::generate(&mut OsRng)).expect("argon2").to_string()
}

fn verify(pw: &str, stored: &str) -> bool {
    PasswordHash::new(stored).is_ok_and(|h| Argon2::default().verify_password(pw.as_bytes(), &h).is_ok())
}

/// NIST SP 800-63B-4 : ≥ 15 caractères si le mot de passe est le seul facteur, ≤ 256 acceptés,
/// aucune règle de composition, refus des mots de passe compromis.
pub async fn check_policy(app: &App, pw: &str) -> R<()> {
    let n = pw.chars().count();
    if n < 15 {
        return err(StatusCode::BAD_REQUEST, "Phrase secrète trop courte (15 caractères minimum).");
    }
    if n > 256 {
        return err(StatusCode::BAD_REQUEST, "Phrase secrète trop longue (256 caractères maximum).");
    }
    if pwned(app, pw).await {
        return err(StatusCode::BAD_REQUEST, "Cette phrase secrète figure dans des fuites de données connues.");
    }
    Ok(())
}

/// Have I Been Pwned, k-anonymat : seuls les 5 premiers caractères du SHA-1 quittent le serveur.
/// Hors ligne, on laisse passer plutôt que bloquer la création de compte.
async fn pwned(app: &App, pw: &str) -> bool {
    let hex: String = sha1::Sha1::digest(pw.as_bytes()).iter().map(|b| format!("{b:02X}")).collect();
    let (prefix, suffix) = hex.split_at(5);
    let res = app.http.get(format!("https://api.pwnedpasswords.com/range/{prefix}")).header("Add-Padding", "true").send().await;
    match res.and_then(|r| r.error_for_status()) {
        Ok(r) => r.text().await.unwrap_or_default().lines().any(|l| l.split_once(':').is_some_and(|(s, c)| s == suffix && c.trim() != "0")),
        Err(e) => {
            tracing::warn!("HIBP injoignable, vérification ignorée : {e}");
            false
        }
    }
}

pub async fn login(app: &App, name: &str, pw: &str) -> R<i64> {
    {
        let mut fails = app.fails.lock().await;
        fails.retain(|_, (_, t)| t.elapsed() < LOCKOUT);
        if fails.get(name).is_some_and(|(n, _)| *n >= MAX_FAILS) {
            return err(StatusCode::TOO_MANY_REQUESTS, "Trop de tentatives, réessayez dans 15 minutes.");
        }
    }
    let row: Option<(i64, Option<String>)> =
        sqlx::query_as("SELECT id, pw FROM users WHERE name = ?").bind(name).fetch_optional(&app.db).await?;
    // Hash factice si l'utilisateur n'existe pas : même durée de réponse, pas d'énumération des comptes.
    static DUMMY: OnceLock<String> = OnceLock::new();
    let stored = row.as_ref().and_then(|r| r.1.clone()).unwrap_or_else(|| DUMMY.get_or_init(|| hash("dummy-password-for-timing")).clone());
    let pw = pw.to_owned();
    let ok = tokio::task::spawn_blocking(move || verify(&pw, &stored)).await?;
    match row {
        Some((id, Some(_))) if ok => {
            app.fails.lock().await.remove(name);
            Ok(id)
        }
        _ => {
            let mut fails = app.fails.lock().await;
            let e = fails.entry(name.to_owned()).or_insert((0, Instant::now()));
            e.0 += 1;
            e.1 = Instant::now();
            err(StatusCode::UNAUTHORIZED, "Identifiant ou phrase secrète incorrect.")
        }
    }
}

// ---------- OIDC ----------

type Oidc = CoreClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointMaybeSet, EndpointMaybeSet>;

/// Découverte à chaque connexion : suit la rotation des clés (JWKS) du fournisseur sans redémarrage.
async fn oidc_client(app: &App) -> R<Oidc> {
    let Some((issuer, id, secret)) = &app.cfg.oidc else { return err(StatusCode::NOT_FOUND, "OIDC non configuré") };
    let meta = CoreProviderMetadata::discover_async(IssuerUrl::new(issuer.clone())?, &app.http).await?;
    Ok(CoreClient::from_provider_metadata(meta, ClientId::new(id.clone()), secret.clone().map(ClientSecret::new))
        .set_redirect_uri(RedirectUrl::new(format!("{}/api/auth/oidc/callback", app.cfg.url))?))
}

pub async fn oidc_start(State(app): State<App>) -> R<Redirect> {
    let client = oidc_client(&app).await?;
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, state, nonce) = client
        .authorize_url(CoreAuthenticationFlow::AuthorizationCode, CsrfToken::new_random, Nonce::new_random)
        .add_scope(Scope::new("profile".into()))
        .add_scope(Scope::new("email".into()))
        .set_pkce_challenge(challenge)
        .url();
    let mut pending = app.oidc_pending.lock().await;
    pending.retain(|_, v| v.2.elapsed() < Duration::from_secs(600));
    pending.insert(state.secret().clone(), (verifier.secret().clone(), nonce.secret().clone(), Instant::now()));
    Ok(Redirect::to(url.as_str()))
}

#[derive(serde::Deserialize)]
pub struct Callback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

pub async fn oidc_callback(State(app): State<App>, Query(q): Query<Callback>) -> R<Response> {
    if let Some(e) = q.error {
        tracing::warn!("OIDC refusé : {e}");
        return Ok(Redirect::to("/login?error=oidc").into_response());
    }
    let (Some(code), Some(state)) = (q.code, q.state) else { return err(StatusCode::BAD_REQUEST, "requête invalide") };
    // Le state est à usage unique : retiré avant tout appel réseau.
    let Some((verifier, nonce, _)) = app.oidc_pending.lock().await.remove(&state) else {
        return err(StatusCode::BAD_REQUEST, "state inconnu ou expiré");
    };
    let client = oidc_client(&app).await?;
    let token = client
        .exchange_code(AuthorizationCode::new(code))?
        .set_pkce_verifier(PkceCodeVerifier::new(verifier))
        .request_async(&app.http)
        .await?;
    let Some(id_token) = token.id_token() else { return err(StatusCode::BAD_GATEWAY, "id_token absent") };
    // Vérifie signature, iss, aud, exp et nonce.
    let claims = id_token.claims(&client.id_token_verifier(), &Nonce::new(nonce))?;
    if let Some(expected) = claims.access_token_hash() {
        let actual = openidconnect::AccessTokenHash::from_token(token.access_token(), id_token.signing_alg()?, id_token.signing_key(&client.id_token_verifier())?)?;
        if &actual != expected {
            return err(StatusCode::BAD_GATEWAY, "at_hash invalide");
        }
    }
    let sub = format!("{}|{}", claims.issuer().as_str(), claims.subject().as_str());
    let name = claims
        .preferred_username()
        .map(|u| u.as_str().to_owned())
        .or_else(|| claims.email().map(|e| e.as_str().to_owned()))
        .unwrap_or_else(|| claims.subject().as_str().to_owned());

    let existing: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE oidc_sub = ?").bind(&sub).fetch_optional(&app.db).await?;
    let id = match existing {
        Some(id) => id,
        None => {
            // Pas de liaison par e-mail (risque de prise de compte) : un sujet OIDC = un compte.
            // Nom déjà pris par un compte local : on suffixe avec le début du sujet.
            let first = !sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM users)").fetch_one(&app.db).await?;
            let short: String = claims.subject().as_str().chars().take(6).collect();
            let taken: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE name = ?)").bind(&name).fetch_one(&app.db).await?;
            let name = if taken { format!("{name}-{short}") } else { name };
            sqlx::query_scalar("INSERT INTO users(name, oidc_sub, admin) VALUES (?, ?, ?) RETURNING id")
                .bind(name)
                .bind(&sub)
                .bind(first)
                .fetch_one(&app.db)
                .await?
        }
    };
    let token = new_session(&app, id).await?;
    Ok(([set_cookie(&app, &token, SESSION_DAYS * 86400)], Redirect::to("/")).into_response())
}

pub fn session_max_age() -> i64 {
    SESSION_DAYS * 86400
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_roundtrip() {
        let h = hash("correct horse battery staple");
        assert!(h.starts_with("$argon2id$"));
        assert!(verify("correct horse battery staple", &h));
        assert!(!verify("wrong horse battery staple", &h));
    }

    #[test]
    fn cookie_parsing() {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, "a=1; bitflow=tok; b=2".parse().unwrap());
        assert_eq!(cookie(&h, "bitflow").as_deref(), Some("tok"));
        assert_eq!(cookie(&h, "bit"), None);
    }
}
