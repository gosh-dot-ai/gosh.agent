use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let prompt_dir = manifest_dir.join("src").join("agent").join("prompts");
    let execution_prompt_path = prompt_dir.join("execution_system.txt");
    let review_prompt_path = prompt_dir.join("review.txt");
    let review_repair_prompt_path = prompt_dir.join("review_repair.txt");

    println!("cargo:rerun-if-changed={}", execution_prompt_path.display());
    println!("cargo:rerun-if-changed={}", review_prompt_path.display());
    println!("cargo:rerun-if-changed={}", review_repair_prompt_path.display());

    let execution_prompt =
        fs::read_to_string(&execution_prompt_path).expect("read execution_system.txt");
    let review_prompt = fs::read_to_string(&review_prompt_path).expect("read review.txt");
    let review_repair_prompt =
        fs::read_to_string(&review_repair_prompt_path).expect("read review_repair.txt");

    let generated = format!(
        "pub fn prompt_asset(name: &str) -> Option<&'static str> {{
    match name {{
        \"execution_system.txt\" => Some({execution_prompt:?}),
        \"review.txt\" => Some({review_prompt:?}),
        \"review_repair.txt\" => Some({review_repair_prompt:?}),
        _ => None,
    }}
}}
"
    );

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let output_path = Path::new(&out_dir).join("prompt_assets.rs");
    fs::write(output_path, generated).expect("write prompt_assets.rs");
}
