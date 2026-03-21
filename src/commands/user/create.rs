use clap::Args;

#[derive(Args)]
pub struct CreateArgs {
    /// Username for the new user
    pub username: String,
}

pub fn execute(args: &CreateArgs) {
    println!("User '{}' created!", args.username);
}
