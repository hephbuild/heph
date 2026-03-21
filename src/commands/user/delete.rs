use clap::Args;

#[derive(Args)]
pub struct DeleteArgs {
    /// Username of the user to delete
    pub username: String,
}

pub fn execute(args: &DeleteArgs) {
    println!("User '{}' deleted!", args.username);
}
