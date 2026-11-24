//! Lecteur : commandes personnalisées en Rust, pilotage de <video> et hls.js via un pont JS minimal.
use bitflow_shared::{Item, MediaInfo, Progress};
use dioxus::{core::spawn_forever, prelude::*};
use serde::Deserialize;
use serde_json::json;

use crate::{Route, api, clock};

/// Reçoit des commandes `{op, ...}` de Rust et renvoie l'état de la vidéo à chaque évènement.
const BRIDGE: &str = r#"
let v; while (!(v = document.getElementById('bf-video'))) await new Promise(r => setTimeout(r, 16));
const p = document.getElementById('bf-player'), box = document.getElementById('bf-subs');
let hls = null, idle;
const wake = () => { p.classList.remove('idle'); clearTimeout(idle);
  idle = setTimeout(() => { if (!v.paused && !p.querySelector('.menu')) p.classList.add('idle'); }, 3000); };
['mousemove', 'touchstart', 'keydown'].forEach(e => p.addEventListener(e, wake));
v.addEventListener('pause', wake);
const push = () => dioxus.send({ t: v.currentTime || 0, paused: v.paused, ended: v.ended,
  buf: v.buffered.length ? v.buffered.end(v.buffered.length - 1) : 0 });
['timeupdate', 'play', 'pause', 'ended', 'progress', 'seeked'].forEach(e => v.addEventListener(e, push));
// Sous-titres affichés hors de la vidéo : ils restent visibles quand l'image est agrandie.
const setSub = url => {
  v.querySelectorAll('track').forEach(t => t.remove());
  box.replaceChildren();
  if (!url) return;
  const t = document.createElement('track');
  t.kind = 'subtitles'; t.src = url;
  v.appendChild(t); t.track.mode = 'hidden';
  t.track.addEventListener('cuechange', () => box.replaceChildren(...[...(t.track.activeCues || [])]
    .map(c => { const d = document.createElement('span'); d.append(c.getCueAsHTML()); return d; })));
};
// Bandes noires incrustées dans le fichier : mesurées sur des images échantillons, puis la partie utile
// est agrandie au maximum sans déformation (fill : elle couvre l'écran, quitte à rogner).
const cv = document.createElement('canvas'), cx = cv.getContext('2d', { willReadFrequently: true });
const samples = [];
let crop = [0, 0, 0, 0], fill = false;
const fit = () => {
  const vw = v.videoWidth, vh = v.videoHeight, W = p.clientWidth, H = p.clientHeight;
  if (!vw || !W) return;
  const [t, r, b, l] = crop, aw = vw * (1 - l - r), ah = vh * (1 - t - b);
  const k0 = Math.min(W / vw, H / vh), k = (fill ? Math.max : Math.min)(W / aw, H / ah);
  v.style.transform = `translate(${(r - l) / 2 * vw * k}px, ${(b - t) / 2 * vh * k}px) scale(${k / k0})`;
};
const measure = () => {
  if (!document.contains(v)) { clearInterval(timer); ro.disconnect(); return; }
  if (!v.videoWidth || v.paused || v.seeking) return;
  const w = 160, h = Math.round(w * v.videoHeight / v.videoWidth);
  cv.width = w; cv.height = h;
  try { cx.drawImage(v, 0, 0, w, h); } catch { return; }
  const d = cx.getImageData(0, 0, w, h).data;
  const lum = (x, y) => { const i = (y * w + x) * 4; return (2 * d[i] + 5 * d[i + 1] + d[i + 2]) >> 3; };
  let sum = 0;
  for (let i = 0; i < d.length; i += 4) sum += d[i + 1];
  if (sum / (w * h) < 30) return; // scène sombre : impossible de distinguer les bandes
  // Proportion de lignes (ou colonnes) noires depuis un bord, limitée à 30 %.
  const edge = (len, n, at) => { let i = 0; while (i < len * .3 && [...Array(n).keys()].every(k => at(i, k) < 40)) i++; return i / len; };
  samples.push([edge(h, w, (i, k) => lum(k, i)), edge(w, h, (i, k) => lum(w - 1 - i, k)),
                edge(h, w, (i, k) => lum(k, h - 1 - i)), edge(w, h, (i, k) => lum(i, k))]);
  if (samples.length > 5) samples.shift();
  if (samples.length < 5) return;
  // 2e plus petite des 5 dernières mesures par bord : un plan sombre isolé ne fausse rien. Ensuite les bandes
  // ne peuvent que rétrécir : l'image ne zoome jamais davantage en cours de film.
  const next = [0, 1, 2, 3].map(j => { const x = samples.map(s => s[j]).sort((a, b) => a - b)[1];
    return Math.min(x < .01 ? 0 : x, measured ? crop[j] : 1); });
  measured = true;
  if (next.some((x, j) => Math.abs(x - crop[j]) > .005)) { crop = next; fit(); }
};
let measured = false;
const timer = setInterval(measure, 1000);
const ro = new ResizeObserver(fit); ro.observe(p);
v.addEventListener('loadedmetadata', fit);
while (true) {
  const c = await dioxus.recv();
  if (c.op === 'load') {
    if (hls) { hls.destroy(); hls = null; }
    v.addEventListener('loadedmetadata', () => { if (c.seek) v.currentTime = c.seek; v.play().catch(() => {}); }, { once: true });
    if (c.hls && window.Hls && Hls.isSupported()) { hls = new Hls({ startPosition: 0 }); hls.loadSource(c.url); hls.attachMedia(v); }
    else v.src = c.url;
    setSub(c.sub);
  }
  else if (c.op === 'sub') setSub(c.url);
  else if (c.op === 'fill') { fill = c.on; fit(); }
  else if (c.op === 'toggle') v.paused ? v.play().catch(() => {}) : v.pause();
  else if (c.op === 'seek') v.currentTime = c.t;
  else if (c.op === 'volume') { v.volume = c.v; v.muted = c.v === 0; }
  else if (c.op === 'rate') v.playbackRate = c.r;
  else if (c.op === 'fs') {
    if (document.fullscreenElement) document.exitFullscreen();
    else if (p.requestFullscreen) p.requestFullscreen();
    else if (v.webkitEnterFullscreen) v.webkitEnterFullscreen();
  }
  else if (c.op === 'pip') {
    if (document.pictureInPictureElement) document.exitPictureInPicture();
    else if (v.requestPictureInPicture) v.requestPictureInPicture().catch(() => {});
  }
}
"#;

#[derive(Deserialize)]
struct State {
    t: f64,
    paused: bool,
    ended: bool,
    buf: f64,
}

const SPEEDS: &[f64] = &[0.5, 0.75, 1.0, 1.25, 1.5, 2.0];
const QUALITIES: &[(&str, Option<u32>)] = &[("Originale", None), ("1080p", Some(1080)), ("720p", Some(720)), ("480p", Some(480))];

#[component]
pub fn Play(id: i64) -> Element {
    let data = use_resource(use_reactive!(|id| async move {
        let item = api::get::<Item>(&format!("/api/items/{id}")).await?;
        let info = api::get::<MediaInfo>(&format!("/api/play/{id}/info")).await?;
        Ok::<_, String>((item, info))
    }));
    match &*data.read_unchecked() {
        Some(Ok((item, info))) => rsx! { Player { key: "{id}", item: item.clone(), info: info.clone() } },
        Some(Err(e)) => rsx! { p { class: "error", "{e}" } },
        None => rsx! { div { class: "loading" } },
    }
}

#[component]
fn Player(item: Item, info: MediaInfo) -> Element {
    let id = item.id;
    let dur = info.duration;
    let next = info.next;
    // Piste « par défaut » au sens du fichier : la lecture directe la joue sans transcodage.
    let default_audio = info.audio.iter().position(|a| a.default).unwrap_or(0);
    let (audio_pick, sub_pick) = (info.audio_pick, info.sub_pick);
    let info = use_signal(|| info);

    let mut t = use_signal(|| 0.0); // temps de la vidéo
    let mut start = use_signal(|| 0.0); // décalage de la session HLS
    let mut buf = use_signal(|| 0.0);
    let mut paused = use_signal(|| true);
    let mut hls = use_signal(|| false);
    let mut audio = use_signal(|| audio_pick);
    let mut sub = use_signal(|| sub_pick);
    let mut height = use_signal(|| None::<u32>);
    let mut volume = use_signal(|| 1.0);
    let mut rate = use_signal(|| 1.0);
    let mut scrub = use_signal(|| None::<f64>);
    let mut saved = use_signal(|| 0.0);
    let mut error = use_signal(|| None::<String>);
    // Remplir l'écran (rogne les bords) au lieu d'ajuster au plus grand sans rogner ; mémorisé par appareil.
    let mut fill = use_signal(|| api::setting("bitflow.fill").as_deref() == Some("1"));
    let mut menu = use_signal(|| false);
    let bridge = use_hook(|| document::eval(BRIDGE));

    let now = move || start() + t();
    let send = move |c: serde_json::Value| {
        let _ = bridge.send(c);
    };
    let mut toggle_fill = move || {
        fill.toggle();
        api::set_setting("bitflow.fill", if fill() { "1" } else { "0" });
        send(json!({ "op": "fill", "on": fill() }));
    };
    let burn = move || sub().filter(|s| info.read().subs.get(*s).is_some_and(|x| x.burn));
    let sub_url = move |offset: f64| {
        sub().filter(|_| burn().is_none()).map(|s| format!("/api/play/{id}/sub/{s}?start={offset}"))
    };

    // Lecture directe si possible ; sinon (ou piste/qualité/incrustation demandée) session HLS à `at`.
    let mut load = move |at: f64| {
        let want_hls = !info.read().direct || audio() != default_audio || height().is_some() || burn().is_some();
        hls.set(want_hls);
        spawn(async move {
            if !want_hls {
                start.set(0.0);
                send(json!({ "op": "load", "url": format!("/api/play/{id}/direct"), "hls": false, "seek": at, "sub": sub_url(0.0) }));
                return;
            }
            let mut q = format!("/api/play/{id}/hls?start={at}&audio={}", audio());
            if let Some(h) = height() {
                q += &format!("&height={h}");
            }
            if let Some(b) = burn() {
                q += &format!("&burn={b}");
            }
            match api::post::<String>(&q, &()).await {
                Ok(url) => {
                    start.set(at);
                    t.set(0.0);
                    send(json!({ "op": "load", "url": url, "hls": true, "seek": 0, "sub": sub_url(at) }));
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };

    let mut seek = move |to: f64| {
        let to = to.clamp(0.0, dur.max(0.0));
        let rel = to - start();
        // En HLS, au-delà de ce qui est déjà transcodé : nouvelle session à partir de `to`.
        if !hls() || (0.0..=buf() + 30.0).contains(&rel) {
            send(json!({ "op": "seek", "t": if hls() { rel } else { to } }));
        } else {
            load(to);
        }
    };

    let mut save = move || {
        let position = now();
        let watched = dur > 0.0 && position >= dur * 0.92;
        saved.set(position);
        spawn_forever(async move {
            let _ = api::post::<()>(&format!("/api/items/{id}/progress"), &Progress { position, watched }).await;
        });
    };

    let mut set_volume = move |v: f64| {
        let v = v.clamp(0.0, 1.0);
        volume.set(v);
        send(json!({ "op": "volume", "v": v }));
    };

    let resume = if !item.watched && item.position > 0.0 && item.position < dur * 0.95 { item.position } else { 0.0 };
    use_hook(move || {
        send(json!({ "op": "fill", "on": fill() }));
        load(resume);
    });
    use_drop(move || {
        spawn_forever(async { let _ = api::delete("/api/hls").await; });
    });

    use_future(move || async move {
        let mut b = bridge;
        while let Ok(s) = b.recv::<State>().await {
            t.set(s.t);
            buf.set(s.buf);
            let was_playing = !paused();
            paused.set(s.paused);
            if s.ended {
                save();
                match next {
                    Some(n) => { navigator().replace(Route::Play { id: n }); }
                    None => navigator().go_back(),
                };
            } else if (s.paused && was_playing) || (now() - saved()).abs() >= 10.0 {
                save();
            }
        }
    });

    let mut set_sub = move |v: Option<usize>| {
        let was_burn = burn().is_some();
        sub.set(v);
        // Sous-titre texte : simple piste <track>, sans recharger la vidéo.
        if burn().is_none() && !was_burn {
            send(json!({ "op": "sub", "url": sub_url(start()) }));
        } else {
            load(now());
        }
    };
    let toggle = move || send(json!({ "op": "toggle" }));
    let shown = scrub().unwrap_or(now());
    let pct = |x: f64| if dur > 0.0 { (x / dur * 100.0).clamp(0.0, 100.0) } else { 0.0 };
    let seek_style = format!("--pos:{:.2}%;--buf:{:.2}%", pct(shown), pct(start() + buf()));
    let title = match (item.season, item.episode) {
        (Some(s), Some(e)) => format!("S{s:02}E{e:02} · {}", item.title),
        _ => item.title.clone(),
    };
    let info_r = info.read();

    rsx! {
        div {
            id: "bf-player",
            class: "player",
            tabindex: 0,
            autofocus: true,
            onkeydown: move |e| {
                match e.key() {
                    Key::Character(c) if c == " " || c == "k" => toggle(),
                    Key::Character(c) if c == "f" => send(json!({ "op": "fs" })),
                    Key::Character(c) if c == "z" => toggle_fill(),
                    Key::Character(c) if c == "m" => set_volume(if volume() > 0.0 { 0.0 } else { 1.0 }),
                    Key::Escape if menu() => menu.set(false),
                    Key::ArrowLeft => seek(now() - 10.0),
                    Key::ArrowRight => seek(now() + 10.0),
                    Key::ArrowUp => set_volume(volume() + 0.1),
                    Key::ArrowDown => set_volume(volume() - 0.1),
                    _ => return,
                }
                e.prevent_default();
            },
            video {
                id: "bf-video",
                "playsinline": "true",
                onclick: move |_| if menu() { menu.set(false) } else { toggle() },
                ondoubleclick: move |_| send(json!({ "op": "fs" })),
            }
            div { id: "bf-subs", class: "subs" }
            div { class: "top-bar",
                button { class: "icon", title: "Retour", onclick: move |_| { save(); navigator().go_back(); }, {icon(BACK)} }
                div { class: "titles",
                    h2 { "{title}" }
                    if let Some(y) = item.year {
                        small { "{y}" }
                    }
                }
            }
            if let Some(e) = error() {
                div { class: "center error", "{e}" }
            } else if paused() {
                button { class: "center-play", title: "Lecture", onclick: move |_| toggle(), {icon(PLAY)} }
            }
            if let Some(n) = next {
                if dur > 0.0 && dur - now() < 30.0 {
                    button { class: "up-next btn primary", onclick: move |_| { save(); navigator().replace(Route::Play { id: n }); }, "Épisode suivant ▶" }
                }
            }
            if menu() {
                div { class: "menu",
                    div { class: "menu-col",
                        h4 { "Vitesse" }
                        for s in SPEEDS.iter().copied() {
                            button {
                                class: if s == rate() { "opt on" } else { "opt" },
                                onclick: move |_| {
                                    rate.set(s);
                                    send(json!({ "op": "rate", "r": s }));
                                },
                                if s == 1.0 { "Normale" } else { "{s}×" }
                            }
                        }
                    }
                    if info_r.audio.len() > 1 {
                        div { class: "menu-col",
                            h4 { "Audio" }
                            for a in info_r.audio.iter() {
                                button {
                                    class: if a.index == audio() { "opt on" } else { "opt" },
                                    onclick: {
                                        let i = a.index;
                                        move |_| {
                                            audio.set(i);
                                            load(now());
                                        }
                                    },
                                    "{a.label}"
                                }
                            }
                        }
                    }
                    if !info_r.subs.is_empty() {
                        div { class: "menu-col",
                            h4 { "Sous-titres" }
                            button { class: if sub().is_none() { "opt on" } else { "opt" }, onclick: move |_| set_sub(None), "Aucun" }
                            for s in info_r.subs.iter() {
                                button {
                                    class: if sub() == Some(s.index) { "opt on" } else { "opt" },
                                    onclick: {
                                        let i = s.index;
                                        move |_| set_sub(Some(i))
                                    },
                                    "{s.label}"
                                }
                            }
                        }
                    }
                    div { class: "menu-col",
                        h4 { "Qualité" }
                        for (label, h) in QUALITIES.iter().copied() {
                            button {
                                class: if h == height() { "opt on" } else { "opt" },
                                onclick: move |_| {
                                    height.set(h);
                                    load(now());
                                },
                                "{label}"
                            }
                        }
                    }
                }
            }
            div { class: "controls",
                div { class: "seek-row",
                    span { class: "time", "{clock(shown)}" }
                    input {
                        class: "seek",
                        r#type: "range",
                        min: "0",
                        max: "{dur}",
                        step: "0.1",
                        value: "{shown}",
                        style: "{seek_style}",
                        "aria-label": "Position",
                        oninput: move |e| scrub.set(e.value().parse().ok()),
                        onchange: move |e| {
                            if let Ok(v) = e.value().parse() { seek(v) }
                            scrub.set(None);
                        },
                    }
                    span { class: "time", "-{clock((dur - shown).max(0.0))}" }
                }
                div { class: "bar",
                    button { class: "icon big", title: "Lecture/Pause (espace)", onclick: move |_| toggle(), {icon(if paused() { PLAY } else { PAUSE })} }
                    button { class: "icon", title: "-10 s (←)", onclick: move |_| seek(now() - 10.0), {icon(REWIND)} }
                    button { class: "icon", title: "+10 s (→)", onclick: move |_| seek(now() + 10.0), {icon(FORWARD)} }
                    div { class: "vol",
                        button { class: "icon", title: "Muet (m)", onclick: move |_| set_volume(if volume() > 0.0 { 0.0 } else { 1.0 }),
                            {icon(if volume() > 0.0 { VOLUME } else { MUTE })}
                        }
                        input { class: "volume", r#type: "range", min: "0", max: "1", step: "0.05", value: "{volume}", "aria-label": "Volume",
                            style: "--pos:{volume() * 100.0:.0}%;--buf:0%",
                            oninput: move |e| set_volume(e.value().parse().unwrap_or(1.0)),
                        }
                    }
                    div { class: "spacer" }
                    button {
                        class: if menu() { "icon on" } else { "icon" },
                        title: "Vitesse, audio, sous-titres, qualité",
                        onclick: move |_| menu.toggle(),
                        {icon(SETTINGS)}
                    }
                    button {
                        class: if fill() { "icon on" } else { "icon" },
                        title: if fill() { "Afficher l'image entière (z)" } else { "Remplir l'écran, bords rognés (z)" },
                        onclick: move |_| toggle_fill(),
                        {icon(FILL)}
                    }
                    button { class: "icon", title: "Image dans l'image", onclick: move |_| send(json!({ "op": "pip" })), {icon(PIP)} }
                    button { class: "icon", title: "Plein écran (f)", onclick: move |_| send(json!({ "op": "fs" })), {icon(FULLSCREEN)} }
                }
            }
        }
    }
}

fn icon(d: &'static str) -> Element {
    rsx! {
        svg { view_box: "0 0 24 24", width: "26", height: "26", "aria-hidden": "true",
            path { d, fill: "currentColor" }
        }
    }
}

// Icônes Material Design (Apache 2.0).
const PLAY: &str = "M8 5v14l11-7z";
const PAUSE: &str = "M6 19h4V5H6v14zm8-14v14h4V5h-4z";
const REWIND: &str = "M11 18V6l-8.5 6 8.5 6zm.5-6l8.5 6V6l-8.5 6z";
const FORWARD: &str = "M4 18l8.5-6L4 6v12zm9-12v12l8.5-6L13 6z";
const VOLUME: &str = "M3 9v6h4l5 5V4L7 9H3zm13.5 3c0-1.77-1.02-3.29-2.5-4.03v8.05c1.48-.73 2.5-2.25 2.5-4.02zM14 3.23v2.06c2.89.86 5 3.54 5 6.71s-2.11 5.85-5 6.71v2.06c4.01-.91 7-4.49 7-8.77s-2.99-7.86-7-8.77z";
const MUTE: &str = "M16.5 12c0-1.77-1.02-3.29-2.5-4.03v2.21l2.45 2.45c.03-.2.05-.41.05-.63zm2.5 0c0 .94-.2 1.82-.54 2.64l1.51 1.51C20.63 14.91 21 13.5 21 12c0-4.28-2.99-7.86-7-8.77v2.06c2.89.86 5 3.54 5 6.71zM4.27 3L3 4.27 7.73 9H3v6h4l5 5v-6.73l4.25 4.25c-.67.52-1.42.93-2.25 1.18v2.06c1.38-.31 2.63-.95 3.69-1.81L19.73 21 21 19.73l-9-9L4.27 3zM12 4L9.91 6.09 12 8.18V4z";
const PIP: &str = "M19 7h-8v6h8V7zm2-4H3c-1.1 0-2 .9-2 2v14c0 1.1.9 1.98 2 1.98h18c1.1 0 2-.88 2-1.98V5c0-1.1-.9-2-2-2zm0 16.01H3V4.98h18v14.03z";
const FULLSCREEN: &str = "M7 14H5v5h5v-2H7v-3zm-2-4h2V7h3V5H5v5zm12 7h-3v2h5v-5h-2v3zM14 5v2h3v3h2V5h-5z";
const FILL: &str = "M15 3l2.3 2.3-2.89 2.87 1.42 1.42L18.7 6.7 21 9V3h-6zM3 9l2.3-2.3 2.87 2.89 1.42-1.42L6.7 5.3 9 3H3v6zm6 12l-2.3-2.3 2.89-2.87-1.42-1.42L5.3 17.3 3 15v6h6zm12-6l-2.3 2.3-2.87-2.89-1.42 1.42 2.89 2.87L15 21h6v-6z";
const SETTINGS: &str = "M19.14 12.94c.04-.3.06-.61.06-.94 0-.32-.02-.64-.07-.94l2.03-1.58c.18-.14.23-.41.12-.61l-1.92-3.32c-.12-.22-.37-.29-.59-.22l-2.39.96c-.5-.38-1.03-.7-1.62-.94l-.36-2.54c-.04-.24-.24-.41-.48-.41h-3.84c-.24 0-.43.17-.47.41l-.36 2.54c-.59.24-1.13.57-1.62.94l-2.39-.96c-.22-.08-.47 0-.59.22L2.74 8.87c-.12.21-.08.47.12.61l2.03 1.58c-.05.3-.09.63-.09.94s.02.64.07.94l-2.03 1.58c-.18.14-.23.41-.12.61l1.92 3.32c.12.22.37.29.59.22l2.39-.96c.5.38 1.03.7 1.62.94l.36 2.54c.05.24.24.41.48.41h3.84c.24 0 .44-.17.47-.41l.36-2.54c.59-.24 1.13-.56 1.62-.94l2.39.96c.22.08.47 0 .59-.22l1.92-3.32c.12-.22.07-.47-.12-.61l-2.01-1.58zM12 15.6c-1.98 0-3.6-1.62-3.6-3.6s1.62-3.6 3.6-3.6 3.6 1.62 3.6 3.6-1.62 3.6-3.6 3.6z";
const BACK: &str = "M20 11H7.83l5.59-5.59L12 4l-8 8 8 8 1.41-1.41L7.83 13H20v-2z";
