use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const BINARIES: &[&str] = &[
    "narsor",
    "narsor-cli",
    "narsor-server",
    "narsor-controller",
    "narsor-runner",
];
const TARGET: &str = "x86_64-unknown-linux-musl";
const ARCHIVE: &str = "narsor-linux-x86_64-musl.tar.gz";

struct Fixture {
    root: PathBuf,
    tools: PathBuf,
    output: PathBuf,
    target_dir: PathBuf,
    path: String,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "narsor-release-artifacts-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let tools = root.join("tools");
        let output = root.join("output");
        let target_dir = root.join("target");
        fs::create_dir_all(&tools).expect("create fake tool directory");

        let path = std::env::var("PATH").unwrap_or_default();
        let fixture = Self {
            root,
            tools,
            output,
            target_dir,
            path,
        };
        fixture.write_tool(
            "cargo",
            r#"#!/bin/sh
set -eu
release_dir="$CARGO_TARGET_DIR/x86_64-unknown-linux-musl/release"
mkdir -p "$release_dir"
for binary in narsor narsor-cli narsor-server narsor-controller narsor-runner; do
    [ "${OMIT_BINARY:-}" != "$binary" ] || continue
    printf '#!/bin/sh\nexit 0\n' > "$release_dir/$binary"
    chmod +x "$release_dir/$binary"
done
"#,
        );
        fixture.write_tool(
            "readelf",
            r#"#!/bin/sh
set -eu
case "$1" in
    -h) echo "ELF Header";;
    -l)
        echo "Program Headers"
        [ "${DYNAMIC_BINARY:-}" != "$(basename "$2")" ] || echo "INTERP"
        ;;
    -d)
        [ "${DYNAMIC_BINARY:-}" != "$(basename "$2")" ] || echo "(NEEDED)"
        ;;
    *) exit 2;;
esac
"#,
        );
        fixture.write_tool("musl-gcc", "#!/bin/sh\nexit 0\n");
        fixture
    }

    fn write_tool(&self, name: &str, body: &str) {
        let path = self.tools.join(name);
        fs::write(&path, body).expect("write fake tool");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .expect("make fake tool executable");
    }

    fn remove_tool(&self, name: &str) {
        fs::remove_file(self.tools.join(name)).expect("remove fake tool");
    }

    fn run(&self, extra_env: &[(&str, &str)]) -> Output {
        let mut path_entries = vec![self.tools.clone()];
        path_entries.extend(std::env::split_paths(&self.path));
        let path = std::env::join_paths(path_entries).expect("join PATH");
        let mut command = Command::new("bash");
        command
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("dist/package-linux.sh"))
            .env("PATH", path)
            .env("CARGO_TARGET_DIR", &self.target_dir)
            .env("NARSOR_DIST_DIR", &self.output);
        for (key, value) in extra_env {
            command.env(key, value);
        }
        command.output().expect("run packaging script")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

static NEXT_FIXTURE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[test]
fn packages_all_static_binaries_with_target_manifest_and_checksum() {
    let fixture = Fixture::new();
    let result = fixture.run(&[]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );

    let archive = fixture.output.join(ARCHIVE);
    assert!(archive.is_file());
    let entries = Command::new("tar")
        .args(["-tzf", archive.to_str().expect("archive path")])
        .output()
        .expect("list release archive");
    assert!(entries.status.success());
    let entries = String::from_utf8_lossy(&entries.stdout);
    for binary in BINARIES {
        assert!(entries.lines().any(|entry| entry == *binary), "{entries}");
    }
    assert!(entries.lines().any(|entry| entry == "manifest.json"));

    let manifest = Command::new("tar")
        .args([
            "-xOf",
            archive.to_str().expect("archive path"),
            "manifest.json",
        ])
        .output()
        .expect("read release manifest");
    let manifest: serde_json::Value =
        serde_json::from_slice(&manifest.stdout).expect("parse release manifest");
    assert_eq!(manifest["target"], TARGET);
    let manifest_binaries = manifest["binaries"].as_array().expect("manifest binaries");
    for binary in BINARIES {
        assert!(manifest_binaries.iter().any(|name| name == binary));
    }

    let checksum = fixture.output.join(format!("{ARCHIVE}.sha256"));
    assert!(checksum.is_file());
    let verified = Command::new("sha256sum")
        .args(["--check", checksum.to_str().expect("checksum path")])
        .current_dir(&fixture.output)
        .output()
        .expect("verify archive checksum");
    assert!(
        verified.status.success(),
        "{}",
        String::from_utf8_lossy(&verified.stdout)
    );
}

#[test]
fn does_not_create_archive_when_a_required_binary_is_missing() {
    let fixture = Fixture::new();
    let result = fixture.run(&[("OMIT_BINARY", "narsor-runner")]);
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("narsor-runner"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!fixture.output.join(ARCHIVE).exists());
}

#[test]
fn rejects_a_dynamically_linked_binary_before_packaging() {
    let fixture = Fixture::new();
    let result = fixture.run(&[("DYNAMIC_BINARY", "narsor-server")]);
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("narsor-server"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!fixture.output.join(ARCHIVE).exists());
}

#[test]
fn reports_the_missing_musl_compiler_before_building() {
    let fixture = Fixture::new();
    fixture.remove_tool("musl-gcc");

    let result = fixture.run(&[]);

    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("musl-gcc is required"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!fixture.output.join(ARCHIVE).exists());
}
