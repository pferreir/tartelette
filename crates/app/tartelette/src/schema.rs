//! `schema`: the JSON schema of board files, kept in `boards/board.schema.json`.

/// The JSON schema of board files, for editors.
pub fn json() -> String {
    pcb::board::spec::json_schema(&pcb_devices::registry())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_committed_schema_is_up_to_date() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../boards/board.schema.json"
        );
        let committed = std::fs::read_to_string(path).unwrap_or_default();
        assert!(
            committed == json(),
            "boards/board.schema.json is stale: run `cargo run -p tartelette -- schema > boards/board.schema.json`"
        );
    }
}
