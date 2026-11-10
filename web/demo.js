// Simulated traffic for the hosted demo and `phonehome --demo`.
// Produces snapshots in the exact shape the real agent serves at /api/state.
(function () {
  const PLACES = {
    US: ["United States", 38, -97], NL: ["Netherlands", 52.5, 5.75], SE: ["Sweden", 62, 15],
    BE: ["Belgium", 50.83, 4], DE: ["Germany", 51, 9], IE: ["Ireland", 53, -8],
    SG: ["Singapore", 1.37, 103.8], JP: ["Japan", 36, 138], FR: ["France", 46, 2],
    GB: ["United Kingdom", 54, -2], BR: ["Brazil", -10, -55], AU: ["Australia", -27, 133],
    IN: ["India", 20, 77], CA: ["Canada", 60, -95], FI: ["Finland", 64, 26],
  };

  // [app, [org, country, hosting, chattiness 0..1, typical burst in bytes]]
  const APPS = [
    ["firefox", [["Google", "US", 0, 0.7, 9000], ["Cloudflare", "US", 1, 0.5, 4000], ["Fastly", "GB", 1, 0.35, 6000], ["Wikimedia", "NL", 0, 0.15, 2500], ["Amazon AWS", "IE", 1, 0.3, 5000], ["Akamai", "FR", 1, 0.2, 3000]]],
    ["spotify", [["Spotify", "SE", 0, 0.45, 2500], ["Google Cloud", "BE", 1, 0.6, 3500], ["Fastly", "US", 1, 0.2, 1500]]],
    ["code", [["Microsoft", "US", 0, 0.3, 3000], ["GitHub", "US", 0, 0.4, 7000], ["Microsoft", "IE", 0, 0.15, 1500]]],
    ["slack", [["Amazon AWS", "US", 1, 0.5, 2200], ["Cloudflare", "DE", 1, 0.2, 900]]],
    ["steam", [["Valve", "US", 0, 0.2, 1800], ["Akamai", "DE", 1, 0.25, 1200], ["Valve", "SE", 0, 0.1, 800]]],
    ["dropbox", [["Dropbox", "US", 0, 0.25, 40000]]],
    ["telegram-desktop", [["Telegram", "NL", 0, 0.3, 1400]]],
    ["zoom", [["Zoom", "US", 0, 0.5, 60000], ["Oracle Cloud", "DE", 1, 0.3, 30000]]],
    ["photo-sync", [["Google Cloud", "FI", 1, 0.2, 120000], ["Alibaba Cloud", "SG", 1, 0.12, 15000]]],
    ["updater", [["Hetzner", "DE", 1, 0.06, 900], ["OVHcloud", "FR", 1, 0.05, 700]]],
  ];

  let seed = 20261110;
  function random() {
    seed = (seed * 1664525 + 1013904223) >>> 0;
    return seed / 4294967296;
  }

  const started = Date.now();
  const apps = APPS.map(([name, dests]) => ({
    name,
    dests: dests.map(([org, cc, hosting, chattiness, burst]) => ({
      org, cc, hosting: !!hosting, chattiness, burst,
      sent: Math.round(burst * (2 + random() * 30)),
      received: Math.round(burst * (10 + random() * 200)),
      connections: 1 + Math.floor(random() * 6),
      ips: 1 + Math.floor(random() * 4),
      active: 1 + Math.floor(random() * 3),
      lastActive: started - Math.floor(random() * 20000),
    })),
  }));

  function step(now) {
    for (const app of apps) {
      for (const d of app.dests) {
        if (random() < d.chattiness) {
          d.sent += Math.round(d.burst * (0.2 + random() * 1.6));
          d.received += Math.round(d.burst * (1 + random() * 12));
          d.lastActive = now;
          d.active = Math.max(1, d.active);
        }
        if (random() < 0.04) {
          d.active = Math.max(0, d.active + (random() < 0.5 ? -1 : 1));
          if (d.active > 0 && random() < 0.5) {
            d.connections += 1;
            d.lastActive = now;
          }
        }
      }
    }
  }

  window.phonehomeDemo = function snapshot() {
    const now = Date.now();
    step(now);
    return {
      version: "demo",
      platform: "linux",
      uptime_s: Math.round((now - started) / 1000),
      byte_counts: true,
      located: true,
      apps: apps
        .map((app) => {
          const dests = app.dests
            .map((d) => ({
              org: d.org, cc: d.cc, hosting: d.hosting, asn: null,
              country: PLACES[d.cc][0], lat: PLACES[d.cc][1], lon: PLACES[d.cc][2],
              sent: d.sent, received: d.received, active: d.active,
              connections: d.connections, ips: d.ips, ports: ["443"],
              idle_ms: now - d.lastActive,
            }))
            .sort((a, b) => b.sent - a.sent);
          return {
            name: app.name,
            sent: dests.reduce((s, d) => s + d.sent, 0),
            received: dests.reduce((s, d) => s + d.received, 0),
            active: dests.reduce((s, d) => s + d.active, 0),
            processes: 1,
            dests,
          };
        })
        .sort((a, b) => b.sent - a.sent),
    };
  };
})();
