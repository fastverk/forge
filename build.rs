// Compiles every forge.v1 proto this module owns.
//
// events.proto and discovery.proto were VENDORED, byte-identical, into
// plugin-tbzl, plugin-forge and geetch — each carrying its own copy because this
// module did not offer them. That is the same drift hazard hard rule 3 exists to
// prevent for forge.proto, just without anything enforcing it: a copy is faithful
// only until someone edits one. They are `package forge.v1` and describe the same
// contract as the rest of this directory, so their home was always here.
//
// events.proto imports discovery.proto; both compile under the one `proto`
// include root, and neither collides with a forge.proto or provision.proto name.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(
            &[
                "proto/forge/v1/forge.proto",
                "proto/forge/v1/provision.proto",
                "proto/forge/v1/discovery.proto",
                "proto/forge/v1/events.proto",
            ],
            &["proto"],
        )?;
    Ok(())
}
