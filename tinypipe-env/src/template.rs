//! Template çözümleme: `${KEY}`, `${KEY:-default}`, `${KEY:?}`, `{{KEY}}`.
//!
//! - `${KEY}` — varsa değer, yoksa boş string
//! - `${KEY:-default}` — yoksa `default`
//! - `${KEY:?}` — yoksa `EnvError::MissingRequired` (Vault-tarzı zorunlu referans)
//! - `{{KEY}}` — `${KEY}`'in alternatif yazımı (aynı semantik)
//!
//! `$${KEY}` ile escape edilebilir (literal `${KEY}`).

use crate::{Env, EnvError};

/// String içindeki tüm placeholder'ları çözer.
pub fn resolve(env: &Env, s: &str) -> Result<String, EnvError> {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];

        // Escape: `$${` → literal `${`
        if c == '$'
            && i + 2 < chars.len()
            && chars[i + 1] == '$'
            && chars[i + 2] == '{'
        {
            out.push('$');
            i += 2;
            continue;
        }

        if c == '$' && i + 1 < chars.len() && chars[i + 1] == '{' {
            // `${...}` — kapanış `}` bul
            if let Some(end) = find_closing(&chars, i + 2, '{', '}') {
                let spec: String = chars[i + 2..end].iter().collect();
                out.push_str(&resolve_spec(env, &spec)?);
                i = end + 1;
                continue;
            }
            out.push(c);
            i += 1;
            continue;
        }

        if c == '{' && i + 1 < chars.len() && chars[i + 1] == '{' {
            // `{{...}}` — kapanış `}}` bul
            if let Some(end) = find_closing(&chars, i + 2, '{', '}') {
                let spec: String = chars[i + 2..end].iter().collect();
                out.push_str(&resolve_spec(env, &spec)?);
                // `}}` iki karakteri de tüket
                i = end + 2;
                continue;
            }
            out.push(c);
            i += 1;
            continue;
        }

        out.push(c);
        i += 1;
    }

    Ok(out)
}

fn find_closing(chars: &[char], start: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 1;
    let mut i = start;
    while i < chars.len() {
        if chars[i] == open {
            depth += 1;
        } else if chars[i] == close {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

fn resolve_spec(env: &Env, spec: &str) -> Result<String, EnvError> {
    // `${KEY:-default}` veya `${KEY:?}`
    if let Some(idx) = spec.find(":-") {
        let (key, default) = spec.split_at(idx);
        let default = &default[2..];
        Ok(env.get(key).unwrap_or_else(|| default.to_string()))
    } else if let Some(idx) = spec.find(":?") {
        let key = &spec[..idx];
        match env.get(key) {
            Some(v) => Ok(v),
            None => Err(EnvError::MissingRequired(key.to_string())),
        }
    } else {
        Ok(env.get(spec).unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::static_provider::StaticEnvProvider;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn env_with(vars: HashMap<&str, &str>) -> Env {
        let m: HashMap<String, String> = vars
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Env::new(vec![Arc::new(StaticEnvProvider::new(m))])
    }

    #[test]
    fn test_simple_and_missing() {
        let env = env_with(HashMap::from([("HOST", "h")]));
        assert_eq!(resolve(&env, "x${HOST}y").unwrap(), "xhy");
        assert_eq!(resolve(&env, "a${MISSING}b").unwrap(), "ab");
    }

    #[test]
    fn test_default_and_required() {
        let env = env_with(HashMap::from([("A", "1")]));
        assert_eq!(resolve(&env, "${A:-d}").unwrap(), "1");
        assert_eq!(resolve(&env, "${B:-d}").unwrap(), "d");
        assert_eq!(resolve(&env, "${A:?}").unwrap(), "1");
        assert!(matches!(
            resolve(&env, "${B:?}"),
            Err(EnvError::MissingRequired(k)) if k == "B"
        ));
    }

    #[test]
    fn test_braces_alternate_and_nested() {
        let env = env_with(HashMap::from([("K", "v")]));
        assert_eq!(resolve(&env, "{{K}}!").unwrap(), "v!");
        assert_eq!(resolve(&env, "{not a var}").unwrap(), "{not a var}");
    }

    #[test]
    fn test_escape() {
        let env = Env::empty();
        assert_eq!(resolve(&env, "$${K}").unwrap(), "${K}");
    }

    #[test]
    fn test_scoped_resolution() {
        let mut m = HashMap::new();
        m.insert("SEED_URL".into(), "scoped".into());
        let env = Env::new(vec![Arc::new(StaticEnvProvider::new(m))]);
        let scoped = env.scoped("seed");
        assert_eq!(resolve(&scoped, "url=${URL}").unwrap(), "url=scoped");
    }
}
