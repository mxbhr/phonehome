/* global d3, topojson */
(function () {
  "use strict";

  // One line colour per application, in order of appearance.
  const CABLES_LIGHT = ["#e63946", "#1d7874", "#e09f00", "#5a4fcf", "#0a85d1", "#d6457f", "#3c8d2f", "#b3541e", "#00838f", "#8e3b9c"];
  const CABLES_DARK = ["#ff6b6b", "#4fd1b5", "#ffc43d", "#9f97ff", "#56b8f5", "#ff7fb0", "#7ed36b", "#f0935a", "#4dd0dc", "#cf85de"];
  const POLL_MS = 1000;
  const LIVE_WINDOW_MS = 2500;
  const DEG = Math.PI / 180;

  const $ = (id) => document.getElementById(id);
  const canvas = $("globe");
  const ctx = canvas.getContext("2d");
  const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)");
  const darkScheme = window.matchMedia("(prefers-color-scheme: dark)");

  const projection = d3.geoOrthographic().clipAngle(90).precision(0.4);
  const path = d3.geoPath(projection, ctx);
  const graticule = d3.geoGraticule10();

  const view = {
    width: 0, height: 0, zoom: 1,
    rotation: [0, -20], target: null,
    land: null, borders: null,
    origin: [2.35, 48.86], originKnown: false,
    hover: null, theme: {},
  };

  const model = {
    snapshot: null, demo: false, stopped: false,
    selected: null,
    colors: new Map(), order: new Map(),
    arcs: [], places: [],
  };

  // ---------- formatting ----------

  function bytes(n) {
    if (n < 1000) return `${n} B`;
    const units = ["kB", "MB", "GB", "TB"];
    let value = n, unit = -1;
    do { value /= 1000; unit += 1; } while (value >= 1000 && unit < units.length - 1);
    return `${value >= 100 ? value.toFixed(0) : value.toFixed(1)} ${units[unit]}`;
  }

  function duration(seconds) {
    if (seconds < 60) return "less than a minute";
    const minutes = Math.round(seconds / 60);
    if (minutes < 60) return `${minutes} minute${minutes > 1 ? "s" : ""}`;
    const hours = Math.floor(minutes / 60);
    return `${hours} h ${String(minutes % 60).padStart(2, "0")}`;
  }

  const plural = (n, word, many) => `${n} ${n === 1 ? word : many || word + "s"}`;

  function colorOf(name) {
    if (!model.colors.has(name)) model.colors.set(name, model.colors.size);
    const palette = darkScheme.matches ? CABLES_DARK : CABLES_LIGHT;
    return palette[model.colors.get(name) % palette.length];
  }

  // ---------- geometry ----------

  const toVector = ([lon, lat]) => {
    const l = lon * DEG, p = lat * DEG;
    return [Math.cos(p) * Math.cos(l), Math.cos(p) * Math.sin(l), Math.sin(p)];
  };
  const toLonLat = ([x, y, z]) => [Math.atan2(y, x) / DEG, Math.asin(Math.max(-1, Math.min(1, z))) / DEG];
  const cross = (a, b) => [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
  const norm = (v) => { const n = Math.hypot(v[0], v[1], v[2]) || 1; return [v[0] / n, v[1] / n, v[2] / n]; };

  /**
   * Great-circle route from a to b, pushed sideways by `bend` radians at its
   * middle so that several lines to the same country fan out instead of overlapping.
   */
  function route(a, b, bend) {
    const va = toVector(a), vb = toVector(b);
    const angle = Math.acos(Math.max(-1, Math.min(1, va[0] * vb[0] + va[1] * vb[1] + va[2] * vb[2])));
    if (angle < 1e-6) return [a, b];
    const normal = norm(cross(va, vb));
    const sin = Math.sin(angle);
    const points = [];
    const steps = 40;
    for (let i = 0; i <= steps; i++) {
      const t = i / steps;
      const ka = Math.sin((1 - t) * angle) / sin, kb = Math.sin(t * angle) / sin;
      const k = bend * Math.sin(Math.PI * t);
      points.push(toLonLat(norm([
        va[0] * ka + vb[0] * kb + normal[0] * k,
        va[1] * ka + vb[1] * kb + normal[1] * k,
        va[2] * ka + vb[2] * kb + normal[2] * k,
      ])));
    }
    return points;
  }

  function visible(point) {
    const center = [-view.rotation[0], -view.rotation[1]];
    return d3.geoDistance(center, point) < Math.PI / 2 - 0.02;
  }

  // ---------- model ----------

  function visibleApps() {
    const apps = model.snapshot ? model.snapshot.apps : [];
    return model.selected ? apps.filter((a) => a.name === model.selected) : apps;
  }

  function rebuildGeometry() {
    const byCountry = new Map();
    for (const app of visibleApps()) {
      for (const dest of app.dests) {
        if (dest.lat == null || dest.lon == null) continue;
        if (!byCountry.has(dest.cc)) {
          byCountry.set(dest.cc, { cc: dest.cc, country: dest.country, point: [dest.lon, dest.lat], lines: [] });
        }
        byCountry.get(dest.cc).lines.push({ app: app.name, dest });
      }
    }

    model.arcs = [];
    model.places = [];
    for (const place of byCountry.values()) {
      // One line per application and country, however many networks it reaches there.
      const perApp = new Map();
      for (const line of place.lines) {
        const entry = perApp.get(line.app) || { app: line.app, sent: 0, active: 0, idle: Infinity, dests: [] };
        entry.sent += line.dest.sent;
        entry.active += line.dest.active;
        entry.idle = Math.min(entry.idle, line.dest.idle_ms);
        entry.dests.push(line.dest);
        perApp.set(line.app, entry);
      }
      const lines = [...perApp.values()].sort((a, b) => a.app.localeCompare(b.app));
      lines.forEach((line, index) => {
        const bend = (index - (lines.length - 1) / 2) * 0.05;
        model.arcs.push({
          app: line.app,
          live: line.idle < LIVE_WINDOW_MS,
          open: line.active > 0,
          sent: line.sent,
          coordinates: route(view.origin, place.point, bend),
        });
      });
      model.places.push({ ...place, lines, weight: lines.reduce((s, l) => s + l.active, 0) });
    }
    model.places.sort((a, b) => b.weight - a.weight);
  }

  // ---------- drawing ----------

  function readTheme() {
    const css = getComputedStyle(document.documentElement);
    const get = (name) => css.getPropertyValue(name).trim();
    view.theme = {
      sea: get("--sea"), seaLine: get("--sea-line"), land: get("--land"), landLine: get("--land-line"),
      ink: get("--ink"), inkSoft: get("--ink-soft"), page: get("--page"),
    };
  }

  function resize() {
    const box = canvas.getBoundingClientRect();
    const ratio = Math.min(window.devicePixelRatio || 1, 2);
    view.width = box.width;
    view.height = box.height;
    canvas.width = Math.round(box.width * ratio);
    canvas.height = Math.round(box.height * ratio);
    ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
    draw(performance.now());
  }

  function draw(time) {
    const { width, height, theme } = view;
    if (!width || !height) return;
    const radius = Math.min(width, height) / 2 * 0.9 * view.zoom;
    projection.translate([width / 2, height / 2]).scale(radius).rotate(view.rotation);

    ctx.clearRect(0, 0, width, height);
    ctx.lineJoin = "round";
    ctx.lineCap = "round";
    ctx.setLineDash([]);

    ctx.beginPath(); path({ type: "Sphere" });
    ctx.fillStyle = theme.sea; ctx.fill();

    ctx.beginPath(); path(graticule);
    ctx.strokeStyle = theme.seaLine; ctx.lineWidth = 0.6; ctx.stroke();

    if (view.land) {
      ctx.beginPath(); path(view.land);
      ctx.fillStyle = theme.land; ctx.fill();
      ctx.beginPath(); path(view.borders);
      ctx.strokeStyle = theme.landLine; ctx.lineWidth = 0.5; ctx.globalAlpha = 0.55; ctx.stroke();
      ctx.globalAlpha = 1;
      ctx.beginPath(); path(view.land);
      ctx.strokeStyle = theme.landLine; ctx.lineWidth = 0.8; ctx.stroke();
    }

    const animate = !reducedMotion.matches;
    for (const arc of model.arcs) {
      const line = { type: "LineString", coordinates: arc.coordinates };
      const width = 1.6 + Math.min(2.4, Math.log10(arc.sent + 10) / 3);
      ctx.beginPath(); path(line);
      ctx.strokeStyle = colorOf(arc.app);
      ctx.lineWidth = arc.live ? width + 0.6 : width;
      ctx.globalAlpha = arc.open ? 1 : 0.35;
      ctx.setLineDash(arc.open ? [] : [2, 5]);
      ctx.stroke();

      if (arc.live && animate) {
        // Gaps travelling outward along the cable while bytes are leaving.
        ctx.beginPath(); path(line);
        ctx.setLineDash([3, 15]);
        ctx.lineDashOffset = -(time / 28) % 18;
        ctx.strokeStyle = theme.land;
        ctx.lineWidth = Math.max(1, width - 1.2);
        ctx.globalAlpha = 0.9;
        ctx.stroke();
        ctx.lineDashOffset = 0;
      }
      ctx.setLineDash([]);
      ctx.globalAlpha = 1;
    }

    ctx.font = '600 13px "Bricolage", system-ui, sans-serif';
    ctx.textBaseline = "middle";
    const taken = [];

    if (visible(view.origin)) {
      const [x, y] = projection(view.origin);
      ctx.beginPath(); ctx.arc(x, y, 9, 0, Math.PI * 2);
      ctx.strokeStyle = theme.ink; ctx.lineWidth = 1.5; ctx.stroke();
      ctx.beginPath(); ctx.arc(x, y, 4, 0, Math.PI * 2);
      ctx.fillStyle = theme.ink; ctx.fill();
      label("You", x, y, 14, taken, true);
    }

    const dots = [];
    for (const place of model.places) {
      if (!visible(place.point)) continue;
      const [x, y] = projection(place.point);
      const r = 3.5 + Math.min(5, Math.sqrt(place.lines.length) * 1.6);
      ctx.beginPath(); ctx.arc(x, y, r, 0, Math.PI * 2);
      ctx.fillStyle = theme.ink; ctx.fill();
      ctx.lineWidth = 2; ctx.strokeStyle = theme.land; ctx.stroke();
      dots.push({ place, x, y, r });
    }
    // The hovered country always gets its name; the others only where there is room.
    dots.sort((a, b) => (b.place === view.hover) - (a.place === view.hover));
    for (const dot of dots) label(dot.place.country, dot.x, dot.y, dot.r + 6, taken, dot.place === view.hover);
  }

  /** Writes a label right of (x, y), or left of it, unless both spots are occupied. */
  function label(text, x, y, gap, taken, force) {
    const width = ctx.measureText(text).width;
    const spots = [x + gap, x - gap - width];
    let left = spots.find((l) => !taken.some((b) => l < b.right + 4 && l + width > b.left - 4 && Math.abs(y - b.y) < 15));
    if (left === undefined) {
      if (!force) return;
      left = spots[0];
    }
    taken.push({ left, right: left + width, y });
    ctx.lineWidth = 4;
    ctx.strokeStyle = view.theme.land;
    ctx.strokeText(text, left, y);
    ctx.fillStyle = view.theme.ink;
    ctx.fillText(text, left, y);
  }

  let running = false;
  function frame(time) {
    let moving = false;
    if (view.target) {
      const [tl, tp] = view.target;
      let dl = ((tl - view.rotation[0] + 540) % 360) - 180;
      const dp = tp - view.rotation[1];
      if (Math.abs(dl) < 0.2 && Math.abs(dp) < 0.2) {
        view.rotation = [tl, tp];
        view.target = null;
      } else {
        view.rotation = [view.rotation[0] + dl * 0.14, view.rotation[1] + dp * 0.14];
        moving = true;
      }
    }
    draw(time);
    const flowing = !reducedMotion.matches && model.arcs.some((a) => a.live);
    if (moving || flowing) requestAnimationFrame(frame);
    else running = false;
  }
  function kick() {
    if (!running) { running = true; requestAnimationFrame(frame); }
  }

  /** Turns the globe to face the given points, zooming in when they sit close together. */
  function lookAt(points, immediate) {
    if (!points.length) return;
    const sum = points.map(toVector).reduce((s, v) => [s[0] + v[0], s[1] + v[1], s[2] + v[2]], [0, 0, 0]);
    const center = toLonLat(norm(sum));
    const spread = Math.max(...points.map((p) => d3.geoDistance(center, p)));
    view.zoom = Math.max(1, Math.min(2.6, 0.72 / Math.sin(Math.max(0.2, Math.min(Math.PI / 2, spread)))));
    const target = [-center[0], Math.max(-60, Math.min(60, -center[1]))];
    if (immediate || reducedMotion.matches) view.rotation = target;
    else view.target = target;
    kick();
  }

  // ---------- panel ----------

  function renderHead() {
    const snap = model.snapshot;
    const status = $("status");
    status.classList.toggle("is-off", model.stopped);
    status.textContent = model.stopped ? "Agent stopped" : model.demo ? "Simulated traffic" : "Watching this machine";

    const apps = snap ? snap.apps.filter((a) => a.active > 0) : [];
    const networks = new Set(), countries = new Set();
    for (const app of apps) {
      for (const d of app.dests) {
        if (d.active === 0) continue;
        networks.add(d.org);
        if (d.cc) countries.add(d.cc);
      }
    }

    const headline = $("headline");
    if (!snap) {
      headline.textContent = "Looking at the connections of this machine";
    } else if (apps.length === 0) {
      headline.textContent = "No app is talking to the Internet right now";
    } else {
      headline.innerHTML =
        `<b>${apps.length}</b> ${apps.length === 1 ? "app is" : "apps are"} talking to ` +
        `<b>${networks.size}</b> ${networks.size === 1 ? "network" : "networks"}` +
        (countries.size ? ` in <b>${countries.size}</b> ${countries.size === 1 ? "country" : "countries"}` : "");
    }

    const subline = $("subline");
    if (!snap) {
      subline.textContent = "";
    } else {
      const sent = snap.apps.reduce((s, a) => s + a.sent, 0);
      const since = `since you started watching, ${duration(snap.uptime_s)} ago`;
      subline.textContent = snap.byte_counts
        ? `${bytes(sent)} sent ${since}.`
        : `Watching ${since}. Byte counts are only available on Linux.`;
    }

    const notice = $("notice");
    notice.hidden = !snap || snap.located;
    if (!notice.hidden) notice.innerHTML = "Locations are unavailable. Run <code>phonehome update</code> to download them.";
  }

  function destRow(dest, byteCounts) {
    const li = document.createElement("li");
    li.className = "dest" + (dest.idle_ms < LIVE_WINDOW_MS ? " is-live" : "");
    const org = document.createElement("span");
    org.className = "dest-org";
    org.textContent = dest.org;
    if (dest.hosting) {
      const tag = document.createElement("span");
      tag.className = "tag";
      tag.textContent = "hosting";
      org.append(tag);
    }
    const amount = document.createElement("span");
    amount.className = "dest-amount";
    amount.textContent = byteCounts ? bytes(dest.sent) : plural(dest.connections, "connection");
    const where = document.createElement("span");
    where.className = "dest-where";
    const state = dest.active > 0 ? plural(dest.active, "open connection") : "closed";
    where.textContent = [dest.country || "", state].filter(Boolean).join(", ");
    li.append(org, amount, where);
    return li;
  }

  function renderApps() {
    const list = $("apps");
    const snap = model.snapshot;
    $("show-all").hidden = !model.selected;

    if (!snap || snap.apps.length === 0) {
      list.innerHTML = snap
        ? `<li class="empty"><strong>Nothing has left this machine yet</strong>Open a web page or start an app: it will show up here within a second.</li>`
        : "";
      return;
    }
    list.querySelector(".empty")?.remove();

    // Ranks are fixed at first sight so rows do not jump around every second.
    for (const app of snap.apps) if (!model.order.has(app.name)) model.order.set(app.name, model.order.size);
    const apps = [...snap.apps].sort((a, b) => model.order.get(a.name) - model.order.get(b.name));

    const existing = new Map([...list.children].map((li) => [li.dataset.app, li]));
    for (const app of apps) {
      let li = existing.get(app.name);
      if (!li) {
        li = document.createElement("li");
        li.className = "app";
        li.dataset.app = app.name;
        li.innerHTML = `<button type="button" class="app-head"><span class="cable"></span><span class="app-name"></span><span class="app-total"></span><span class="app-meta"></span></button><ul class="dests"></ul>`;
        li.querySelector(".app-name").textContent = app.name;
        li.querySelector("button").addEventListener("click", () => select(model.selected === app.name ? null : app.name));
        list.append(li);
      }
      existing.delete(app.name);

      const selected = model.selected === app.name;
      li.classList.toggle("is-dimmed", !!model.selected && !selected);
      li.querySelector("button").setAttribute("aria-pressed", String(selected));
      li.querySelector(".cable").style.background = colorOf(app.name);
      li.querySelector(".app-total").textContent = snap.byte_counts ? bytes(app.sent) : plural(app.active, "connection");
      const countries = new Set(app.dests.map((d) => d.cc).filter(Boolean)).size;
      li.querySelector(".app-meta").textContent =
        `${plural(app.dests.length, "network")}` + (countries ? ` in ${plural(countries, "country", "countries")}` : "") +
        (snap.byte_counts ? `, ${bytes(app.received)} received` : "");

      const dests = li.querySelector(".dests");
      const limit = selected ? app.dests.length : 3;
      dests.replaceChildren(...app.dests.slice(0, limit).map((d) => destRow(d, snap.byte_counts)));
      if (app.dests.length > limit) {
        const more = document.createElement("li");
        more.className = "more";
        more.textContent = `and ${app.dests.length - limit} more`;
        dests.append(more);
      }
    }
    for (const stale of existing.values()) stale.remove();
  }

  function select(name) {
    model.selected = name;
    rebuildGeometry();
    renderApps();
    lookAt([view.origin, ...model.places.map((p) => p.point)]);
    kick();
  }

  // ---------- pointer ----------

  function placeAt(x, y) {
    let best = null, bestDistance = 16;
    for (const place of model.places) {
      if (!visible(place.point)) continue;
      const [px, py] = projection(place.point);
      const distance = Math.hypot(px - x, py - y);
      if (distance < bestDistance) { best = place; bestDistance = distance; }
    }
    return best;
  }

  function showTooltip(place, x, y) {
    const tip = $("tooltip");
    if (!place) { tip.hidden = true; return; }
    tip.replaceChildren();
    const title = document.createElement("strong");
    title.textContent = place.country;
    const ul = document.createElement("ul");
    for (const line of place.lines) {
      const li = document.createElement("li");
      const dot = document.createElement("span");
      dot.className = "dot";
      dot.style.background = colorOf(line.app);
      const text = document.createElement("span");
      text.textContent = `${line.app}: ${[...new Set(line.dests.map((d) => d.org))].join(", ")}`;
      li.append(dot, text);
      ul.append(li);
    }
    tip.append(title, ul);
    tip.hidden = false;
    const box = canvas.getBoundingClientRect();
    tip.style.left = `${Math.min(x, box.width - 300)}px`;
    tip.style.top = `${Math.min(y, box.height - 40 - place.lines.length * 20)}px`;
  }

  let drag = null;
  canvas.addEventListener("pointerdown", (e) => {
    drag = { x: e.clientX, y: e.clientY, rotation: [...view.rotation] };
    view.target = null;
    canvas.setPointerCapture(e.pointerId);
    canvas.classList.add("dragging");
  });
  canvas.addEventListener("pointermove", (e) => {
    const box = canvas.getBoundingClientRect();
    if (drag) {
      const k = 0.28 / view.zoom;
      view.rotation = [
        drag.rotation[0] + (e.clientX - drag.x) * k,
        Math.max(-85, Math.min(85, drag.rotation[1] - (e.clientY - drag.y) * k)),
      ];
      showTooltip(null);
      draw(performance.now());
      return;
    }
    const place = placeAt(e.clientX - box.left, e.clientY - box.top);
    if (place !== view.hover) { view.hover = place; draw(performance.now()); }
    showTooltip(place, e.clientX - box.left, e.clientY - box.top);
  });
  const endDrag = () => { drag = null; canvas.classList.remove("dragging"); };
  canvas.addEventListener("pointerup", endDrag);
  canvas.addEventListener("pointercancel", endDrag);
  canvas.addEventListener("pointerleave", () => { view.hover = null; showTooltip(null); draw(performance.now()); });
  canvas.addEventListener("wheel", (e) => {
    e.preventDefault();
    view.zoom = Math.max(0.8, Math.min(4, view.zoom * (e.deltaY < 0 ? 1.1 : 0.9)));
    draw(performance.now());
  }, { passive: false });

  $("show-all").addEventListener("click", () => select(null));

  // ---------- data ----------

  function accept(snapshot) {
    const first = !model.snapshot;
    model.snapshot = snapshot;
    model.stopped = false;
    rebuildGeometry();
    if (first && model.places.length) lookAt([view.origin, ...model.places.map((p) => p.point)], true);
    renderHead();
    renderApps();
    kick();
  }

  async function poll() {
    if (model.demo) {
      accept(window.phonehomeDemo());
    } else {
      try {
        const response = await fetch("api/state", { cache: "no-store" });
        if (!response.ok) throw new Error(String(response.status));
        const data = await response.json();
        if (data.demo) model.demo = true;
        else if (data.apps) accept(data);
      } catch {
        // No agent behind this page: either it stopped, or this is the hosted demo.
        if (model.snapshot) { model.stopped = true; renderHead(); }
        else model.demo = true;
      }
      if (model.demo && !model.snapshot) accept(window.phonehomeDemo());
    }
    setTimeout(poll, POLL_MS);
  }

  async function start() {
    readTheme();
    new ResizeObserver(resize).observe(canvas);
    darkScheme.addEventListener("change", () => { readTheme(); renderApps(); draw(performance.now()); });
    renderHead();

    const [world, zones] = await Promise.all([
      fetch("vendor/countries-110m.json").then((r) => r.json()),
      fetch("vendor/timezones.json").then((r) => r.json()).catch(() => ({})),
    ]);
    view.land = topojson.feature(world, world.objects.land);
    view.borders = topojson.mesh(world, world.objects.countries, (a, b) => a !== b);

    // The browser knows the time zone; its reference city is close enough to draw from.
    const zone = Intl.DateTimeFormat().resolvedOptions().timeZone;
    if (zones[zone]) {
      view.origin = [zones[zone][1], zones[zone][0]];
      view.originKnown = true;
    }
    $("origin-note").textContent = view.originKnown
      ? `Lines start from ${zone.split("/").pop().replace(/_/g, " ")}, the reference city of your time zone, and end at the centre of each country.`
      : "Lines end at the centre of each country.";
    view.rotation = [-view.origin[0] + 28, -Math.max(-50, Math.min(50, view.origin[1])) + 22];

    document.fonts?.ready.then(() => draw(performance.now()));
    poll();
  }

  start();
})();
