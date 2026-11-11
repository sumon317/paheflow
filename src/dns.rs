use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::net::SocketAddr;
use std::time::Duration;

#[derive(Clone)]
pub struct SmartDnsResolver {
    doh_client: reqwest::Client,
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
        Self { doh_client }
    }
}

impl Resolve for SmartDnsResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        let doh = self.doh_client.clone();

        Box::pin(async move {
            // 1. Try standard system DNS first
            let target = format!("{}:0", host);
            if let Ok(lookup) = tokio::net::lookup_host(&target).await {
                let addrs: Vec<SocketAddr> = lookup.collect();
                if !addrs.is_empty() {
                    let iter: Addrs = Box::new(addrs.into_iter());
                    return Ok(iter);
                }
            }

            // 2. Fast Anycast fallback for AnimePahe / owocdn / kwik domains (bypasses ISP DNS blocks)
            let lower = host.to_ascii_lowercase();
            if lower.ends_with(".owocdn.top")
                || lower.ends_with(".owocdn.com")
                || lower == "owocdn.top"
            {
                let addrs = vec![
                    SocketAddr::from(([172, 67, 145, 23], 0)),
                    SocketAddr::from(([104, 21, 79, 107], 0)),
                ];
                let iter: Addrs = Box::new(addrs.into_iter());
                return Ok(iter);
            }
            if lower.ends_with(".kwik.cx") || lower == "kwik.cx" {
                let addrs = vec![
                    SocketAddr::from(([104, 21, 54, 81], 0)),
                    SocketAddr::from(([172, 67, 136, 199], 0)),
                ];
                let iter: Addrs = Box::new(addrs.into_iter());
                return Ok(iter);
            }
            if lower.ends_with(".kwik.si") || lower == "kwik.si" {
                let addrs = vec![SocketAddr::from(([104, 247, 81, 99], 0))];
                let iter: Addrs = Box::new(addrs.into_iter());
                return Ok(iter);
            }
            if lower.ends_with(".animepahe.pw") || lower == "animepahe.pw" {
                let addrs = vec![
                    SocketAddr::from(([172, 67, 214, 176], 0)),
                    SocketAddr::from(([104, 21, 37, 233], 0)),
                ];
                let iter: Addrs = Box::new(addrs.into_iter());
                return Ok(iter);
            }

            // 3. Fallback: Query Cloudflare DoH (1.1.1.1) directly by IP
            let cf_url = format!("https://1.1.1.1/dns-query?name={}&type=A", lower);
            if let Ok(resp) = doh
                .get(&cf_url)
                .header("accept", "application/dns-json")
                .send()
                .await
            {
                if resp.status().is_success() {
                    if let Ok(json) = resp.json::<serde_json::Value>().await {
                        if let Some(answers) = json.get("Answer").and_then(|a| a.as_array()) {
                            let mut addrs = Vec::new();
                            for ans in answers {
                                if let Some(ip_str) = ans.get("data").and_then(|d| d.as_str()) {
                                    if let Ok(ip) = ip_str.parse::<std::net::IpAddr>() {
                                        addrs.push(SocketAddr::new(ip, 0));
                                    }
                                }
                            }
                            if !addrs.is_empty() {
                                let iter: Addrs = Box::new(addrs.into_iter());
                                return Ok(iter);
                            }
                        }
                    }
                }
            }

            // 4. Fallback: Query Google DoH (8.8.8.8) directly by IP
            let gg_url = format!("https://8.8.8.8/resolve?name={}&type=A", lower);
            if let Ok(resp) = doh.get(&gg_url).send().await {
                if resp.status().is_success() {
                    if let Ok(json) = resp.json::<serde_json::Value>().await {
                        if let Some(answers) = json.get("Answer").and_then(|a| a.as_array()) {
                            let mut addrs = Vec::new();
                            for ans in answers {
                                if let Some(ip_str) = ans.get("data").and_then(|d| d.as_str()) {
                                    if let Ok(ip) = ip_str.parse::<std::net::IpAddr>() {
                                        addrs.push(SocketAddr::new(ip, 0));
                                    }
                                }
                            }
                            if !addrs.is_empty() {
                                let iter: Addrs = Box::new(addrs.into_iter());
                                return Ok(iter);
                            }
                        }
                    }
                }
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
