//! Musique : vues album/artiste et barre de lecture persistante avec file d'attente.
use bitflow_shared::Item;
use dioxus::prelude::*;
use serde::Deserialize;
use serde_json::json;

use crate::{Card, Route, api, clock};

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

fn shuffled(mut v: Vec<Item>) -> Vec<Item> {
    // Fisher-Yates avec l'aléa du navigateur.
    for i in (1..v.len()).rev() {
        v.swap(i, (js_sys::Math::random() * (i + 1) as f64) as usize);
    }
    v
}

#[component]
pub fn AlbumView(item: Item) -> Element {
    let mut queue = use_context::<Signal<Queue>>();
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
                    button { class: "btn primary", onclick: move |_| queue.set(Queue { tracks: l1.clone(), index: 0 }), "▶ Lecture" }
                    button { class: "btn", onclick: move |_| queue.set(Queue { tracks: shuffled(l2.clone()), index: 0 }), "⤮ Aléatoire" }
                }
            }
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
                    button {
                        class: "ghost",
                        title: "Ajouter à la file",
                        onclick: {
                            let t = t.clone();
                            move |_| queue.write().tracks.push(t.clone())
                        },
                        "+"
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

const AUDIO_JS: &str = r#"
let a; while (!(a = document.getElementById('bf-audio'))) await new Promise(r => setTimeout(r, 50));
const push = action => dioxus.send({ t: a.currentTime || 0, d: isFinite(a.duration) ? a.duration : 0, paused: a.paused, ended: a.ended, action: action || null });
['timeupdate', 'play', 'pause', 'ended', 'loadedmetadata'].forEach(e => a.addEventListener(e, () => push()));
const ms = navigator.mediaSession;
if (ms) {
  [['play', 'play'], ['pause', 'pause'], ['previoustrack', 'prev'], ['nexttrack', 'next']].forEach(([k, v]) => { try { ms.setActionHandler(k, () => push(v)); } catch (e) {} });
  try { ms.setActionHandler('seekto', d => { a.currentTime = d.seekTime; }); } catch (e) {}
}
while (true) {
  const c = await dioxus.recv();
  if (c.op === 'load') {
    a.src = c.url; a.play().catch(() => {});
    if (ms && window.MediaMetadata) ms.metadata = new MediaMetadata({ title: c.title, artist: c.artist, artwork: c.art ? [{ src: c.art }] : [] });
  }
  else if (c.op === 'toggle') a.paused ? a.play().catch(() => {}) : a.pause();
  else if (c.op === 'play') a.play().catch(() => {});
  else if (c.op === 'pause') a.pause();
  else if (c.op === 'seek') a.currentTime = c.t;
  else if (c.op === 'volume') a.volume = c.v;
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

/// Barre de lecture en bas de l'écran ; reste active pendant la navigation (montée dans `Shell`).
#[component]
pub fn MusicBar() -> Element {
    let mut queue = use_context::<Signal<Queue>>();
    let bridge = use_hook(|| document::eval(AUDIO_JS));
    let mut t = use_signal(|| 0.0);
    let mut d = use_signal(|| 0.0);
    let mut paused = use_signal(|| true);
    let mut shuffle = use_signal(|| false);
    let mut repeat = use_signal(|| 0u8); // 0 non, 1 toute la file, 2 la piste
    let mut volume = use_signal(|| 1.0);
    let mut show_queue = use_signal(|| false);
    let mut scrub = use_signal(|| None::<f64>);
    let mut loaded = use_signal(|| None::<i64>);
    let send = move |c: serde_json::Value| {
        let _ = bridge.send(c);
    };

    // Charge la piste courante quand elle change.
    use_effect(move || {
        let q = queue.read();
        let Some(tr) = q.current() else { return };
        if *loaded.peek() == Some(tr.id) {
            return;
        }
        loaded.set(Some(tr.id));
        let art = api::img(&tr.poster, "poster").map(|u| format!("{}{u}", api::origin()));
        send(json!({ "op": "load", "url": format!("/api/play/{}/audio", tr.id), "title": tr.title, "artist": tr.artist, "art": art }));
    });

    let mut step = move |delta: i64| {
        let mut q = queue.write();
        let n = q.tracks.len();
        if n == 0 {
            return;
        }
        if shuffle() && delta > 0 && n > 1 {
            // Jamais la même piste deux fois de suite.
            q.index = (q.index + 1 + (js_sys::Math::random() * (n - 1) as f64) as usize) % n;
            return;
        }
        let i = q.index as i64 + delta;
        if i >= n as i64 {
            if repeat() == 1 {
                q.index = 0;
            }
        } else if i >= 0 {
            q.index = i as usize;
        }
    };
    let mut prev = move || {
        if t() > 3.0 { send(json!({ "op": "seek", "t": 0 })) } else { step(-1) }
    };

    use_future(move || async move {
        let mut b = bridge;
        while let Ok(s) = b.recv::<AudioState>().await {
            t.set(s.t);
            d.set(s.d);
            paused.set(s.paused);
            match s.action.as_deref() {
                Some("next") => step(1),
                Some("prev") => prev(),
                Some("play") => send(json!({ "op": "play" })),
                Some("pause") => send(json!({ "op": "pause" })),
                _ => {}
            }
            if s.ended {
                if repeat() == 2 {
                    send(json!({ "op": "seek", "t": 0 }));
                    send(json!({ "op": "play" }));
                } else {
                    step(1);
                }
            }
        }
    });

    let q = queue.read();
    let shown = scrub().unwrap_or(t());
    let pct = if d() > 0.0 { shown / d() * 100.0 } else { 0.0 };
    rsx! {
        audio { id: "bf-audio", "preload": "auto" }
        if let Some(cur) = q.current().cloned() {
            div { class: "musicbar",
                if show_queue() {
                    div { class: "queue panel",
                        div { class: "queue-head",
                            h2 { "File de lecture" }
                            button { class: "ghost", onclick: move |_| queue.set(Queue::default()), "Vider" }
                        }
                        ol {
                            for (i, tr) in q.tracks.iter().cloned().enumerate() {
                                li { key: "{i}-{tr.id}", class: if i == q.index { "playing" } else { "" },
                                    button { class: "q-title", onclick: move |_| queue.write().index = i,
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
                                            "×"
                                        }
                                    }
                                }
                            }
                        }
                    }
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
                        button { class: if shuffle() { "icon on" } else { "icon" }, title: "Aléatoire", onclick: move |_| shuffle.toggle(), "⤮" }
                        button { class: "icon", title: "Précédent", onclick: move |_| prev(), "⏮" }
                        button { class: "icon big", title: "Lecture/Pause", onclick: move |_| send(json!({ "op": "toggle" })),
                            if paused() { "▶" } else { "⏸" }
                        }
                        button { class: "icon", title: "Suivant", onclick: move |_| step(1), "⏭" }
                        button {
                            class: if repeat() > 0 { "icon on" } else { "icon" },
                            title: match repeat() { 0 => "Répéter : non", 1 => "Répéter : toute la file", _ => "Répéter : la piste" },
                            onclick: move |_| repeat.set((repeat() + 1) % 3),
                            if repeat() == 2 { "🔂" } else { "🔁" }
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
                    input { class: "volume", r#type: "range", min: "0", max: "1", step: "0.05", value: "{volume}", "aria-label": "Volume",
                        oninput: move |e| {
                            let v: f64 = e.value().parse().unwrap_or(1.0);
                            volume.set(v);
                            send(json!({ "op": "volume", "v": v }));
                        },
                    }
                    button { class: if show_queue() { "icon on" } else { "icon" }, title: "File de lecture", onclick: move |_| show_queue.toggle(), "☰" }
                }
            }
        }
    }
}
