//! Turns an IP address into "who and where": country, network owner, and a
//! readable company name.
//!
//! Uses two free MaxMind-format databases (country: CC0, ASN: CC BY 4.0)
//! fetched once into the user's data directory. No licence key, no account.

use anyhow::{anyhow, Context, Result};
use maxminddb::Reader;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

const COUNTRIES_TSV: &str = include_str!("../data/countries.tsv");
const NPM_REGISTRY: &str = "https://registry.npmjs.org";

struct Database {
    package: &'static str,
    file_in_package: &'static str,
    local_name: &'static str,
}

const DATABASES: [Database; 2] = [
    Database {
        package: "@ip-location-db/geo-whois-asn-country-mmdb",
        file_in_package: "package/geo-whois-asn-country.mmdb",
        local_name: "country.mmdb",
    },
    Database {
        package: "@ip-location-db/asn-mmdb",
        file_in_package: "package/asn.mmdb",
        local_name: "asn.mmdb",
    },
];

/// Networks worth naming plainly. `hosting` marks clouds and CDNs: traffic
/// going there is hosted by them, not necessarily read by them.
const KNOWN_NETWORKS: &[(u32, &str, bool)] = &[
    (15169, "Google", false),
    (19527, "Google", false),
    (36040, "Google", false),
    (43515, "Google", false),
    (396982, "Google Cloud", true),
    (16509, "Amazon AWS", true),
    (14618, "Amazon AWS", true),
    (8075, "Microsoft", false),
    (8068, "Microsoft", false),
    (13335, "Cloudflare", true),
    (32934, "Meta", false),
    (714, "Apple", false),
    (6185, "Apple", false),
    (20940, "Akamai", true),
    (16625, "Akamai", true),
    (21342, "Akamai", true),
    (63949, "Akamai Cloud", true),
    (54113, "Fastly", true),
    (16276, "OVHcloud", true),
    (24940, "Hetzner", true),
    (14061, "DigitalOcean", true),
    (20473, "Vultr", true),
    (12876, "Scaleway", true),
    (31898, "Oracle Cloud", true),
    (36351, "IBM Cloud", true),
    (45102, "Alibaba Cloud", true),
    (37963, "Alibaba Cloud", true),
    (132203, "Tencent Cloud", true),
    (45090, "Tencent Cloud", true),
    (60781, "Leaseweb", true),
    (9009, "M247", true),
    (60068, "CDN77", true),
    (2906, "Netflix", false),
    (40027, "Netflix", false),
    (13414, "X (Twitter)", false),
    (36459, "GitHub", false),
    (46489, "Twitch", false),
    (32590, "Valve", false),
    (19679, "Dropbox", false),
    (14413, "LinkedIn", false),
    (8403, "Spotify", false),
    (62041, "Telegram", false),
    (30103, "Zoom", false),
    (2635, "Automattic", false),
    (14907, "Wikimedia", false),
    (6507, "Riot Games", false),
    (57976, "Blizzard", false),
];

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Place {
    /// ISO 3166-1 alpha-2, empty when unknown.
    pub cc: String,
    pub country: String,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub asn: Option<u32>,
    pub org: String,
    pub hosting: bool,
}

#[derive(Deserialize)]
struct CountryRecord {
    country_code: Option<String>,
}

#[derive(Deserialize)]
struct AsnRecord {
    autonomous_system_number: Option<u32>,
    autonomous_system_organization: Option<String>,
}

pub struct Geo {
    country: Option<Reader<Vec<u8>>>,
    asn: Option<Reader<Vec<u8>>>,
    countries: HashMap<&'static str, (&'static str, f64, f64)>,
    cache: HashMap<IpAddr, Place>,
}

impl Geo {
    /// Opens whatever databases are present; lookups degrade to "Unknown" without them.
    pub fn open(dir: &Path) -> Self {
        let read = |name: &str| Reader::open_readfile(dir.join(name)).ok();
        Self {
            country: read("country.mmdb"),
            asn: read("asn.mmdb"),
            countries: parse_countries(),
            cache: HashMap::new(),
        }
    }

    pub fn has_databases(&self) -> bool {
        self.country.is_some() && self.asn.is_some()
    }

    pub fn locate(&mut self, ip: IpAddr) -> Place {
        if let Some(place) = self.cache.get(&ip) {
            return place.clone();
        }
        let place = self.lookup(ip);
        if self.cache.len() > 50_000 {
            self.cache.clear();
        }
        self.cache.insert(ip, place.clone());
        place
    }

    fn lookup(&self, ip: IpAddr) -> Place {
        if !is_public(ip) {
            let org = if ip.is_loopback() {
                "This machine"
            } else {
                "Local network"
            };
            return Place {
                cc: String::new(),
                country: String::new(),
                lat: None,
                lon: None,
                asn: None,
                org: org.to_string(),
                hosting: false,
            };
        }

        let cc = self
            .country
            .as_ref()
            .and_then(|db| db.lookup::<CountryRecord>(ip).ok())
            .and_then(|r| r.country_code)
            .unwrap_or_default();
        let record = self
            .asn
            .as_ref()
            .and_then(|db| db.lookup::<AsnRecord>(ip).ok());
        let asn = record.as_ref().and_then(|r| r.autonomous_system_number);
        let raw_org = record.and_then(|r| r.autonomous_system_organization);

        let (org, hosting) = match asn.and_then(known_network) {
            Some((name, hosting)) => (name.to_string(), hosting),
            None => (
                raw_org
                    .as_deref()
                    .map(tidy_org)
                    .unwrap_or_else(|| "Unknown network".to_string()),
                false,
            ),
        };
        let (country, lat, lon) = match self.countries.get(cc.as_str()) {
            Some((name, lat, lon)) => (name.to_string(), Some(*lat), Some(*lon)),
            None => (String::new(), None, None),
        };
        Place {
            cc,
            country,
            lat,
            lon,
            asn,
            org,
            hosting,
        }
    }
}

fn parse_countries() -> HashMap<&'static str, (&'static str, f64, f64)> {
    COUNTRIES_TSV
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let cc = parts.next()?;
            let name = parts.next()?;
            let lat = parts.next()?.parse().ok()?;
            let lon = parts.next()?.parse().ok()?;
            Some((cc, (name, lat, lon)))
        })
        .collect()
}

fn known_network(asn: u32) -> Option<(&'static str, bool)> {
    KNOWN_NETWORKS
        .iter()
        .find(|(n, _, _)| *n == asn)
        .map(|(_, name, hosting)| (*name, *hosting))
}

/// Registry names are often shouted slugs with legal suffixes; make them readable.
pub fn tidy_org(raw: &str) -> String {
    let mut name = raw.trim().to_string();
    for suffix in [
        ", PBC",
        " PBC",
        ", Inc.",
        " Inc.",
        ", Inc",
        " Inc",
        ", LLC",
        " LLC",
        " L.L.C.",
        " Ltd.",
        " Ltd",
        " Limited",
        " GmbH",
        " S.A.S.",
        " SAS",
        " S.A.",
        " SA",
        " B.V.",
        " BV",
        " AB",
        " AG",
        " Corp.",
        " Corporation",
    ] {
        if name.len() > suffix.len() && name.to_lowercase().ends_with(&suffix.to_lowercase()) {
            name.truncate(name.len() - suffix.len());
            break;
        }
    }
    name.trim_end_matches(',').trim().to_string()
}

/// True for addresses routable on the public Internet.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_documentation()
                || (o[0] == 100 && (o[1] & 0xc0) == 64)) // carrier-grade NAT, 100.64.0.0/10
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00 // unique local, fc00::/7
                || (first & 0xffc0) == 0xfe80) // link-local, fe80::/10
        }
    }
}

pub fn data_dir() -> Result<PathBuf> {
    let dir = dirs::data_local_dir()
        .ok_or_else(|| anyhow!("no data directory on this system"))?
        .join("phonehome");
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

pub fn databases_present(dir: &Path) -> bool {
    DATABASES.iter().all(|db| dir.join(db.local_name).is_file())
}

/// Downloads both databases from the npm registry, where `ip-location-db` publishes them.
pub fn download_databases(dir: &Path) -> Result<()> {
    let agent = ureq::AgentBuilder::new().try_proxy_from_env(true).build();
    for db in DATABASES {
        let meta: serde_json::Value = agent
            .get(&format!("{NPM_REGISTRY}/{}/latest", db.package))
            .call()
            .with_context(|| format!("querying {}", db.package))?
            .into_json()?;
        let tarball = meta["dist"]["tarball"]
            .as_str()
            .ok_or_else(|| anyhow!("no tarball for {}", db.package))?;
        let version = meta["version"].as_str().unwrap_or("?");
        eprintln!("  fetching {} {version}", db.package);

        let response = agent
            .get(tarball)
            .call()
            .with_context(|| format!("downloading {tarball}"))?;
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(response.into_reader()));
        let mut bytes = Vec::new();
        for entry in archive.entries()? {
            let mut entry = entry?;
            if entry.path()?.to_string_lossy() == db.file_in_package {
                entry.read_to_end(&mut bytes)?;
                break;
            }
        }
        if bytes.is_empty() {
            return Err(anyhow!(
                "{} not found in {}",
                db.file_in_package,
                db.package
            ));
        }
        // Write then rename, so an interrupted download never leaves a half file behind.
        let tmp = dir.join(format!("{}.part", db.local_name));
        fs::write(&tmp, &bytes)?;
        fs::rename(&tmp, dir.join(db.local_name))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_addresses() {
        for public in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(is_public(public.parse().unwrap()), "{public}");
        }
        for local in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.1.10",
            "172.20.0.1",
            "169.254.1.1",
            "100.64.0.1",
            "::1",
            "fe80::1",
            "fd00::1",
        ] {
            assert!(!is_public(local.parse().unwrap()), "{local}");
        }
    }

    #[test]
    fn tidies_registry_names() {
        assert_eq!(tidy_org("Orange S.A."), "Orange");
        assert_eq!(tidy_org("Hetzner Online GmbH"), "Hetzner Online");
        assert_eq!(tidy_org("Signal Messenger, LLC"), "Signal Messenger");
        assert_eq!(tidy_org("SA"), "SA");
    }

    #[test]
    fn embeds_country_centroids() {
        let countries = parse_countries();
        assert!(countries.len() > 240);
        let (name, lat, lon) = countries["FR"];
        assert_eq!(name, "France");
        assert!((lat - 46.0).abs() < 3.0 && (lon - 2.0).abs() < 3.0);
    }

    #[test]
    fn local_addresses_never_hit_the_databases() {
        let mut geo = Geo {
            country: None,
            asn: None,
            countries: parse_countries(),
            cache: HashMap::new(),
        };
        assert_eq!(geo.locate("127.0.0.1".parse().unwrap()).org, "This machine");
        assert_eq!(
            geo.locate("192.168.1.1".parse().unwrap()).org,
            "Local network"
        );
        assert_eq!(
            geo.locate("8.8.8.8".parse().unwrap()).org,
            "Unknown network"
        );
    }

    /// Runs only where the databases have been downloaded (`phonehome update`).
    #[test]
    fn resolves_well_known_addresses() {
        let Ok(dir) = data_dir() else { return };
        if !databases_present(&dir) {
            return;
        }
        let mut geo = Geo::open(&dir);
        let google = geo.locate("8.8.8.8".parse().unwrap());
        assert_eq!(google.org, "Google");
        assert_eq!(google.cc, "US");
        let cloudflare = geo.locate("1.1.1.1".parse().unwrap());
        assert_eq!(cloudflare.org, "Cloudflare");
        assert!(cloudflare.hosting);
    }
}
