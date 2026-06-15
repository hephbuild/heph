#[allow(warnings)]
mod bindings;

use bindings::Guest;

struct Component;

impl Guest for Component {
    fn greet(name: String) -> String {
        // Call back into the host, then fold the host's answer into our reply.
        // Proves the guest→host callback path over wasm.
        let from_host = bindings::host_lookup(&name);
        format!("hello {name} ({from_host})")
    }
}

bindings::export!(Component with_types_in bindings);
