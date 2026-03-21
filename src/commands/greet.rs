use clap::Args;

#[derive(Args)]
pub struct GreetArgs {
    /// Name of the person to greet
    #[arg(short, long)]
    pub name: String,
}

pub fn execute(args: &GreetArgs) {
    println!("Hello, {}!", args.name);
}
