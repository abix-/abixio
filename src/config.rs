use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "abixio", about = "S3-compatible erasure-coded object store")]
pub struct Config {
    /// Listen address
    #[arg(long, default_value = ":10000")]
    pub listen: String,

    /// Comma-separated disk paths
    #[arg(long, value_delimiter = ',')]
    pub disks: Vec<PathBuf>,

    /// Number of data shards
    #[arg(long)]
    pub data: usize,

    /// Number of parity shards
    #[arg(long, default_value_t = 0)]
    pub parity: usize,

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
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        if self.data == 0 {
            return Err("data shards must be >= 1".to_string());
        }
        if self.disks.is_empty() {
            return Err("no disks specified".to_string());
        }
        let total = self.data + self.parity;
        if self.disks.len() < total {
            return Err(format!(
                "need at least {} disks (data={} + parity={}), got {}",
                total,
                self.data,
                self.parity,
                self.disks.len()
            ));
        }
        for path in &self.disks {
            if !path.is_dir() {
                return Err(format!("disk path does not exist: {}", path.display()));
            }
        }
        // validate duration strings
        parse_duration(&self.scan_interval)
            .map_err(|_| format!("invalid scan_interval: {}", self.scan_interval))?;
        parse_duration(&self.heal_interval)
            .map_err(|_| format!("invalid heal_interval: {}", self.heal_interval))?;
        Ok(())
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

    fn config_with(disks: Vec<PathBuf>, data: usize, parity: usize) -> Config {
        Config {
            listen: ":9000".to_string(),
            disks,
            data,
            parity,
            no_auth: false,
            scan_interval: "10m".to_string(),
            heal_interval: "24h".to_string(),
            mrf_workers: 2,
        }
    }

    #[test]
    fn valid_4_disks_2_2() {
        let (_base, paths) = make_dirs(4);
        let cfg = config_with(paths, 2, 2);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn valid_1_disk_1_0() {
        let (_base, paths) = make_dirs(1);
        let cfg = config_with(paths, 1, 0);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn valid_2_disks_1_1() {
        let (_base, paths) = make_dirs(2);
        let cfg = config_with(paths, 1, 1);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn valid_3_disks_2_1() {
        let (_base, paths) = make_dirs(3);
        let cfg = config_with(paths, 2, 1);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn invalid_data_zero() {
        let (_base, paths) = make_dirs(1);
        let cfg = config_with(paths, 0, 1);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn invalid_too_few_disks() {
        let (_base, paths) = make_dirs(3);
        let cfg = config_with(paths, 2, 2); // needs 4, got 3
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn valid_more_disks_than_needed() {
        let (_base, paths) = make_dirs(6);
        let cfg = config_with(paths, 2, 2); // pool of 6, default EC 2+2
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn invalid_no_disks() {
        let cfg = config_with(vec![], 1, 0);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn invalid_disk_path_missing() {
        let cfg = config_with(vec![PathBuf::from("/tmp/nonexistent_abixio_test")], 1, 0);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn defaults() {
        let (_base, paths) = make_dirs(2);
        let cfg = config_with(paths, 1, 1);
        assert_eq!(cfg.listen, ":9000");
        assert_eq!(cfg.scan_interval_duration(), Duration::from_secs(600));
        assert_eq!(cfg.heal_interval_duration(), Duration::from_secs(86400));
        assert_eq!(cfg.mrf_workers, 2);
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
