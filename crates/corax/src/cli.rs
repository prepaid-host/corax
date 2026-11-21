//! Nagios compatible argument parsing (`-pv`, `-ud`, …) plus corax extensions.

use std::path::PathBuf;

pub const USAGE: &str = "\
Usage: corax [options] <main_config_file>

Nagios compatible options:
  -v, --verify-config     Verify the configuration and exit
  -d, --daemon            Run as a daemon (fork into the background)
  -p, -u, -s              Accepted for compatibility (object precaching is not needed)
  -V, --version           Print the version

corax options:
      --no-check-helper           Do not use the pre-forking Python helper for Checkmk host checks
      --max-concurrent-checks N   Limit the number of concurrently running checks
      --log-level LEVEL           error, warn, info (default), debug, trace
      --diag-log FILE             Diagnostic log file in daemon mode (default: corax.log next to log_file)
      --query FILE                Answer the Livestatus queries in FILE (separated by blank lines,
                                  `-` reads standard input) from the configuration, the retention
                                  file and the log files, without starting the core
  -h, --help                      Show this help

Environment: CORAX_CHECK_HELPER=0 disables the check helper, RUST_LOG overrides --log-level.";

#[derive(Debug, Default, Clone)]
pub struct Args {
    pub verify: bool,
    pub daemon: bool,
    pub help: bool,
    pub version: bool,
    pub check_helper: bool,
    pub max_concurrent_checks: usize,
    pub log_level: String,
    pub diag_log: Option<PathBuf>,
    pub query: Option<PathBuf>,
    pub config: Option<PathBuf>,
}

impl Args {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut a = Args {
            check_helper: std::env::var("CORAX_CHECK_HELPER").map(|v| v != "0").unwrap_or(true),
            log_level: "info".into(),
            ..Default::default()
        };
        let mut it = args.into_iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--verify-config" => a.verify = true,
                "--daemon" => a.daemon = true,
                "--help" => a.help = true,
                "--version" => a.version = true,
                "--no-check-helper" => a.check_helper = false,
                "--max-concurrent-checks" => {
                    let v = it.next().ok_or("--max-concurrent-checks needs a value")?;
                    a.max_concurrent_checks = v.parse().map_err(|_| format!("invalid number '{v}'"))?;
                }
                "--log-level" => a.log_level = it.next().ok_or("--log-level needs a value")?,
                "--diag-log" => a.diag_log = Some(it.next().ok_or("--diag-log needs a value")?.into()),
                "--query" => a.query = Some(it.next().ok_or("--query needs a value")?.into()),
                s if s.starts_with("--") => return Err(format!("unknown option '{s}'")),
                s if s.starts_with('-') && s.len() > 1 => {
                    for c in s[1..].chars() {
                        match c {
                            'v' => a.verify = true,
                            'd' => a.daemon = true,
                            'h' => a.help = true,
                            'V' => a.version = true,
                            'p' | 'u' | 's' | 'x' => {}
                            other => return Err(format!("unknown option '-{other}'")),
                        }
                    }
                }
                _ => {
                    if a.config.is_some() {
                        return Err("only one configuration file may be given".into());
                    }
                    a.config = Some(PathBuf::from(arg));
                }
            }
        }
        Ok(a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(v: &[&str]) -> Args {
        Args::parse(v.iter().map(|s| s.to_string())).unwrap()
    }

    #[test]
    fn nagios_style_flags() {
        let a = p(&["-pv", "/x/nagios.cfg"]);
        assert!(a.verify && !a.daemon);
        assert_eq!(a.config.unwrap().to_str().unwrap(), "/x/nagios.cfg");
        let a = p(&["-ud", "/x/nagios.cfg"]);
        assert!(a.daemon && !a.verify);
        assert!(Args::parse(["-z".to_string()]).is_err());
        assert_eq!(p(&["--max-concurrent-checks", "8", "c"]).max_concurrent_checks, 8);
        assert_eq!(p(&["--query", "-", "c"]).query.unwrap().to_str(), Some("-"));
        assert!(Args::parse(["--query".to_string()]).is_err());
    }
}
