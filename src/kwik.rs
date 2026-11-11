use regex::Regex;

/// Decodes Dean Edwards JavaScript packer:
/// eval(function(p,a,c,k,e,d){...}('payload', base, count, 'keywords'.split('|'), 0, {}))
pub fn unpack_dean_edwards(packed_js: &str) -> Option<String> {
    let re = Regex::new(
        r#"(?s)\}\s*\(\s*['"](?P<payload>.*)['"]\s*,\s*(?P<radix>\d+)\s*,\s*(?P<count>\d+)\s*,\s*['"](?P<kw>.*)['"]\s*\.split\s*\(\s*['"]\|['"]\s*\)"#
    ).ok()?;

    let caps = re.captures(packed_js)?;
    let payload = caps.name("payload")?.as_str();
    let radix: u32 = caps.name("radix")?.as_str().parse().ok()?;
    let count: usize = caps.name("count")?.as_str().parse().ok()?;
    let keywords_raw = caps.name("kw")?.as_str();
    let keywords: Vec<&str> = keywords_raw.split('|').collect();

    let mut result = String::with_capacity(payload.len() * 2);
    let mut word = String::new();

    let flush_word = |word: &mut String, res: &mut String| {
        if word.is_empty() {
            return;
        }
        if let Some(val) = parse_base(word, radix) {
            if val < count && val < keywords.len() {
                let kw = keywords[val];
                if !kw.is_empty() {
                    res.push_str(kw);
                    word.clear();
                    return;
                }
            }
        }
        res.push_str(word);
        word.clear();
    };

    let mut chars = payload.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next_ch) = chars.next() {
                flush_word(&mut word, &mut result);
                match next_ch {
                    '\\' => result.push('\\'),
                    '\'' => result.push('\''),
                    '"' => result.push('"'),
                    'n' => result.push('\n'),
                    'r' => result.push('\r'),
                    't' => result.push('\t'),
                    other => {
                        result.push('\\');
                        result.push(other);
                    }
                }
            } else {
                result.push('\\');
            }
        } else if ch.is_ascii_alphanumeric() {
            word.push(ch);
        } else {
            flush_word(&mut word, &mut result);
            result.push(ch);
        }
    }
    flush_word(&mut word, &mut result);

    Some(result)
}

fn parse_base(s: &str, radix: u32) -> Option<usize> {
    if radix == 0 || radix > 62 {
        return None;
    }
    let mut val: usize = 0;
    for ch in s.chars() {
        let digit = match ch {
            '0'..='9' => (ch as u32) - ('0' as u32),
            'a'..='z' => (ch as u32) - ('a' as u32) + 10,
            'A'..='Z' => (ch as u32) - ('A' as u32) + 36,
            _ => return None,
        };
        if digit >= radix {
            return None;
        }
        val = val
            .checked_mul(radix as usize)?
            .checked_add(digit as usize)?;
    }
    Some(val)
}

/// Extracts the direct .m3u8 playlist URL from Kwik HTML page content
pub fn extract_m3u8_from_kwik(html: &str) -> Option<String> {
    // 1. Match each eval(function(p,a,c,k,e,d)...) block individually
    let eval_re = Regex::new(
        r#"(?s)eval\s*\(\s*function\s*\(p,a,c,k,e,d\).*?\.split\s*\(\s*['"]\|['"]\s*\)\s*,\s*0\s*,\s*\{\}\s*\)\s*\)"#
    ).ok()?;

    for m in eval_re.find_iter(html) {
        if let Some(unpacked) = unpack_dean_edwards(m.as_str()) {
            if let Some(url) = find_m3u8(&unpacked) {
                return Some(url);
            }
        }
    }

    // 2. Direct fallback
    find_m3u8(html)
}

fn find_m3u8(text: &str) -> Option<String> {
    let m3u8_re = Regex::new(r#"https?://[^\s"'<>]+\.m3u8[^\s"'<>]*"#).ok()?;
    m3u8_re.find(text).map(|m| m.as_str().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dean_edwards() {
        let packed = r#"eval(function(p,a,c,k,e,d){while(c--)if(k[c])p=p.replace(new RegExp('\\b'+c.toString(a)+'\\b','g'),k[c]);return p}('0 1="2";',10,3,'const|source|https://example.com/playlist.m3u8'.split('|'),0,{}))"#;
        let unpacked = unpack_dean_edwards(packed).unwrap();
        assert!(unpacked.contains("https://example.com/playlist.m3u8"));
        let url = find_m3u8(&unpacked).unwrap();
        assert_eq!(url, "https://example.com/playlist.m3u8");
    }

    #[test]
    fn test_extract_m3u8_from_kwik_with_multiple_evals() {
        let sample = std::fs::read_to_string("/tmp/kwik_sample.html").unwrap_or_default();
        if !sample.is_empty() {
            let url = extract_m3u8_from_kwik(&sample);
            println!("Extracted M3U8 URL: {:?}", url);
            assert!(url.is_some(), "Should find m3u8 in real Kwik HTML");
            let m3u8 = url.unwrap();
            assert!(m3u8.starts_with("https://"));
            assert!(m3u8.ends_with(".m3u8") || m3u8.contains(".m3u8"));
        }
    }
}
