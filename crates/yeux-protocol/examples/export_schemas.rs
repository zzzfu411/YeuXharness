use std::{collections::BTreeSet, env, fs, path::PathBuf};

use yeux_protocol::stable_schema_bundle;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut check = false;
    let mut output = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("spec/schema");
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--check" => check = true,
            "--output" => {
                output = arguments
                    .next()
                    .ok_or("--output requires a directory")?
                    .into();
            }
            _ => return Err(format!("unknown argument: {argument}").into()),
        }
    }

    let documents = stable_schema_bundle()
        .into_iter()
        .map(|(name, schema)| {
            let mut json = serde_json::to_string_pretty(&schema)?;
            json.push('\n');
            Ok((format!("{name}.schema.json"), json))
        })
        .collect::<Result<Vec<_>, serde_json::Error>>()?;

    if check {
        let expected_names: BTreeSet<_> = documents.iter().map(|(name, _)| name.clone()).collect();
        let actual_names: BTreeSet<_> = fs::read_dir(&output)?
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| name.ends_with(".schema.json"))
            .collect();
        if actual_names != expected_names {
            return Err(format!(
                "schema file set is stale: expected {expected_names:?}, found {actual_names:?}"
            )
            .into());
        }
        for (name, expected) in documents {
            let path = output.join(name);
            let actual = fs::read_to_string(&path)?;
            if actual != expected {
                return Err(format!("schema is stale: {}", path.display()).into());
            }
        }
        return Ok(());
    }

    fs::create_dir_all(&output)?;
    for (name, document) in documents {
        fs::write(output.join(name), document)?;
    }
    Ok(())
}
