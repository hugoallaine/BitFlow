//! Musique : vues album/artiste et barre de lecture persistante avec file d'attente.
use bitflow_shared::Item;
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{Card, Route, api, clock, icons::{self as ic, icon}, playlists::AddToPlaylist};

/// File de lecture partagée par toute l'application (fournie dans `App`).
#[derive(Clone, Default, PartialEq)]
pub struct Queue {
    pub tracks: Vec<Item>,
    pub index: usize,
}

impl Queue {
    pub fn current(&self) -> Option<&Item> {
        self.tracks.get(self.index)
    }
}

pub fn shuffled(mut v: Vec<Item>) -> Vec<Item> {
    // Fisher-Yates avec l'aléa du navigateur.
    for i in (1..v.len()).rev() {
        v.swap(i, (js_sys::Math::random() * (i + 1) as f64) as usize);
    }
    v
}

#[component]
pub fn AlbumView(item: Item) -> Element {
    let mut queue = use_context::<Signal<Queue>>();
    let mut adding = use_signal(|| None::<Vec<i64>>);
    let id = item.id;
    let tracks = use_resource(use_reactive!(|id| async move { api::get::<Vec<Item>>(&format!("/api/items/{id}/children")).await }));
    let list = match &*tracks.read_unchecked() {
        Some(Ok(v)) => v.clone(),
        Some(Err(e)) => return rsx! { p { class: "error", "{e}" } },
        None => return rsx! { div { class: "loading" } },
    };
    let total: f64 = list.iter().filter_map(|t| t.duration).sum();
    let playing = queue.read().current().map(|t| t.id);
    let multi_disc = list.iter().filter_map(|t| t.season).max().unwrap_or(1) > 1;
    let (l1, l2) = (list.clone(), list.clone());
    let ids: Vec<i64> = list.iter().map(|t| t.id).collect();
    rsx! {
        div { class: "album-head",
            if let Some(src) = api::img(&item.poster, "poster") {
                img { class: "cover", src, alt: "{item.title}" }
            } else {
                div { class: "cover noposter", "♪" }
            }
            div {
                p { class: "muted", "Album" }
                h1 { "{item.title}" }
                p { class: "facts",
                    if let Some(artist_id) = item.parent_id {
                        Link { to: Route::Detail { id: artist_id }, "{item.artist.clone().unwrap_or_default()}" }
                    }
                    if let Some(y) = item.year { span { "{y}" } }
                    span { "{list.len()} pistes · {clock(total)}" }
                }
                div { class: "actions",
                    button { class: "btn primary", onclick: move |_| queue.set(Queue { tracks: l1.clone(), index: 0 }), {icon(ic::PLAY)} "Lecture" }
                    button { class: "btn", onclick: move |_| queue.set(Queue { tracks: shuffled(l2.clone()), index: 0 }), {icon(ic::SHUFFLE)} "Aléatoire" }
                    button { class: "btn", onclick: move |_| adding.set(Some(ids.clone())), {icon(ic::PLAYLIST_ADD)} "Ajouter à une playlist" }
                }
            }
        }
        if let Some(items) = adding() {
            AddToPlaylist { items, on_close: move |_| adding.set(None) }
        }
        ol { class: "tracklist",
            for (i, t) in list.iter().cloned().enumerate() {
                li {
                    key: "{t.id}",
                    class: if playing == Some(t.id) { "track playing" } else { "track" },
                    ondoubleclick: {
                        let all = list.clone();
                        move |_| queue.set(Queue { tracks: all.clone(), index: i })
                    },
                    button {
                        class: "num",
                        title: "Lire",
                        onclick: {
                            let all = list.clone();
                            move |_| queue.set(Queue { tracks: all.clone(), index: i })
                        },
                        span { class: "n",
                            if multi_disc { "{t.season.unwrap_or(1)}-" }
                            "{t.episode.map(|n| n.to_string()).unwrap_or_default()}"
                        }
                        span { class: "p", "▶" }
                    }
                    div { class: "t-title",
                        span { "{t.title}" }
                        if t.artist != item.artist {
                            small { "{t.artist.clone().unwrap_or_default()}" }
                        }
                    }
                    div { class: "t-actions",
                        button { class: "ghost", title: "Ajouter à une playlist", onclick: move |_| adding.set(Some(vec![t.id])), {icon(ic::PLAYLIST_ADD)} }
                        button {
                            class: "ghost",
                            title: "Ajouter à la file de lecture",
                            onclick: {
                                let t = t.clone();
                                move |_| queue.write().tracks.push(t.clone())
                            },
                            {icon(ic::ADD)}
                        }
                    }
                    span { class: "dur", "{clock(t.duration.unwrap_or(0.0))}" }
                }
            }
        }
    }
}

#[component]
pub fn ArtistView(item: Item) -> Element {
    let id = item.id;
    let albums = use_resource(use_reactive!(|id| async move { api::get::<Vec<Item>>(&format!("/api/items/{id}/children")).await }));
    rsx! {
        div { class: "album-head",
            if let Some(src) = api::img(&item.poster, "poster") {
                img { class: "cover round", src, alt: "{item.title}" }
            }
            div {
                p { class: "muted", "Artiste" }
                h1 { "{item.title}" }
            }
        }
        if let Some(Ok(v)) = &*albums.read_unchecked() {
            h2 { "Albums" }
            div { class: "grid",
                for a in v.iter().cloned() {
                    Card { key: "{a.id}", item: a }
                }
            }
        }
    }
}

/// Moteur audio : deux <audio> en alternance -> gain de normalisation -> pré-ampli -> égaliseur 10 bandes
/// -> limiteur -> volume. La piste suivante est préchargée pour l'enchaînement sans blanc ou le fondu.
/// Rust envoie l'état voulu (`set`) ; le JS joue, enchaîne, puis signale `advanced`.
const AUDIO_JS: &str = r#"
let els; while (!(els = [document.getElementById('bf-audio-a'), document.getElementById('bf-audio-b')]).every(Boolean)) await new Promise(r => setTimeout(r, 50));
const E = els.map(a => ({ a, id: null, norm: 1, gain: null }));
const FREQS = [32, 64, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];
let cur = 0, next = null, xf = 0, fading = false, ctx = null, pre, master, bands = [], vol = 1;
let eq = { on: false, preamp: 0, bands: FREQS.map(() => 0) };
const applyEq = () => {
  if (!ctx) return;
  pre.gain.value = eq.on ? Math.pow(10, eq.preamp / 20) : 1;
  bands.forEach((b, i) => b.gain.value = eq.on ? eq.bands[i] : 0);
};
const graph = () => {
  if (ctx) return;
  ctx = new AudioContext();
  pre = ctx.createGain(); master = ctx.createGain(); master.gain.value = vol;
  bands = FREQS.map((f, i) => { const b = ctx.createBiquadFilter(); b.type = i === 0 ? 'lowshelf' : i === 9 ? 'highshelf' : 'peaking'; b.frequency.value = f; b.Q.value = 1.1; return b; });
  // Limiteur : évite la saturation quand l'égaliseur ou la normalisation montent le niveau.
  const lim = ctx.createDynamicsCompressor(); lim.threshold.value = -1; lim.knee.value = 0; lim.ratio.value = 20; lim.attack.value = 0.003; lim.release.value = 0.25;
  [pre, ...bands, lim, master, ctx.destination].reduce((x, y) => (x.connect(y), y));
  E.forEach(e => { const src = ctx.createMediaElementSource(e.a); e.gain = ctx.createGain(); e.gain.gain.value = e.norm; src.connect(e.gain).connect(pre); });
  applyEq();
};
const setGain = (e, v) => { if (!e.gain) return; e.gain.gain.cancelScheduledValues(ctx.currentTime); e.gain.gain.setValueAtTime(v, ctx.currentTime); };
const load = (e, info) => { e.id = info.id; e.norm = info.gain; e.a.src = info.url; e.a.preload = 'auto'; };
const push = action => { const a = E[cur].a; dioxus.send({ t: a.currentTime || 0, d: isFinite(a.duration) ? a.duration : 0, paused: a.paused, ended: a.ended && !next, action: action || null }); };
E.forEach((e, i) => ['timeupdate', 'play', 'pause', 'loadedmetadata'].forEach(ev => e.a.addEventListener(ev, () => { if (i === cur) push(); })));
E.forEach((e, i) => e.a.addEventListener('ended', () => { if (i === cur && !fading) { if (next) advance(0); else push(); } }));
const preloadNext = () => { const o = E[1 - cur]; if (next && !fading && o.id !== next.id) load(o, next); };
// Bascule sur la piste préchargée ; dur = durée du fondu enchaîné (0 = enchaînement direct).
const advance = dur => {
  const o = E[1 - cur], c = E[cur];
  if (!next || o.id !== next.id) return;
  const t = ctx.currentTime;
  if (dur > 0) {
    fading = true;
    o.gain.gain.cancelScheduledValues(t); o.gain.gain.setValueAtTime(0, t); o.gain.gain.linearRampToValueAtTime(o.norm, t + dur);
    c.gain.gain.cancelScheduledValues(t); c.gain.gain.setValueAtTime(c.norm, t); c.gain.gain.linearRampToValueAtTime(0, t + dur);
    setTimeout(() => { c.a.pause(); fading = false; preloadNext(); }, dur * 1000);
  } else { setGain(o, o.norm); c.a.pause(); }
  o.a.currentTime = 0; o.a.play().catch(() => {});
  cur = 1 - cur; next = null;
  push('advanced');
};
setInterval(() => {
  const c = E[cur].a;
  if (!next || fading || c.paused || !isFinite(c.duration)) return;
  const rem = c.duration - c.currentTime;
  const dur = next.fade ? Math.min(xf, c.duration / 3) : 0;
  // Sans fondu : bascule 80 ms avant la fin pour masquer le temps de démarrage (quasi sans blanc).
  if (dur > 0 ? rem <= dur : rem <= 0.08) advance(dur);
}, 40);
const ms = navigator.mediaSession;
if (ms) {
  [['play', 'play'], ['pause', 'pause'], ['previoustrack', 'prev'], ['nexttrack', 'next']].forEach(([k, v]) => { try { ms.setActionHandler(k, () => push(v)); } catch (e) {} });
  try { ms.setActionHandler('seekto', d => { E[cur].a.currentTime = d.seekTime; }); } catch (e) {}
}
while (true) {
  const m = await dioxus.recv();
  if (m.op === 'set') {
    graph(); if (ctx.state === 'suspended') ctx.resume();
    xf = m.xf;
    const c = E[cur];
    if (m.resume != null) {
      // Barre recréée (retour du lecteur vidéo) : même piste, en pause à la même position.
      load(c, m.cur); setGain(c, c.norm); c.a.currentTime = m.resume;
    } else if (c.id !== m.cur.id) {
      // Autre piste demandée (clic, précédent, suivant manuel) : coupure nette.
      E.forEach(e => e.a.pause()); fading = false;
      load(c, m.cur); setGain(c, c.norm); c.a.play().catch(() => {});
    } else { c.norm = m.cur.gain; if (!fading) setGain(c, c.norm); }
    if (ms && window.MediaMetadata) ms.metadata = new MediaMetadata({ title: m.cur.title, artist: m.cur.artist || '', artwork: m.cur.art ? [{ src: m.cur.art }] : [] });
    next = m.next;
    if (next && E[1 - cur].id === next.id) E[1 - cur].norm = next.gain;
    preloadNext();
  }
  else if (m.op === 'eq') { eq = m; applyEq(); }
  else if (m.op === 'volume') { vol = m.v; if (master) master.gain.value = vol; else E.forEach(e => e.a.volume = vol); }
  else if (m.op === 'toggle') { const a = E[cur].a; if (ctx) ctx.resume(); a.paused ? a.play().catch(() => {}) : a.pause(); }
  else if (m.op === 'play') E[cur].a.play().catch(() => {});
  else if (m.op === 'pause') E.forEach(e => e.a.pause());
  else if (m.op === 'seek') E[cur].a.currentTime = m.t;
  else if (m.op === 'stop') { E.forEach(e => { e.a.pause(); e.a.removeAttribute('src'); e.id = null; }); next = null; }
}
"#;

#[derive(Deserialize)]
struct AudioState {
    t: f64,
    d: f64,
    paused: bool,
    ended: bool,
    action: Option<String>,
}

const SETTINGS_KEY: &str = "bitflow.music";
pub const FREQS: [&str; 10] = ["32", "64", "125", "250", "500", "1k", "2k", "4k", "8k", "16k"];
pub const PRESETS: &[(&str, [f64; 10])] = &[
    ("Plat", [0.0; 10]),
    ("Basses +", [6.0, 5.0, 4.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
    ("Aigus +", [0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 2.0, 4.0, 5.0, 6.0]),
    ("Voix", [-2.0, -2.0, -1.0, 1.0, 3.0, 4.0, 3.0, 1.0, 0.0, -1.0]),
    ("Rock", [4.0, 3.0, 1.0, -1.0, -1.0, 1.0, 2.0, 3.0, 4.0, 4.0]),
    ("Électro", [5.0, 4.0, 1.0, 0.0, -2.0, 1.0, 0.0, 1.0, 4.0, 5.0]),
    ("Classique", [3.0, 2.0, 1.0, 0.0, 0.0, 0.0, -1.0, -1.0, 1.0, 2.0]),
    ("Faible volume", [5.0, 3.0, 0.0, 0.0, -1.0, 0.0, 0.0, 0.0, 3.0, 4.0]),
];

/// Réglages audio, propres à l'appareil (un égaliseur pour casque ne convient pas à des enceintes).
#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct MusicSettings {
    /// off | auto | track | album
    pub norm: String,
    /// Durée du fondu enchaîné en secondes (0 = enchaînement sans blanc).
    pub crossfade: f64,
    /// Pas de fondu entre deux pistes consécutives d'un même album.
    pub smart: bool,
    pub eq: bool,
    pub preamp: f64,
    pub bands: [f64; 10],
}

impl Default for MusicSettings {
    fn default() -> Self {
        MusicSettings { norm: "auto".into(), crossfade: 5.0, smart: true, eq: false, preamp: 0.0, bands: [0.0; 10] }
    }
}

pub fn load_settings() -> MusicSettings {
    api::setting(SETTINGS_KEY).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// Référence ReplayGain 2.0 : -18 LUFS.
const TARGET_LUFS: f64 = -18.0;

/// Gain linéaire de normalisation, plafonné pour que le pic ne dépasse pas 0 dBFS.
/// `auto` : gain d'album quand la file est un seul album lu dans l'ordre, sinon gain de piste.
fn norm_gain(t: &Item, mode: &str, album_mode: bool) -> f64 {
    let album = mode == "album" || (mode == "auto" && album_mode);
    let (loudness, peak) = match mode {
        "off" => return 1.0,
        _ if album => (t.album_loudness.or(t.loudness), t.album_peak.or(t.peak)),
        _ => (t.loudness, t.peak),
    };
    let Some(l) = loudness else { return 1.0 };
    10f64.powf((TARGET_LUFS - l) / 20.0).min(1.0 / peak.unwrap_or(1.0).max(0.01))
}

/// Piste suivante selon répétition et aléatoire (`seed` fixé entre deux bascules pour rester stable).
fn next_index(q: &Queue, shuffle: bool, repeat: u8, seed: f64) -> Option<usize> {
    let n = q.tracks.len();
    match () {
        _ if n == 0 => None,
        _ if repeat == 2 => Some(q.index),
        _ if shuffle && n > 1 => Some((q.index + 1 + (seed * (n - 1) as f64) as usize) % n),
        _ if q.index + 1 < n => Some(q.index + 1),
        _ if repeat == 1 => Some(0),
        _ => None,
    }
}

/// Deux pistes qui se suivent sur le même disque du même album (concept, live, mix) : jamais de fondu.
fn consecutive(a: &Item, b: &Item) -> bool {
    a.parent_id == b.parent_id && a.season == b.season && a.episode.zip(b.episode).is_some_and(|(x, y)| y == x + 1)
}

thread_local! {
    /// Position de lecture, conservée quand la barre est démontée (lecteur vidéo plein écran).
    static LAST_T: std::cell::Cell<f64> = const { std::cell::Cell::new(0.0) };
}

const SLEEP: &[(&str, f64)] = &[("Désactivée", 0.0), ("15 min", 15.0), ("30 min", 30.0), ("1 h", 60.0), ("Fin de la piste", -1.0)];

/// Barre de lecture en bas de l'écran ; reste active pendant la navigation (montée dans `Shell`).
#[component]
pub fn MusicBar() -> Element {
    let mut queue = use_context::<Signal<Queue>>();
    let settings = use_context::<Signal<MusicSettings>>();
    let bridge = use_hook(|| document::eval(AUDIO_JS));
    let mut t = use_signal(|| 0.0);
    let mut d = use_signal(|| 0.0);
    let mut paused = use_signal(|| true);
    let mut shuffle = use_signal(|| false);
    let mut repeat = use_signal(|| 0u8); // 0 non, 1 toute la file, 2 la piste
    let mut seed = use_signal(js_sys::Math::random);
    let mut volume = use_signal(|| 1.0);
    let mut panel = use_signal(|| ""); // "", "queue", "eq"
    let mut scrub = use_signal(|| None::<f64>);
    let mut sleep = use_signal(|| 0usize);
    let mut sleep_at = use_signal(|| 0.0);
    let mut adding = use_signal(|| None::<Vec<i64>>);
    // Premier `set` après le montage : une file existante vient d'une barre démontée, on ne relance pas.
    let fresh = use_hook(|| std::rc::Rc::new(std::cell::Cell::new(queue.peek().current().is_some())));
    let send = move |c: serde_json::Value| {
        let _ = bridge.send(c);
    };

    // Réglages enregistrés sur l'appareil à chaque modification.
    use_effect(move || api::set_setting(SETTINGS_KEY, &serde_json::to_string(&*settings.read()).unwrap_or_default()));
    use_effect(move || {
        let s = settings.read();
        send(json!({ "op": "eq", "on": s.eq, "preamp": s.preamp, "bands": s.bands }));
    });

    // État voulu -> moteur JS : piste courante, suivante, gains et fondu.
    use_effect(move || {
        let q = queue.read();
        let s = settings.read();
        let Some(cur) = q.current() else { return };
        let album_mode = !shuffle() && q.tracks.iter().all(|x| x.parent_id == cur.parent_id);
        let next = next_index(&q, shuffle(), repeat(), seed()).map(|i| &q.tracks[i]).map(|n| {
            json!({ "id": n.id, "url": format!("/api/play/{}/audio", n.id), "gain": norm_gain(n, &s.norm, album_mode),
                    "fade": s.crossfade > 0.0 && !(s.smart && consecutive(cur, n)) })
        });
        let art = api::img(&cur.poster, "poster").map(|u| format!("{}{u}", api::origin()));
        let resume = fresh.replace(false).then(|| LAST_T.get());
        send(json!({ "op": "set", "xf": s.crossfade, "next": next, "resume": resume, "cur": {
            "id": cur.id, "url": format!("/api/play/{}/audio", cur.id), "gain": norm_gain(cur, &s.norm, album_mode),
            "title": cur.title, "artist": cur.artist, "art": art } }));
    });

    let mut go = move |i: Option<usize>| {
        if let Some(i) = i {
            queue.write().index = i;
            seed.set(js_sys::Math::random());
        }
    };
    // L'index est calculé avant `go` : lire et écrire la file dans la même instruction = double emprunt (panique).
    let mut skip = move || {
        let i = next_index(&queue.read(), shuffle(), if repeat() == 2 { 0 } else { repeat() }, seed());
        go(i)
    };
    let mut prev = move || {
        let i = queue.read().index;
        if t() > 3.0 || i == 0 { send(json!({ "op": "seek", "t": 0 })) } else { go(Some(i - 1)) }
    };
    let mut stop_if_sleep = move |track_end: bool| {
        let mode = SLEEP[sleep()].1;
        if (mode > 0.0 && js_sys::Date::now() >= sleep_at()) || (mode < 0.0 && track_end) {
            send(json!({ "op": "pause" }));
            sleep.set(0);
        }
    };

    use_future(move || async move {
        let mut b = bridge;
        while let Ok(s) = b.recv::<AudioState>().await {
            t.set(s.t);
            LAST_T.set(s.t);
            d.set(s.d);
            paused.set(s.paused);
            match s.action.as_deref() {
                // Le moteur a déjà basculé sur la piste préchargée : on suit.
                Some("advanced") => {
                    let i = next_index(&queue.read(), shuffle(), repeat(), seed());
                    go(i);
                    stop_if_sleep(true);
                }
                Some("next") => skip(),
                Some("prev") => prev(),
                Some("play") => send(json!({ "op": "play" })),
                Some("pause") => send(json!({ "op": "pause" })),
                _ => stop_if_sleep(s.ended),
            }
        }
    });

    let q = queue.read();
    let shown = scrub().unwrap_or(t());
    let pct = if d() > 0.0 { shown / d() * 100.0 } else { 0.0 };
    let mut toggle_panel = move |name: &'static str| panel.set(if panel() == name { "" } else { name });
    rsx! {
        audio { id: "bf-audio-a" }
        audio { id: "bf-audio-b" }
        // Hors de la barre : son flou d'arrière-plan confinerait la fenêtre à la barre.
        if let Some(items) = adding() {
            AddToPlaylist { items, on_close: move |_| adding.set(None) }
        }
        if let Some(cur) = q.current().cloned() {
            div { class: "musicbar",
                if panel() == "queue" {
                    div { class: "queue panel",
                        div { class: "queue-head",
                            h2 { "File de lecture" }
                            div {
                                button { class: "ghost", onclick: move |_| adding.set(Some(queue.read().tracks.iter().map(|t| t.id).collect())), "Enregistrer en playlist" }
                                button { class: "ghost", onclick: move |_| { send(json!({ "op": "stop" })); queue.set(Queue::default()); }, "Vider" }
                            }
                        }
                        ol {
                            for (i, tr) in q.tracks.iter().cloned().enumerate() {
                                li { key: "{i}-{tr.id}", class: if i == q.index { "playing" } else { "" },
                                    button { class: "q-title", onclick: move |_| go(Some(i)),
                                        span { "{tr.title}" }
                                        small { "{tr.artist.clone().unwrap_or_default()}" }
                                    }
                                    if i != q.index {
                                        button {
                                            class: "ghost",
                                            title: "Retirer",
                                            onclick: move |_| {
                                                let mut q = queue.write();
                                                q.tracks.remove(i);
                                                if i < q.index { q.index -= 1; }
                                            },
                                            {icon(ic::CLOSE)}
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if panel() == "eq" {
                    div { class: "queue panel eq-panel", Equalizer {} }
                }
                div { class: "mb-now",
                    if let Some(src) = api::img(&cur.poster, "poster") {
                        img { src, alt: "" }
                    }
                    div { class: "mb-text",
                        if let Some(album) = cur.parent_id {
                            Link { to: Route::Detail { id: album }, "{cur.title}" }
                        }
                        small { "{cur.artist.clone().unwrap_or_default()}" }
                    }
                }
                div { class: "mb-center",
                    div { class: "mb-buttons",
                        button { class: if shuffle() { "icon on" } else { "icon" }, title: "Aléatoire", onclick: move |_| shuffle.toggle(), {icon(ic::SHUFFLE)} }
                        button { class: "icon", title: "Précédent", onclick: move |_| prev(), {icon(ic::PREV)} }
                        button { class: "icon big", title: "Lecture/Pause", onclick: move |_| send(json!({ "op": "toggle" })),
                            {icon(if paused() { ic::PLAY } else { ic::PAUSE })}
                        }
                        button { class: "icon", title: "Suivant", onclick: move |_| skip(), {icon(ic::NEXT)} }
                        button { class: "icon", title: "Arrêter et fermer", onclick: move |_| { send(json!({ "op": "stop" })); queue.set(Queue::default()); }, {icon(ic::STOP)} }
                        button {
                            class: if repeat() > 0 { "icon on" } else { "icon" },
                            title: match repeat() { 0 => "Répéter : non", 1 => "Répéter : toute la file", _ => "Répéter : la piste" },
                            onclick: move |_| repeat.set((repeat() + 1) % 3),
                            {icon(if repeat() == 2 { ic::REPEAT_ONE } else { ic::REPEAT })}
                        }
                    }
                    div { class: "mb-seek",
                        small { "{clock(shown)}" }
                        input {
                            class: "seek",
                            r#type: "range",
                            min: "0",
                            max: "{d}",
                            step: "0.5",
                            value: "{shown}",
                            style: "--pos:{pct:.2}%;--buf:{pct:.2}%",
                            "aria-label": "Position",
                            oninput: move |e| scrub.set(e.value().parse().ok()),
                            onchange: move |e| {
                                if let Ok(v) = e.value().parse::<f64>() { send(json!({ "op": "seek", "t": v })) }
                                scrub.set(None);
                            },
                        }
                        small { "{clock(d())}" }
                    }
                }
                div { class: "mb-right",
                    button {
                        class: if sleep() > 0 { "icon on sleep" } else { "icon sleep" },
                        title: "Minuterie : {SLEEP[sleep()].0}",
                        onclick: move |_| {
                            let n = (sleep() + 1) % SLEEP.len();
                            sleep.set(n);
                            sleep_at.set(js_sys::Date::now() + SLEEP[n].1 * 60_000.0);
                        },
                        {icon(ic::SLEEP)}
                        if sleep() > 0 { small { "{SLEEP[sleep()].0}" } }
                    }
                    button { class: if panel() == "eq" || settings.read().eq { "icon on" } else { "icon" }, title: "Égaliseur", onclick: move |_| toggle_panel("eq"), {icon(ic::EQ)} }
                    input { class: "volume", r#type: "range", min: "0", max: "1", step: "0.05", value: "{volume}", "aria-label": "Volume", style: "--pos:{volume() * 100.0:.0}%;--buf:0%",
                        oninput: move |e| {
                            let v: f64 = e.value().parse().unwrap_or(1.0);
                            volume.set(v);
                            send(json!({ "op": "volume", "v": v }));
                        },
                    }
                    button { class: if panel() == "queue" { "icon on" } else { "icon" }, title: "File de lecture", onclick: move |_| toggle_panel("queue"), {icon(ic::QUEUE)} }
                }
            }
        }
    }
}

/// Égaliseur 10 bandes (-12 à +12 dB), préréglages et pré-ampli. Utilisé dans la barre et dans les paramètres.
#[component]
pub fn Equalizer() -> Element {
    let mut s = use_context::<Signal<MusicSettings>>();
    let cur = s.read().clone();
    let preset = PRESETS.iter().find(|(_, b)| *b == cur.bands).map(|(n, _)| *n).unwrap_or("Personnalisé");
    rsx! {
        div { class: "eq",
            div { class: "eq-head",
                label { class: "switch",
                    input { r#type: "checkbox", checked: cur.eq, onchange: move |e| s.write().eq = e.checked() }
                    " Égaliseur"
                }
                select {
                    "aria-label": "Préréglage",
                    onchange: move |e| {
                        if let Some((_, b)) = PRESETS.iter().find(|(n, _)| *n == e.value()) {
                            let mut w = s.write();
                            w.bands = *b;
                            w.eq = true;
                        }
                    },
                    if preset == "Personnalisé" { option { selected: true, "Personnalisé" } }
                    for (name, _) in PRESETS {
                        option { value: *name, selected: *name == preset, "{name}" }
                    }
                }
            }
            div { class: "eq-bands",
                div { class: "eq-band",
                    small { "{cur.preamp:+.0}" }
                    input { r#type: "range", min: "-12", max: "6", step: "1", value: "{cur.preamp}", "aria-label": "Pré-ampli",
                        oninput: move |e| { let mut w = s.write(); w.preamp = e.value().parse().unwrap_or(0.0); w.eq = true; },
                    }
                    small { "Pré" }
                }
                for (i, f) in FREQS.iter().enumerate() {
                    div { class: "eq-band",
                        small { "{cur.bands[i]:+.0}" }
                        input { r#type: "range", min: "-12", max: "12", step: "1", value: "{cur.bands[i]}", "aria-label": "{f} Hz",
                            // Toucher une bande active l'égaliseur, comme dans Plexamp.
                            oninput: move |e| { let mut w = s.write(); w.bands[i] = e.value().parse().unwrap_or(0.0); w.eq = true; },
                        }
                        small { "{f}" }
                    }
                }
            }
        }
    }
}

/// Section « Musique » des paramètres.
#[component]
pub fn MusicSettingsPanel() -> Element {
    let mut s = use_context::<Signal<MusicSettings>>();
    let cur = s.read().clone();
    rsx! {
        section { class: "panel wide",
            h2 { "Musique " small { class: "muted", "réglages de cet appareil" } }
            div { class: "music-settings",
                div {
                    form { onsubmit: move |e| e.prevent_default(),
                        div { class: "field",
                            label { r#for: "norm", "Normalisation du volume" }
                            select { id: "norm", onchange: move |e| s.write().norm = e.value(),
                                for (k, label) in [("auto", "Automatique"), ("track", "Par piste"), ("album", "Par album"), ("off", "Désactivée")] {
                                    option { value: k, selected: cur.norm == k, "{label}" }
                                }
                            }
                        }
                        div { class: "field",
                            label { r#for: "xf", "Fondu enchaîné"
                                small { if cur.crossfade > 0.0 { "{cur.crossfade:.0} s" } else { "désactivé : enchaînement sans blanc" } }
                            }
                            input { id: "xf", r#type: "range", min: "0", max: "12", step: "1", value: "{cur.crossfade}",
                                style: "--pos:{cur.crossfade / 12.0 * 100.0:.0}%;--buf:0%",
                                class: "volume",
                                oninput: move |e| s.write().crossfade = e.value().parse().unwrap_or(0.0),
                            }
                        }
                        div { class: "field",
                            label { r#for: "smart", "Fondu intelligent"
                                small { "pas de fondu entre deux pistes qui se suivent sur un album" }
                            }
                            input { id: "smart", class: "toggle", r#type: "checkbox", checked: cur.smart, onchange: move |e| s.write().smart = e.checked() }
                        }
                    }
                    p { class: "muted small",
                        "Automatique : gain d'album quand on écoute un album en entier, sinon gain de piste. "
                        "La normalisation vise -18 LUFS (référence ReplayGain 2.0) et utilise les tags ReplayGain s'ils existent, "
                        "sinon l'analyse faite par le serveur. Le gain est plafonné pour ne jamais saturer."
                    }
                }
                Equalizer {}
            }
        }
    }
}
