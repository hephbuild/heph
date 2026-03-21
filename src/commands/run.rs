use clap::Args;
use crate::htaddr::parse::parse_taddr;

#[derive(Args)]
#[command(override_usage = "run <TARGET_ADDRESS>\n       run <LABEL> <PACKAGE_MATCHER>")]
pub struct RunArgs {
    /// Target address (e.g., //pkg:name) OR Label
    #[arg(value_name = "TARGET_ADDRESS/LABEL")]
    pub arg1: String,
    /// Package matcher (only if first argument is a Label)
    #[arg(value_name = "PACKAGE_MATCHER")]
    pub arg2: Option<String>,
}

pub fn execute(args: &RunArgs) {
    if let Some(matcher) = &args.arg2 {
        let label = &args.arg1;
        println!("Run with label: {}, package matcher: {}", label, matcher);
    } else {
        let address = &args.arg1;
        match parse_taddr(address) {
            Ok(addr) => println!("Run with target address: {:?}", addr),
            Err(e) => {
                eprintln!("Error: '{}' is not a valid target address: {}", address, e);
            }
        }
    }
}
