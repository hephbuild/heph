use rheph_proto_gen::rheph::v1::User;
use clap::Args;

#[derive(Args)]
pub struct CreateArgs {
    /// ID for the new user
    pub id: String,

    /// Username for the new user
    pub username: String,

    /// Email for the new user
    pub email: String,
}

pub fn execute(args: &CreateArgs) {
    let user = User {
        id: args.id.clone(),
        username: args.username.clone(),
        email: args.email.clone(),
    };

    let json = serde_json::to_string_pretty(&user).unwrap();
    println!("User created (JSON):\n{}", json);
}
