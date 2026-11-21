//! `parts`: the device kinds a board file can use, or one kind's keys.
use crate::cli::PartsArgs;

pub fn run(args: &PartsArgs) -> Result<(), String> {
    let registry = pcb_devices::registry();
    match &args.kind {
        Some(name) => {
            let kind = registry.get(name).ok_or_else(|| {
                let names: Vec<_> = registry.kinds().map(|k| k.name).collect();
                format!("no device kind {name:?} (there are {})", names.join(", "))
            })?;
            print!("{}", pcb::board::spec::describe(kind));
        }
        None => {
            let width = registry.kinds().map(|k| k.name.len()).max().unwrap_or(0);
            for k in registry.kinds() {
                println!("{:width$}  {}", k.name, k.summary);
            }
            println!(
                "\n`tartelette parts KIND` describes one; a part is `[parts.NAME]` with `kind = \"KIND\"` and its keys."
            );
        }
    }
    Ok(())
}
