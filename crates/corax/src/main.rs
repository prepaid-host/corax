//! `corax` — a Nagios-compatible, high-performance monitoring core.
//!
//! The command line is compatible with the way OMD/Checkmk invokes Nagios:
//!
//! ```text
//! corax -pv /omd/sites/x/tmp/nagios/nagios.cfg     # verify configuration
//! corax -ud /omd/sites/x/tmp/nagios/nagios.cfg     # start as daemon
//! corax /omd/sites/x/tmp/nagios/nagios.cfg         # run in the foreground
//! ```

mod cli;
mod daemon;

use std::process::ExitCode;

/// musl's allocator serializes on a global lock and is several times slower than glibc's;
/// large Livestatus answers built on concurrent threads made that the bottleneck of the
/// static binary.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> ExitCode {
    let args = match cli::Args::parse(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{msg}\n\n{}", cli::USAGE);
            return ExitCode::from(3);
        }
    };
    if args.help {
        println!("{}", cli::USAGE);
        return ExitCode::SUCCESS;
    }
    if args.version {
        println!("corax {} (Nagios 3 compatible)", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let Some(config) = args.config.clone() else {
        eprintln!("{}", cli::USAGE);
        return ExitCode::from(3);
    };
    if args.verify {
        return daemon::verify(&config);
    }
    daemon::run(&config, &args)
}
