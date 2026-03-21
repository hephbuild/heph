use clap::Args;

#[derive(Args)]
pub struct AddArgs {
    /// First number
    pub a: i32,
    /// Second number
    pub b: i32,
}

pub fn execute(args: &AddArgs) {
    println!("Result: {}", args.a + args.b);
}
