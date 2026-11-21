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
    if let Some(queries) = &args.query {
        return daemon::query(&config, queries);
    }
    daemon::run(&config, &args)
}
