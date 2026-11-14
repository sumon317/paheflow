use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DNS_CACHE_TTL: Duration = Duration::from_secs(300);
type DnsCache = Arc<Mutex<HashMap<String, (Vec<IpAddr>, Instant)>>>;

#[derive(Clone)]
pub struct SmartDnsResolver {
    doh_client: reqwest::Client,
    cache: DnsCache,
}

impl Default for SmartDnsResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl SmartDnsResolver {
    pub fn new() -> Self {
        let doh_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(3))
            .build()
            .unwrap_or_default();
        Self {
            doh_client,
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn get_cached(&self, lower: &str) -> Option<Vec<SocketAddr>> {
        let mut guard = self.cache.lock().ok()?;
        if let Some((ips, at)) = guard.get(lower) {
            if at.elapsed() < DNS_CACHE_TTL {
                return Some(ips.iter().map(|ip| SocketAddr::new(*ip, 0)).collect());
            }
        }
        guard.remove(lower);
        None
    }

    fn put_cached(&self, lower: String, ips: Vec<IpAddr>) {
        if ips.is_empty() {
            return;
        }
        if let Ok(mut guard) = self.cache.lock() {
            if guard.len() > 256 {
                guard.retain(|_, (_, at)| at.elapsed() < DNS_CACHE_TTL);
            }
            guard.insert(lower, (ips, Instant::now()));
        }
    }
}

fn filter_ips(ips: impl IntoIterator<Item = IpAddr>, is_localhost: bool) -> Vec<IpAddr> {
    ips.into_iter()
        .filter(|ip| is_localhost || (!ip.is_loopback() && !ip.is_unspecified()))
        .collect()
}

fn anycast_fallback(lower: &str) -> Option<Vec<SocketAddr>> {
    if lower.ends_with(".owocdn.top") || lower.ends_with(".owocdn.com") || lower == "owocdn.top" {
        return Some(vec![
            SocketAddr::from(([172, 67, 145, 23], 0)),
            SocketAddr::from(([104, 21, 79, 107], 0)),
        ]);
    }
    if lower.ends_with(".kwik.cx") || lower == "kwik.cx" {
        return Some(vec![
            SocketAddr::from(([104, 21, 54, 81], 0)),
            SocketAddr::from(([172, 67, 136, 199], 0)),
        ]);
    }
    if lower.ends_with(".kwik.si") || lower == "kwik.si" {
        return Some(vec![SocketAddr::from(([104, 247, 81, 99], 0))]);
    }
    if lower.ends_with(".animepahe.pw") || lower == "animepahe.pw" {
        return Some(vec![
            SocketAddr::from(([172, 67, 214, 176], 0)),
            SocketAddr::from(([104, 21, 37, 233], 0)),
        ]);
    }
    if lower.ends_with(".animepahe.com")
        || lower == "animepahe.com"
        || lower.ends_with(".animepahe.org")
        || lower == "animepahe.org"
        || lower.ends_with(".animepahe.ru")
        || lower == "animepahe.ru"
    {
        return Some(vec![
            SocketAddr::from(([104, 21, 75, 223], 0)),
            SocketAddr::from(([172, 67, 182, 108], 0)),
        ]);
    }
    if lower.ends_with(".animepahe.si") || lower == "animepahe.si" {
        return Some(vec![SocketAddr::from(([162, 4, 84, 239], 0))]);
    }
    None
}

async fn query_doh_ips(
    doh: &reqwest::Client,
    url: &str,
    accept_dns_json: bool,
    is_localhost: bool,
) -> Vec<IpAddr> {
    let mut req = doh.get(url);
    if accept_dns_json {
        req = req.header("accept", "application/dns-json");
    }
    let Ok(resp) = req.send().await else {
        return Vec::new();
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let Ok(json) = resp.json::<serde_json::Value>().await else {
        return Vec::new();
    };
    let Some(answers) = json.get("Answer").and_then(|a| a.as_array()) else {
        return Vec::new();
    };
    let parsed = answers
        .iter()
        .filter_map(|ans| ans.get("data").and_then(|d| d.as_str()))
        .filter_map(|ip_str| ip_str.parse::<IpAddr>().ok());
    filter_ips(parsed, is_localhost)
}

impl Resolve for SmartDnsResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        let resolver = self.clone();

        Box::pin(async move {
            // 1. Try standard system DNS first (ignoring poisoned loopback/unspecified answers)
            let is_localhost = host.eq_ignore_ascii_case("localhost");
            let target = format!("{}:0", host);
            if let Ok(lookup) = tokio::net::lookup_host(&target).await {
                let addrs: Vec<SocketAddr> = lookup
                    .filter(|addr| {
                        is_localhost || (!addr.ip().is_loopback() && !addr.ip().is_unspecified())
                    })
                    .collect();
                if !addrs.is_empty() {
                    let iter: Addrs = Box::new(addrs.into_iter());
                    return Ok(iter);
                }
            }

            let lower = host.to_ascii_lowercase();

            // 2. Check cached DoH results before querying upstream resolvers
            if let Some(cached) = resolver.get_cached(&lower) {
                let iter: Addrs = Box::new(cached.into_iter());
                return Ok(iter);
            }

            // 3. Query Cloudflare DoH (1.1.1.1) directly by IP
            let cf_url = format!("https://1.1.1.1/dns-query?name={}&type=A", lower);
            let cf_ips = query_doh_ips(&resolver.doh_client, &cf_url, true, is_localhost).await;
            if !cf_ips.is_empty() {
                resolver.put_cached(lower, cf_ips.clone());
                let addrs: Vec<SocketAddr> = cf_ips
                    .into_iter()
                    .map(|ip| SocketAddr::new(ip, 0))
                    .collect();
                let iter: Addrs = Box::new(addrs.into_iter());
                return Ok(iter);
            }

            // 4. Query Google DoH (8.8.8.8) directly by IP
            let gg_url = format!("https://8.8.8.8/resolve?name={}&type=A", lower);
            let gg_ips = query_doh_ips(&resolver.doh_client, &gg_url, false, is_localhost).await;
            if !gg_ips.is_empty() {
                resolver.put_cached(lower, gg_ips.clone());
                let addrs: Vec<SocketAddr> = gg_ips
                    .into_iter()
                    .map(|ip| SocketAddr::new(ip, 0))
                    .collect();
                let iter: Addrs = Box::new(addrs.into_iter());
                return Ok(iter);
            }

            // 5. Last-resort Anycast fallback when both system DNS and DoH fail/are blocked
            if let Some(addrs) = anycast_fallback(&lower) {
                let iter: Addrs = Box::new(addrs.into_iter());
                return Ok(iter);
            }

            Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "Failed to resolve hostname '{}' via system DNS and DoH fallbacks",
                    host
                ),
            )) as Box<dyn std::error::Error + Send + Sync>)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_ips_blocks_loopback_and_unspecified_for_remote_hosts() {
        let raw: Vec<IpAddr> = vec![
            "127.0.0.1".parse().unwrap(),
            "0.0.0.0".parse().unwrap(),
            "::1".parse().unwrap(),
            "104.21.37.233".parse().unwrap(),
        ];
        assert_eq!(
            filter_ips(raw.clone(), false),
            vec!["104.21.37.233".parse::<IpAddr>().unwrap()]
        );
        assert_eq!(filter_ips(raw, true).len(), 4);
    }

    #[test]
    fn anycast_fallback_matches_known_domains_only() {
        assert!(anycast_fallback("vault-13.owocdn.top").is_some());
        assert!(anycast_fallback("animepahe.pw").is_some());
        assert!(anycast_fallback("kwik.cx").is_some());
        assert!(anycast_fallback("example.com").is_none());
    }

    #[tokio::test]
    async fn test_smart_dns_resolves_owocdn() {
        let resolver = SmartDnsResolver::new();
        let name = "vault-13.owocdn.top".parse::<Name>().unwrap();
        let res = resolver.resolve(name).await;
        assert!(
            res.is_ok(),
            "Resolver must succeed for owocdn: {:?}",
            res.err()
        );
        let addrs: Vec<SocketAddr> = res.unwrap().collect();
        assert!(!addrs.is_empty());
    }

    #[tokio::test]
    async fn test_smart_dns_resolves_animepahe() {
        let resolver = SmartDnsResolver::new();
        let name = "animepahe.pw".parse::<Name>().unwrap();
        let res = resolver.resolve(name).await;
        assert!(
            res.is_ok(),
            "Resolver must succeed for animepahe: {:?}",
            res.err()
        );
        let addrs: Vec<SocketAddr> = res.unwrap().collect();
        assert!(!addrs.is_empty());
    }
}
