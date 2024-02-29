use std::{path::Path, process::Command};

pub fn verify(temp_dir: &Path, name: &str, constants_name: Option<&str>) -> Result<(), String> {
    let pilcom = std::env::var("PILCOM")
        .expect("Please set the PILCOM environment variable to the path to the pilcom repository.");

    let constants_name = constants_name.unwrap_or(name);

    let constants_file = format!(
        "{}/{constants_name}_constants.bin",
        temp_dir.to_str().unwrap()
    );
    let commits_file = format!("{}/{name}_commits.bin", temp_dir.to_str().unwrap());
    let constraints_file = format!("{}/constraints.json", temp_dir.to_str().unwrap());

    let verifier_output = Command::new("node")
        .args([
            "--max-old-space-size=8000".to_string(), // 8GB of memory
            format!("{pilcom}/src/main_pilverifier.js"),
            commits_file,
            "-j".to_string(),
            constraints_file,
            "-c".to_string(),
            constants_file,
        ])
        .output()
        .expect("failed to run pil verifier");
    if !verifier_output.status.success() {
        log::error!(
            "Pil verifier run was unsuccessful.\nStdout: {}\nStderr: {}\n",
            String::from_utf8_lossy(&verifier_output.stdout),
            String::from_utf8_lossy(&verifier_output.stderr)
        );
        return Err("Pil verifier run was unsuccessful.".to_string());
    } else {
        let output = String::from_utf8(verifier_output.stdout).unwrap();
        log::error!("PIL verifier output: {}", output);
        if !output.trim().ends_with("PIL OK!!") {
            return Err("Verified did not say 'PIL OK' for {name}.".to_string());
        }
    }
    Ok(())
}
