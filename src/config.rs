use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "abixio", about = "S3-compatible erasure-coded object store")]
pub struct Config {
    /// Listen address
    #[arg(long, default_value = ":10000")]
    pub listen: String,

    /// TLS certificate path for HTTPS listener
    #[arg(long)]
    pub tls_cert: Option<String>,

    /// TLS private key path for HTTPS listener
    #[arg(long)]
    pub tls_key: Option<String>,

    /// Volume paths (supports {N...M} range expansion)
    #[arg(long, value_delimiter = ',')]
    pub volumes: Vec<String>,

    /// Node endpoints for cluster mode (supports {N...M} range expansion)
    #[arg(long, value_delimiter = ',', default_value = "")]
    pub nodes: Vec<String>,

    /// Disable authentication
    #[arg(long)]
    pub no_auth: bool,

    /// Scanner loop interval (e.g. "10m", "1h")
    #[arg(long, default_value = "10m")]
    pub scan_interval: String,

    /// Minimum time between integrity checks per object (e.g. "24h")
    #[arg(long, default_value = "24h")]
    pub heal_interval: String,

    /// Number of MRF heal workers
    #[arg(long, default_value_t = 2)]
    pub mrf_workers: usize,

    /// Write tier for local volumes: `file` (default), `wal`
    /// (write-ahead log with background materialize), `log`
    /// (legacy log-structured store), or `pool` (legacy pre-opened
    /// temp file pool). See `docs/write-path.md`.
    #[arg(long, default_value = "file")]
    pub write_tier: String,

    /// RAM write cache size in MB. 0 disables the cache.
    #[arg(long, default_value_t = 256)]
    pub write_cache: u64,
}

impl Config {
    /// Expand {N...M} ranges in --volumes and --nodes, then validate.
    pub fn expand_and_validate(&mut self) -> Result<(), String> {
        // expand ranges
        self.volumes = self.volumes.iter().flat_map(|v| expand_ranges(v)).collect();
        self.nodes = self.nodes.iter().flat_map(|v| expand_ranges(v)).collect();
        // filter empty node entries (from default_value = "")
        self.nodes.retain(|s| !s.is_empty());

        if self.volumes.is_empty() {
            return Err("no volumes specified".to_string());
        }
        match (&self.tls_cert, &self.tls_key) {
            (Some(cert), Some(key)) => {
                if !PathBuf::from(cert).is_file() {
                    return Err(format!("tls cert path does not exist: {}", cert));
                }
                if !PathBuf::from(key).is_file() {
                    return Err(format!("tls key path does not exist: {}", key));
                }
            }
            (None, None) => {}
            _ => return Err("both --tls-cert and --tls-key must be provided together".to_string()),
        }
        for raw in &self.volumes {
            let path = PathBuf::from(raw);
            if !path.is_dir() {
                return Err(format!("volume path does not exist: {}", raw));
            }
        }
        // validate duration strings
        parse_duration(&self.scan_interval)
            .map_err(|_| format!("invalid scan_interval: {}", self.scan_interval))?;
        parse_duration(&self.heal_interval)
            .map_err(|_| format!("invalid heal_interval: {}", self.heal_interval))?;
        // validate write tier
        match self.write_tier.as_str() {
            "file" | "wal" | "log" | "pool" => {}
            other => return Err(format!("invalid write_tier: {} (expected file|wal|log|pool)", other)),
        }
        Ok(())
    }

    /// Return volume paths as PathBuf (call after expand_and_validate).
    pub fn volume_paths(&self) -> Vec<PathBuf> {
        self.volumes.iter().map(PathBuf::from).collect()
    }

    pub fn scan_interval_duration(&self) -> Duration {
        parse_duration(&self.scan_interval).unwrap_or(Duration::from_secs(600))
    }

    pub fn heal_interval_duration(&self) -> Duration {
        parse_duration(&self.heal_interval).unwrap_or(Duration::from_secs(86400))
    }
}

/// Parse a simple duration string like "10m", "24h", "30s", "1h30m".
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("invalid duration".to_string());
    }
    let mut total_secs = 0u64;
    let mut num_buf = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num_buf.push(c);
        } else {
            let n: u64 = num_buf.parse().map_err(|_| "invalid number".to_string())?;
            num_buf.clear();
            match c {
                's' => total_secs += n,
                'm' => total_secs += n * 60,
                'h' => total_secs += n * 3600,
                'd' => total_secs += n * 86400,
                _ => return Err("invalid duration".to_string()),
            }
        }
    }
    if !num_buf.is_empty() {
        // trailing number without unit = seconds
        let n: u64 = num_buf.parse().map_err(|_| "invalid number".to_string())?;
        total_secs += n;
    }
    if total_secs == 0 {
        return Err("invalid duration".to_string());
    }
    Ok(Duration::from_secs(total_secs))
}

/// Expand {N...M} range patterns in a string.
/// Returns a vec of expanded strings. Multiple ranges produce all combinations.
/// No braces -> returns the input unchanged.
pub fn expand_ranges(s: &str) -> Vec<String> {
    // find first {N...M} pattern
    let Some(open) = s.find('{') else {
        return vec![s.to_string()];
    };
    let Some(close) = s[open..].find('}') else {
        return vec![s.to_string()];
    };
    let close = open + close;
    let inner = &s[open + 1..close];

    // parse N...M
    let Some((start_s, end_s)) = inner.split_once("...") else {
        return vec![s.to_string()];
    };
    let prefix = &s[..open];
    let suffix = &s[close + 1..];

    // try numeric range first
    if let (Ok(start), Ok(end)) = (start_s.parse::<i64>(), end_s.parse::<i64>()) {
        let mut results = Vec::new();
        let step: i64 = if start <= end { 1 } else { -1 };
        let mut i = start;
        loop {
            let expanded = format!("{}{}{}", prefix, i, suffix);
            // recursively expand remaining ranges in suffix
            results.extend(expand_ranges(&expanded));
            if i == end {
                break;
            }
            i += step;
        }
        return results;
    }

    // try single-char alpha range (a...z)
    if start_s.len() == 1 && end_s.len() == 1 {
        let start_c = start_s.as_bytes()[0];
        let end_c = end_s.as_bytes()[0];
        if start_c.is_ascii_alphabetic() && end_c.is_ascii_alphabetic() {
            let mut results = Vec::new();
            let (lo, hi) = if start_c <= end_c {
                (start_c, end_c)
            } else {
                (end_c, start_c)
            };
            for c in lo..=hi {
                let expanded = format!("{}{}{}", prefix, c as char, suffix);
                results.extend(expand_ranges(&expanded));
            }
            return results;
        }
    }

    vec![s.to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_dirs(n: usize) -> (TempDir, Vec<PathBuf>) {
        let base = TempDir::new().unwrap();
        let mut paths = Vec::new();
        for i in 0..n {
            let p = base.path().join(format!("d{}", i));
            std::fs::create_dir_all(&p).unwrap();
            paths.push(p);
        }
        (base, paths)
    }

    fn config_with(volumes: Vec<String>) -> Config {
        Config {
            listen: ":10000".to_string(),
            tls_cert: None,
            tls_key: None,
            nodes: Vec::new(),
            volumes,
            no_auth: false,
            scan_interval: "10m".to_string(),
            heal_interval: "24h".to_string(),
            mrf_workers: 2,
            write_tier: "file".to_string(),
            write_cache: 256,
        }
    }

    #[test]
    fn valid_single_volume() {
        let (_base, paths) = make_dirs(1);
        let vols: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        let mut cfg = config_with(vols);
        assert!(cfg.expand_and_validate().is_ok());
    }

    #[test]
    fn valid_multiple_volumes() {
        let (_base, paths) = make_dirs(4);
        let vols: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        let mut cfg = config_with(vols);
        assert!(cfg.expand_and_validate().is_ok());
    }

    #[test]
    fn invalid_no_volumes() {
        let mut cfg = config_with(vec![]);
        assert!(cfg.expand_and_validate().is_err());
    }

    #[test]
    fn invalid_volume_path_missing() {
        let mut cfg = config_with(vec!["/tmp/nonexistent_abixio_test".to_string()]);
        assert!(cfg.expand_and_validate().is_err());
    }

    #[test]
    fn defaults() {
        let (_base, paths) = make_dirs(2);
        let vols: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        let cfg = config_with(vols);
        assert_eq!(cfg.listen, ":10000");
        assert_eq!(cfg.scan_interval_duration(), Duration::from_secs(600));
        assert_eq!(cfg.heal_interval_duration(), Duration::from_secs(86400));
        assert_eq!(cfg.mrf_workers, 2);
    }

    #[test]
    fn expand_ranges_numeric() {
        assert_eq!(
            expand_ranges("/data{1...4}"),
            vec!["/data1", "/data2", "/data3", "/data4"]
        );
    }

    #[test]
    fn expand_ranges_node_url() {
        assert_eq!(
            expand_ranges("http://node{1...3}:10000"),
            vec![
                "http://node1:10000",
                "http://node2:10000",
                "http://node3:10000"
            ]
        );
    }

    #[test]
    fn expand_ranges_nested() {
        let result = expand_ranges("rack{1...2}-node{1...2}");
        assert_eq!(
            result,
            vec!["rack1-node1", "rack1-node2", "rack2-node1", "rack2-node2"]
        );
    }

    #[test]
    fn expand_ranges_no_braces() {
        assert_eq!(expand_ranges("/data1"), vec!["/data1"]);
        assert_eq!(
            expand_ranges("http://node:10000"),
            vec!["http://node:10000"]
        );
    }

    #[test]
    fn expand_ranges_single_value() {
        assert_eq!(expand_ranges("/data{3...3}"), vec!["/data3"]);
    }

    #[test]
    fn expand_and_validate_expands_volumes() {
        let (_base, paths) = make_dirs(3);
        let base_path = paths[0].parent().unwrap().display().to_string();
        let pattern = format!("{}/d{{0...2}}", base_path);
        let mut cfg = config_with(vec![pattern]);
        assert!(cfg.expand_and_validate().is_ok());
        assert_eq!(cfg.volumes.len(), 3);
    }

    #[test]
    fn parse_duration_works() {
        assert_eq!(parse_duration("10m"), Ok(Duration::from_secs(600)));
        assert_eq!(parse_duration("24h"), Ok(Duration::from_secs(86400)));
        assert_eq!(parse_duration("30s"), Ok(Duration::from_secs(30)));
        assert_eq!(parse_duration("1h30m"), Ok(Duration::from_secs(5400)));
        assert!(parse_duration("").is_err());
        assert!(parse_duration("abc").is_err());
    }
}
