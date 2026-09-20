use std::{env, fs, path::Path};

fn collect(root: &Path, path: &Path, output: &mut String) {
    let mut entries: Vec<_> = fs::read_dir(path).unwrap().map(|e| e.unwrap()).collect();
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        let path = entry.path();
        assert!(
            !entry.file_type().unwrap().is_symlink(),
            "UI assets must not be symlinks"
        );
        if path.is_dir() {
            collect(root, &path, output);
        } else {
            let name = path
                .strip_prefix(root)
                .unwrap()
                .to_str()
                .unwrap()
                .replace('\\', "/");
            output.push_str(&format!(
                "({name:?}, include_bytes!({:?})),\n",
                path.to_str().unwrap()
            ));
        }
    }
}

fn main() {
    println!("cargo:rerun-if-changed=frontend/dist");
    let root = Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()).join("frontend/dist");
    let mut output = String::from("pub static ASSETS: &[(&str, &[u8])] = &[\n");
    if root.join("index.html").is_file() {
        collect(&root, &root, &mut output);
    } else {
        assert!(
            env::var("PROFILE").unwrap() != "release",
            "Build the UI first with npm ci && npm run build -w frontend (or use npm run build)"
        );
        println!(
            "cargo:warning=UI not built; use npm run build to create the standalone application"
        );
    }
    output.push_str("];\n");
    fs::write(
        Path::new(&env::var("OUT_DIR").unwrap()).join("ui_assets.rs"),
        output,
    )
    .unwrap();
}
